//! Authored lifecycle controls; captured native branches live in file_reference.
use mnemosyne_core::CONFIG_DIR;
use mnemosyne_engine::{file_operations::LinkUpdateDecision, Engine};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "mn-file-operation-controls-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::create_dir(root.join(CONFIG_DIR)).unwrap();
        fs::write(
            root.join(CONFIG_DIR).join("app.json"),
            b"{\"alwaysUpdateLinks\":false}",
        )
        .unwrap();
        fs::write(root.join("Target.md"), "target\n").unwrap();
        fs::write(root.join("Source.md"), "[[Target]]\n").unwrap();
        fs::write(root.join("Other.md"), "other\n").unwrap();
        Self(root)
    }
    fn engine(&self) -> Engine {
        let mut engine = Engine::open(&self.0).unwrap();
        engine.enable_metadata_lifecycle().unwrap();
        engine
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn clean(engine: &mut Engine) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !engine.metadata_readiness().is_cache_clean {
        assert!(
            Instant::now() < deadline,
            "metadata owner failed to become clean"
        );
        engine.poll_metadata_lifecycle().unwrap();
    }
}
fn pending(engine: &mut Engine) -> mnemosyne_engine::file_operations::FileOperationId {
    let id = engine.begin_file_rename("Target.md", "Renamed.md", "Renamed.md");
    engine.poll_file_operations();
    assert_eq!(engine.pending_link_update().unwrap().id, id);
    id
}
#[test]
fn abandoned_consumer_keeps_decision_and_serial_mutation_without_retaining_result() {
    let fixture = Fixture::new();
    let mut engine = fixture.engine();
    let first = pending(&mut engine);
    engine.abandon_file_operation_result(first);
    let second = engine.begin_file_rename("Other.md", "Second.md", "Second.md");
    engine.poll_file_operations();
    assert!(fixture.0.join("Other.md").exists());
    assert!(!fixture.0.join("Second.md").exists());
    assert_eq!(engine.pending_link_update().unwrap().id, first);
    engine
        .decide_link_update(first, LinkUpdateDecision::Once)
        .unwrap();
    engine.poll_file_operations();
    assert!(engine.take_file_operation_result(first).is_none());
    assert_eq!(
        fs::read_to_string(fixture.0.join("Source.md")).unwrap(),
        "[[Renamed]]\n"
    );
    clean(&mut engine);
    engine.poll_file_operations();
    engine.take_file_operation_result(second).unwrap().unwrap();
    assert!(!fixture.0.join("Other.md").exists());
    assert!(fixture.0.join("Second.md").exists());
    assert!(engine.pending_link_update().is_none());
}
#[test]
fn queued_source_follows_its_identity_after_rename() {
    let fixture = Fixture::new();
    let mut engine = fixture.engine();
    let id = engine.begin_file_rename("Other.md", "Final.md", "Final.md");
    engine.vault.rename("Other.md", "Moved.md").unwrap();
    engine.file_renamed("Other.md", "Moved.md");
    clean(&mut engine);
    engine.poll_file_operations();
    engine.take_file_operation_result(id).unwrap().unwrap();
    assert!(!fixture.0.join("Moved.md").exists());
    assert_eq!(
        fs::read_to_string(fixture.0.join("Final.md")).unwrap(),
        "other\n"
    );
}
#[test]
fn queued_deleted_identity_does_not_rename_a_replacement_at_same_path() {
    let fixture = Fixture::new();
    let mut engine = fixture.engine();
    let id = engine.begin_file_rename("Other.md", "Final.md", "Final.md");
    engine.vault.delete("Other.md").unwrap();
    engine.file_deleted("Other.md");
    engine.vault.create("Other.md", "replacement\n").unwrap();
    engine.file_created("Other.md");
    clean(&mut engine);
    engine.poll_file_operations();
    assert!(engine.take_file_operation_result(id).unwrap().is_err());
    assert_eq!(
        fs::read_to_string(fixture.0.join("Other.md")).unwrap(),
        "replacement\n"
    );
    assert!(!fixture.0.join("Final.md").exists());
}
#[test]
fn current_loaded_source_lookup_distinguishes_reconciled_delete_from_disk_read_failure() {
    for reconciled in [true, false] {
        let fixture = Fixture::new();
        let mut engine = fixture.engine();
        let id = pending(&mut engine);
        if reconciled {
            engine.vault.delete("Source.md").unwrap();
            engine.file_deleted("Source.md");
        } else {
            fs::remove_file(fixture.0.join("Source.md")).unwrap();
        }
        engine
            .decide_link_update(id, LinkUpdateDecision::Once)
            .unwrap();
        engine.poll_file_operations();
        let result = engine.take_file_operation_result(id).unwrap();
        assert_eq!(result.is_ok(), reconciled);
        assert!(
            !fixture.0.join("Source.md").exists(),
            "must not resurrect source"
        );
        assert!(
            fixture.0.join("Renamed.md").exists(),
            "physical rename is retained"
        );
    }
}
#[test]
fn always_choice_survives_disk_config_refresh_and_shutdown_flushes_pending_save() {
    let fixture = Fixture::new();
    let mut engine = fixture.engine();
    let id = pending(&mut engine);
    engine
        .decide_link_update(id, LinkUpdateDecision::Always)
        .unwrap();
    engine.poll_file_operations();
    engine.take_file_operation_result(id).unwrap().unwrap();
    assert_eq!(
        engine.get_config("alwaysUpdateLinks"),
        &serde_json::json!(true)
    );
    let disk: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture.0.join(CONFIG_DIR).join("app.json")).unwrap()).unwrap();
    assert_eq!(disk["alwaysUpdateLinks"], false);
    engine.refresh_vault().unwrap();
    assert_eq!(
        engine.get_config("alwaysUpdateLinks"),
        &serde_json::json!(true)
    );
    engine.prepare_shutdown().unwrap();
    let disk: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture.0.join(CONFIG_DIR).join("app.json")).unwrap()).unwrap();
    assert_eq!(disk["alwaysUpdateLinks"], true);
}

#[test]
fn initial_config_save_does_not_hide_genuine_external_config_reload() {
    let fixture = Fixture::new();
    let mut engine = fixture.engine();
    fs::write(
        fixture.0.join(CONFIG_DIR).join("app.json"),
        b"{\"alwaysUpdateLinks\":false,\"newLinkFormat\":\"relative\"}",
    )
    .unwrap();
    engine.refresh_vault().unwrap();
    assert_eq!(
        engine.get_config("newLinkFormat"),
        &serde_json::json!("relative")
    );
    engine.prepare_shutdown().unwrap();
    let disk: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture.0.join(CONFIG_DIR).join("app.json")).unwrap()).unwrap();
    assert_eq!(disk["newLinkFormat"], "relative");
}
