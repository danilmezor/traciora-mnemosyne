//! Observer plumbing tests; native session/event conformance is a separate gate.
use mnemosyne_core::CONFIG_DIR;
use mnemosyne_engine::{
    observation::{snapshot, Selection},
    Engine, FileTarget, OpenOptions,
};
use serde_json::{json, Value};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn fixture() -> Fixture {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    // Concurrent tests can observe the same wall-clock tick. Claim a fresh
    // directory exclusively so no fixture can populate or remove another's.
    let root = loop {
        let sequence = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "mn-observe-{}-{stamp}-{sequence}",
            std::process::id()
        ));
        match fs::create_dir(&path) {
            Ok(()) => break Fixture(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => panic!("cannot create observer fixture {}: {error}", path.display()),
        }
    };
    fs::create_dir(root.0.join(CONFIG_DIR)).unwrap();
    fs::write(root.0.join("Note.md"), "# Saved\nsaved\n").unwrap();
    fs::write(root.0.join("image.png"), [0, 255, 13, 10]).unwrap();
    fs::write(
        root.0.join(CONFIG_DIR).join("app.json"),
        r#"{"custom":null,"alwaysUpdateLinks":true}"#,
    )
    .unwrap();
    root
}

#[test]
fn observation_keeps_dirty_editor_distinct_from_saved_bytes() {
    let root = fixture();
    let mut engine = Engine::open(&root.0).unwrap();
    engine
        .open_file(FileTarget::path("Note.md"), OpenOptions::default())
        .unwrap();
    engine
        .editor_mut()
        .unwrap()
        .set_value("unsaved editor\n".into());
    let selection = Selection {
        session_fields: vec!["active_file".into(), "buffers".into(), "leaves".into()],
        ..Selection::default()
    };
    let observed = serde_json::to_value(snapshot(&engine, &selection).unwrap()).unwrap();
    assert_eq!(observed["observation"]["session"]["active_file"], "Note.md");
    let id = engine.session().active_leaf.as_deref().unwrap();
    let buffer = &observed["observation"]["session"]["buffers"][id];
    assert_eq!(buffer["text"], "unsaved editor\n");
    assert_eq!(buffer["saved_text"], "# Saved\nsaved\n");
    assert_eq!(buffer["dirty"], true);
    assert_eq!(
        fs::read_to_string(root.0.join("Note.md")).unwrap(),
        "# Saved\nsaved\n"
    );
    assert_eq!(
        observed["readiness"]["ready"], false,
        "snapshot alone cannot assert async readiness"
    );
}

#[test]
fn observed_null_config_and_empty_attachment_cache_are_present() {
    let root = fixture();
    let engine = Engine::open(&root.0).unwrap();
    let selection = Selection {
        effective_config_keys: vec!["custom".into(), "nonexistent".into()],
        metadata_paths: vec!["image.png".into(), "missing.png".into()],
        ..Selection::default()
    };
    let observed = serde_json::to_value(snapshot(&engine, &selection).unwrap()).unwrap();
    assert_eq!(
        observed["observation"]["config"]["effective"]["custom"],
        json!({"present":true,"value":null})
    );
    assert_eq!(
        observed["observation"]["config"]["effective"]["nonexistent"],
        json!({"present":false})
    );
    assert_eq!(
        observed["observation"]["metadata"]["cache"]["image.png"],
        json!({"present":true,"value":{}})
    );
    assert_eq!(
        observed["observation"]["metadata"]["cache"]["missing.png"],
        json!({"present":false})
    );
}

#[test]
fn unimplemented_journal_and_session_fields_remain_unavailable() {
    let root = fixture();
    let engine = Engine::open(&root.0).unwrap();
    let selection = Selection {
        session_fields: vec!["workspace_groups".into()],
        event_names: vec!["vault:create".into()],
        ..Selection::default()
    };
    let observed: Value = serde_json::to_value(snapshot(&engine, &selection).unwrap()).unwrap();
    assert!(observed["unavailable"]["events"].is_string());
    assert!(observed["unavailable"]["session/workspace_groups"].is_string());
    assert!(
        observed["observation"].get("events").is_none(),
        "missing journal is not an empty event list"
    );
    assert!(serde_json::from_value::<Selection>(json!({"invented":true})).is_err());
}

#[test]
fn actual_journal_observation_filters_names_and_rejects_unknown_domains() {
    let root = fixture();
    let mut engine = Engine::open(&root.0).unwrap();
    let cursor = engine.start_event_journal();
    let path = engine.vault.create("Created.md", "created\n").unwrap();
    engine.file_created(&path);
    let selection = Selection {
        event_names: vec!["vault:create".into(), "metadata:unknown".into()],
        after_cursor: cursor,
        ..Selection::default()
    };
    let observed = serde_json::to_value(snapshot(&engine, &selection).unwrap()).unwrap();
    assert_eq!(
        observed["observation"]["events"]["records"][0]["path"],
        "Created.md"
    );
    assert_eq!(
        observed["observation"]["events"]["records"][0]["name"],
        "create"
    );
    assert_eq!(
        observed["observation"]["events"]["records"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(observed["unavailable"]["events/metadata:unknown"].is_string());
    let after = Selection {
        after_cursor: engine.event_cursor(),
        event_names: vec!["vault:create".into()],
        ..Selection::default()
    };
    let observed = serde_json::to_value(snapshot(&engine, &after).unwrap()).unwrap();
    assert_eq!(observed["observation"]["events"]["records"], json!([]));
}

#[test]
fn mixed_lifecycle_cursor_failures_never_fall_back_to_vault_history() {
    let root = fixture();
    let mut engine = Engine::open(&root.0).unwrap();
    engine.start_event_journal();
    let first = engine.start_lifecycle_journal();
    let path = engine.vault.create("Created.md", "created\n").unwrap();
    engine.file_created(&path);
    let current = engine.start_lifecycle_journal();
    assert!(current > first);
    for cursor in [first, current + 1] {
        let selection = Selection {
            event_names: vec!["vault:create".into(), "metadata:changed".into()],
            after_cursor: cursor,
            ..Selection::default()
        };
        let observed = serde_json::to_value(snapshot(&engine, &selection).unwrap()).unwrap();
        assert!(observed["observation"].get("events").is_none());
        assert!(observed["unavailable"]["events/vault:create"].is_string());
        assert!(observed["unavailable"]["events/metadata:changed"].is_string());
    }
    engine.stop_lifecycle_journal();
    let selection = Selection {
        event_names: vec!["vault:create".into(), "metadata:changed".into()],
        after_cursor: 0,
        ..Selection::default()
    };
    let observed = serde_json::to_value(snapshot(&engine, &selection).unwrap()).unwrap();
    assert!(observed["observation"].get("events").is_none());
    assert!(observed["unavailable"]["events/vault:create"].is_string());
    assert!(observed["unavailable"]["events/metadata:changed"].is_string());
}
