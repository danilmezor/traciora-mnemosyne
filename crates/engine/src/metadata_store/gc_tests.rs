//! Authored source-directed timer/failure controls, separate from native60s proof.
use super::*;
use crate::Engine;
use std::{
    fs,
    time::{Duration, Instant},
};

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        loop {
            let path = std::env::temp_dir().join(format!(
                "mn-gc-unit-{}-{}",
                std::process::id(),
                NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("cannot create GC unit fixture {}: {error}", path.display()),
            }
        }
    }
    fn persisted(&self) -> serde_json::Value {
        serde_json::from_slice(&fs::read(self.0.join(".mnemosyne/metadata-cache.json")).unwrap())
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn hash(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}
fn arm(engine: &mut Engine, start: Instant) {
    // Unit-only injected clock: use actual arming and public polling paths.
    engine.metadata_store.arm_periodic_gc(start);
    engine.metadata_store.arm_startup_gc(start);
}
fn modify(engine: &mut Engine, path: &str, text: &str) {
    engine.vault.modify(path, text).unwrap();
    engine.file_modified(path);
}
fn has_hash(engine: &Engine, fixture: &Fixture, hash: &str, expected: bool) {
    assert_eq!(
        engine.metadata_store.metadata.contains_key(hash),
        expected,
        "live hash {hash}"
    );
    assert_eq!(
        fixture.persisted()["metadata"].get(hash).is_some(),
        expected,
        "disk hash {hash}"
    );
}

#[test]
fn exact_startup_boundary_collects_orphan_but_keeps_shared_reference_and_revert_is_fresh() {
    let fixture = Fixture::new();
    for (path, content) in [
        ("A.md", "# Shared\n"),
        ("B.md", "# Shared\n"),
        ("Old.md", "# Old\n"),
    ] {
        fs::write(fixture.0.join(path), content).unwrap();
    }
    drop(Engine::open(&fixture.0).unwrap());
    let mut engine = Engine::open(&fixture.0).unwrap();
    let start = Instant::now();
    arm(&mut engine, start);
    let old = hash("# Old\n");
    let shared = hash("# Shared\n");
    let changed = hash("# Changed\n");
    modify(&mut engine, "Old.md", "# Changed\n");
    engine.vault.delete("A.md").unwrap();
    engine.file_deleted("A.md");
    let before = serde_json::to_value(engine.file_cache("B.md").unwrap()).unwrap();
    let cursor = engine.start_event_journal();
    assert!(engine
        .poll_metadata_cache_gc_at(start + Duration::from_secs(60) - Duration::from_nanos(1))
        .unwrap()
        .is_empty());
    has_hash(&engine, &fixture, &old, true);
    assert_eq!(
        engine
            .poll_metadata_cache_gc_at(start + Duration::from_secs(60))
            .unwrap(),
        [old.clone()]
    );
    has_hash(&engine, &fixture, &old, false);
    has_hash(&engine, &fixture, &shared, true);
    has_hash(&engine, &fixture, &changed, true);
    assert_eq!(
        serde_json::to_value(engine.file_cache("B.md").unwrap()).unwrap(),
        before
    );
    assert!(engine.event_journal_since(cursor).unwrap().is_empty());
    modify(&mut engine, "Old.md", "# Old\n");
    let reverted = serde_json::to_value(engine.file_cache("Old.md").unwrap()).unwrap();
    assert!(
        reverted.get("v").is_none(),
        "collected content must be parsed fresh"
    );
    assert_eq!(reverted["headings"][0]["heading"], "Old");
    has_hash(&engine, &fixture, &old, true);
    assert_eq!(
        fs::read_to_string(fixture.0.join("Old.md")).unwrap(),
        "# Old\n"
    );
    assert_eq!(
        fs::read_to_string(fixture.0.join("B.md")).unwrap(),
        "# Shared\n"
    );
}

#[test]
fn periodic_deadline_is_independent_600_not_660_and_repeats_at_1200() {
    let fixture = Fixture::new();
    fs::write(fixture.0.join("A.md"), "# One\n").unwrap();
    let mut engine = Engine::open(&fixture.0).unwrap();
    let start = Instant::now();
    arm(&mut engine, start);
    modify(&mut engine, "A.md", "# Two longer\n");
    assert_eq!(
        engine
            .poll_metadata_cache_gc_at(start + Duration::from_secs(60))
            .unwrap(),
        [hash("# One\n")]
    );
    modify(&mut engine, "A.md", "# Three different\n");
    let cursor = engine.start_event_journal();
    assert!(engine
        .poll_metadata_cache_gc_at(start + Duration::from_secs(600) - Duration::from_nanos(1))
        .unwrap()
        .is_empty());
    has_hash(&engine, &fixture, &hash("# Two longer\n"), true);
    assert_eq!(
        engine
            .poll_metadata_cache_gc_at(start + Duration::from_secs(600))
            .unwrap(),
        [hash("# Two longer\n")]
    );
    has_hash(&engine, &fixture, &hash("# Two longer\n"), false);
    assert!(engine.event_journal_since(cursor).unwrap().is_empty());
    modify(&mut engine, "A.md", "# Four final\n");
    let cursor = engine.start_event_journal();
    assert!(engine
        .poll_metadata_cache_gc_at(start + Duration::from_secs(1200) - Duration::from_nanos(1))
        .unwrap()
        .is_empty());
    has_hash(&engine, &fixture, &hash("# Three different\n"), true);
    assert_eq!(
        engine
            .poll_metadata_cache_gc_at(start + Duration::from_secs(1200))
            .unwrap(),
        [hash("# Three different\n")]
    );
    has_hash(&engine, &fixture, &hash("# Three different\n"), false);
    has_hash(&engine, &fixture, &hash("# Four final\n"), true);
    assert!(engine.event_journal_since(cursor).unwrap().is_empty());
}

#[test]
fn raw_alias_protects_old_hash_until_that_exact_key_is_deleted() {
    let fixture = Fixture::new();
    fs::write(fixture.0.join("Alias.md"), "# Original\n").unwrap();
    let mut engine = Engine::open(&fixture.0).unwrap();
    let start = Instant::now();
    arm(&mut engine, start);
    fs::rename(fixture.0.join("Alias.md"), fixture.0.join("Moved.md")).unwrap();
    engine.vault = mnemosyne_core::Vault::open(&fixture.0).unwrap();
    engine.file_renamed_with_raw("Alias.md", "./Moved.md", "Moved.md");
    modify(&mut engine, "Moved.md", "# Changed canonical\n");
    assert_eq!(
        engine.file_cache("./Moved.md").unwrap().headings[0].heading,
        "Original"
    );
    let cursor = engine.start_event_journal();
    assert!(engine
        .poll_metadata_cache_gc_at(start + Duration::from_secs(60))
        .unwrap()
        .is_empty());
    has_hash(&engine, &fixture, &hash("# Original\n"), true);
    assert!(engine.event_journal_since(cursor).unwrap().is_empty());
    engine.file_deleted("./Moved.md");
    assert!(engine.file_cache("./Moved.md").is_none());
    assert!(fixture.0.join("Moved.md").is_file());
    let cursor = engine.start_event_journal();
    assert_eq!(
        engine
            .poll_metadata_cache_gc_at(start + Duration::from_secs(600))
            .unwrap(),
        [hash("# Original\n")]
    );
    has_hash(&engine, &fixture, &hash("# Original\n"), false);
    has_hash(&engine, &fixture, &hash("# Changed canonical\n"), true);
    assert!(engine.event_journal_since(cursor).unwrap().is_empty());
}

#[test]
fn failed_cleanup_checkpoint_reports_error_preserves_prior_bytes_and_retries_later() {
    let fixture = Fixture::new();
    fs::write(fixture.0.join("A.md"), "# Prior\n").unwrap();
    let mut engine = Engine::open(&fixture.0).unwrap();
    let start = Instant::now();
    arm(&mut engine, start);
    modify(&mut engine, "A.md", "# Current saved\n");
    let private = fixture.0.join(".mnemosyne");
    let backup = fixture.0.join(".prior-private");
    let previous = fs::read(private.join("metadata-cache.json")).unwrap();
    fs::rename(&private, &backup).unwrap();
    fs::write(&private, b"injected parent collision").unwrap();
    let cursor = engine.start_event_journal();
    assert!(engine
        .poll_metadata_cache_gc_at(start + Duration::from_secs(60))
        .is_err());
    assert!(engine
        .config_diagnostics()
        .iter()
        .any(|message| message.starts_with("Metadata cache cleanup:")));
    assert_eq!(
        fs::read(backup.join("metadata-cache.json")).unwrap(),
        previous
    );
    assert_eq!(fs::read(&private).unwrap(), b"injected parent collision");
    assert_eq!(
        fs::read_to_string(fixture.0.join("A.md")).unwrap(),
        "# Current saved\n"
    );
    assert!(
        engine.metadata_store.dirty,
        "failed checkpoint retains pending persistence"
    );
    assert!(engine.event_journal_since(cursor).unwrap().is_empty());
    fs::remove_file(&private).unwrap();
    fs::rename(&backup, &private).unwrap();
    assert!(engine
        .poll_metadata_cache_gc_at(start + Duration::from_secs(600))
        .unwrap()
        .is_empty());
    has_hash(&engine, &fixture, &hash("# Prior\n"), false);
    has_hash(&engine, &fixture, &hash("# Current saved\n"), true);
    assert!(engine.event_journal_since(cursor).unwrap().is_empty());
}
