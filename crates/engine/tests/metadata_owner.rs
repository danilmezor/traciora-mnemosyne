//! Authored owner-integrity controls, separate from captured native timelines.
use mnemosyne_engine::Engine;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        loop {
            let path = std::env::temp_dir().join(format!(
                "mn-owner-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("fixture: {error}"),
            }
        }
    }
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn changed_payload_uses_the_same_read_buffer_as_the_cache() {
    let root = Fixture::new();
    fs::write(root.path().join("A.md"), "# Original\n").unwrap();
    let mut engine = Engine::open(root.path()).unwrap();
    engine.enable_metadata_lifecycle().unwrap();
    engine.start_lifecycle_journal();
    engine.vault.modify("A.md", "# Queued\n").unwrap();
    engine.file_modified("A.md");
    // The public Vault snapshot remains queued while the saved read changes.
    let saved = "# Actual buffer captured by worker\n";
    fs::write(root.path().join("A.md"), saved).unwrap();
    engine.poll_metadata_lifecycle().unwrap();
    engine.poll_metadata_lifecycle().unwrap();
    let event = engine
        .lifecycle_journal_since(0)
        .unwrap()
        .iter()
        .find(|event| event.name == "changed")
        .unwrap();
    assert_eq!(event.data.as_deref(), Some(saved));
    assert_eq!(
        engine.file_cache("A.md").unwrap().headings[0].heading,
        "Actual buffer captured by worker"
    );
    assert_eq!(
        fs::read(root.path().join("A.md")).unwrap(),
        saved.as_bytes()
    );
}

#[test]
fn reload_preserves_retained_service_delivery() {
    let root = Fixture::new();
    fs::write(root.path().join("A.md"), "# Original\n").unwrap();
    let mut engine = Engine::open(root.path()).unwrap();
    engine.enable_retained_watch().unwrap();
    engine.reload().unwrap();
    assert!(engine.metadata_lifecycle_enabled());
    // Private collectors reset on reload; normal service activation survives.
    engine.start_lifecycle_journal();
    fs::write(root.path().join("A.md"), "# Changed after reload\n").unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        engine.poll_external_changes().unwrap();
        engine.poll_metadata_lifecycle().unwrap();
        if engine.file_cache("A.md").is_some_and(|cache| {
            cache
                .headings
                .first()
                .is_some_and(|heading| heading.heading == "Changed after reload")
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "retained watcher did not survive reload"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(engine
        .lifecycle_journal_since(0)
        .unwrap()
        .iter()
        .any(|event| event.name == "changed"));
}

#[test]
fn read_error_finishes_the_task_and_later_work_can_recover() {
    let root = Fixture::new();
    let path = root.path().join("A.md");
    fs::write(&path, "# Original\n").unwrap();
    let mut engine = Engine::open(root.path()).unwrap();
    engine.enable_metadata_lifecycle().unwrap();
    let cursor = engine.start_lifecycle_journal();
    engine.vault.modify("A.md", "# Queued change\n").unwrap();
    engine.file_modified("A.md");
    assert_eq!(engine.metadata_readiness().in_progress, 1);
    fs::remove_file(&path).unwrap();
    let now = Instant::now();
    engine.poll_metadata_lifecycle_at(now).unwrap();
    assert!(engine.metadata_readiness().is_cache_clean);
    engine
        .poll_metadata_lifecycle_at(now + Duration::from_millis(10))
        .unwrap();
    let names = engine
        .lifecycle_journal_since(cursor)
        .unwrap()
        .iter()
        .map(|event| event.qualified_name())
        .collect::<Vec<_>>();
    assert_eq!(names, ["vault:modify", "metadata:finished"]);
    assert_eq!(
        engine.file_cache("A.md").unwrap().headings[0].heading,
        "Original"
    );
    engine
        .vault
        .modify("A.md", "# Recovered saved buffer\n")
        .unwrap();
    engine.file_modified("A.md");
    engine.poll_metadata_lifecycle().unwrap();
    engine.poll_metadata_lifecycle().unwrap();
    assert!(engine.metadata_readiness().is_cache_clean);
    assert_eq!(
        engine.file_cache("A.md").unwrap().headings[0].heading,
        "Recovered saved buffer"
    );
}
