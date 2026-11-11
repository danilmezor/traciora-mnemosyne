//! Native natural startup cleanup replay. Candidate clock advancement checks
//! the same before/after observations without claiming native callback latency.
use mnemosyne_engine::Engine;
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        loop {
            let path = std::env::temp_dir().join(format!(
                "mn-gc-{}-{}",
                std::process::id(),
                NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("cannot create GC fixture {}: {error}", path.display()),
            }
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn persisted(root: &Path) -> Value {
    serde_json::from_slice(&fs::read(root.join(".mnemosyne/metadata-cache.json")).unwrap()).unwrap()
}

#[test]
fn native_natural_startup_collection_keeps_shared_hash_and_revert_reparses_collected_content() {
    let evidence: Value = serde_json::from_str(include_str!("fixtures/cache_gc.json")).unwrap();
    assert_eq!(evidence["identity"]["reference_version"], "1.14.4");
    let fixture = Fixture::new();
    let mut engine = None;
    let mut opened_before = Instant::now();
    let mut opened_after = opened_before;
    for stage in evidence["steps"].as_array().unwrap() {
        let id = stage["id"].as_str().unwrap();
        let operation = &stage["operation"];
        match operation["type"].as_str().unwrap() {
            "create-initial-files" => {
                for (path, content) in evidence["initial_files"].as_object().unwrap() {
                    fs::write(fixture.0.join(path), content.as_str().unwrap()).unwrap();
                }
                opened_before = Instant::now();
                engine = Some(Engine::open(&fixture.0).unwrap());
                opened_after = Instant::now();
            }
            "restart" => {
                drop(engine.take());
                opened_before = Instant::now();
                engine = Some(Engine::open(&fixture.0).unwrap());
                opened_after = Instant::now();
            }
            "modify" => {
                let engine = engine.as_mut().unwrap();
                let path = operation["path"].as_str().unwrap();
                engine
                    .vault
                    .modify(path, operation["content"].as_str().unwrap())
                    .unwrap();
                engine.file_modified(path);
            }
            "delete" => {
                let engine = engine.as_mut().unwrap();
                let path = operation["path"].as_str().unwrap();
                engine.vault.delete(path).unwrap();
                engine.file_deleted(path);
                // Conservative external clock seam: before-open+59 is before
                // the armed 60s deadline; exact boundaries are unit controls.
                let cursor = engine.start_event_journal();
                assert!(engine
                    .poll_metadata_cache_gc_at(opened_before + Duration::from_secs(59))
                    .unwrap()
                    .is_empty());
                assert!(engine.event_journal_since(cursor).unwrap().is_empty());
            }
            "wait-for-cache-gc" => {
                let engine = engine.as_mut().unwrap();
                let cursor = engine.start_event_journal();
                let mut removed = engine
                    .poll_metadata_cache_gc_at(opened_after + Duration::from_secs(61))
                    .unwrap();
                removed.sort();
                let mut expected: Vec<String> =
                    serde_json::from_value(operation["hashes_removed"].clone()).unwrap();
                expected.sort();
                assert_eq!(removed, expected, "{id}: collected selected hash");
                assert!(
                    engine.event_journal_since(cursor).unwrap().is_empty(),
                    "GC must not fabricate vault mutations"
                );
            }
            kind => panic!("unknown native GC operation {kind}"),
        }
        let engine = engine.as_ref().unwrap();
        let mut caches = serde_json::Map::new();
        let mut bytes = serde_json::Map::new();
        for path in evidence["selected_paths"].as_array().unwrap() {
            let path = path.as_str().unwrap();
            caches.insert(
                path.into(),
                serde_json::to_value(engine.file_cache(path)).unwrap(),
            );
            let saved = match fs::read(fixture.0.join(path)) {
                Ok(bytes) => serde_json::to_value(bytes).unwrap(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Value::Null,
                Err(error) => panic!("{id}: cannot read {path}: {error}"),
            };
            bytes.insert(path.into(), saved);
        }
        assert_eq!(
            Value::Object(caches),
            stage["result"]["cache"],
            "{id}: full selected caches"
        );
        assert_eq!(
            Value::Object(bytes),
            stage["file_bytes"],
            "{id}: exact selected file bytes"
        );
        let stored = persisted(&fixture.0);
        for (hash, expected) in stage["result"]["hash_presence"].as_object().unwrap() {
            assert_eq!(
                stored["metadata"].get(hash).is_some(),
                expected.as_bool().unwrap(),
                "{id}: persisted content hash {hash}"
            );
        }
        // Native hash lists include its bootstrap note. Only independently
        // selected role hashes are compared; all selected keys remain exact.
    }
}
