//! The 10ms debounce and programmatic peer update are shipped-source contracts.
//! The existing native separate-command session trace verifies the unsaved
//! peer texts while disk stays saved; the newer immediate pane trace verifies
//! that editors are independent before this asynchronous callback.
use mnemosyne_engine::{Engine, FileTarget, OpenOptions};
use serde_json::{json, Value};
use std::{
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};
struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
#[test]
fn quick_preview_updates_same_file_peers_after_ten_ms_without_saving_or_moving_owner_selection() {
    let native: Value =
        serde_json::from_str(include_str!("fixtures/reference_session.json")).unwrap();
    let root = Fixture(std::env::temp_dir().join(format!(
            "mn-peer-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )));
    fs::create_dir_all(&root.0).unwrap();
    fs::write(
        root.0.join("A.md"),
        native["initial_files"]["A.md"].as_str().unwrap(),
    )
    .unwrap();
    let mut engine = Engine::open(&root.0).unwrap();
    engine
        .open_file(FileTarget::path("A.md"), OpenOptions::default())
        .unwrap();
    let first = engine.session().active_leaf.clone().unwrap();
    engine
        .open_file(FileTarget::path("A.md"), OpenOptions { new_tab: true })
        .unwrap();
    let second = engine.session().active_leaf.clone().unwrap();
    let text = native["steps"][11]["editor_set_value"].as_str().unwrap();
    let cursor = engine.start_event_journal();
    engine.set_editor_value_at(text.into(), 0).unwrap();
    assert_eq!(engine.editor_for_leaf(&first).unwrap().text, "saved A\n");
    assert_eq!(engine.editor_for_leaf(&second).unwrap().text, text);
    let first_selection = engine.editor_for_leaf(&first).unwrap().selection.clone();
    let second_selection = engine.editor_for_leaf(&second).unwrap().selection.clone();
    assert!(engine.process_markdown_timers(9).unwrap().is_empty());
    assert_eq!(engine.editor_for_leaf(&first).unwrap().text, "saved A\n");
    assert!(engine.process_markdown_timers(10).unwrap().is_empty());
    let expected = &native["steps"][12]["expected_state"]["leaves"];
    assert_eq!(
        engine.editor_for_leaf(&first).unwrap().text,
        expected[0]["text"]
    );
    assert_eq!(
        engine.editor_for_leaf(&second).unwrap().text,
        expected[1]["text"]
    );
    assert_eq!(
        engine.editor_for_leaf(&first).unwrap().selection,
        first_selection
    );
    assert_eq!(
        engine.editor_for_leaf(&second).unwrap().selection,
        second_selection
    );
    assert_eq!(
        engine.markdown_leaf_state(&first).unwrap().reading_data(),
        text
    );
    assert_eq!(
        engine
            .markdown_leaf_state(&first)
            .unwrap()
            .last_saved_data(),
        "saved A\n"
    );
    assert_eq!(
        engine.editor_for_leaf(&first).unwrap().edit_revision,
        0,
        "programmatic set is not a user edit"
    );
    assert_eq!(
        engine.read_saved(FileTarget::default()).unwrap(),
        native["steps"][12]["expected_process"]["stdout"]
    );
    assert!(engine.event_journal_since(cursor).unwrap().is_empty());
    assert_eq!(engine.process_markdown_timers(2000).unwrap(), vec![second]);
    assert_eq!(fs::read_to_string(root.0.join("A.md")).unwrap(), text);
    let events = engine.event_journal_since(cursor).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(json!(events[0].name), "modify");
}

#[test]
fn programmatic_quick_preview_peer_uses_clean_saved_reload_branch() {
    // Authored Q4.loadFileInternal dirty-branch invariant. A peer changed by a
    // `set` transaction has unsaved text but is not a user-dirty editor.
    let root = Fixture(std::env::temp_dir().join(format!(
        "mn-clean-peer-{}-{}", std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
    )));
    fs::create_dir_all(&root.0).unwrap();
    fs::write(root.0.join("A.md"), "saved A\n").unwrap();
    let mut engine = Engine::open(&root.0).unwrap();
    engine
        .open_file(FileTarget::path("A.md"), OpenOptions::default())
        .unwrap();
    let first = engine.session().active_leaf.clone().unwrap();
    engine
        .open_file(FileTarget::path("A.md"), OpenOptions { new_tab: true })
        .unwrap();
    engine.set_editor_value_at("draft A\n".into(), 0).unwrap();
    engine.process_markdown_timers(10).unwrap();
    assert_eq!(engine.editor_for_leaf(&first).unwrap().text, "draft A\n");
    assert_eq!(engine.editor_for_leaf(&first).unwrap().edit_revision, 0);
    engine.vault.modify("A.md", "external A\n").unwrap();
    engine.file_modified("A.md");
    assert_eq!(engine.editor_for_leaf(&first).unwrap().text, "external A\n");
    assert_eq!(
        engine.editor_for_leaf(&first).unwrap().saved_text,
        "external A\n"
    );
}
