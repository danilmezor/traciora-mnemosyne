//! Focused actual-observation tests: immediate editor ownership, structural
//! pane operations, and saved native workspace import. Cleanup was forced;
//! graceful shutdown is a separate gate, not implied by these checks.
use mnemosyne_core::CONFIG_DIR;
use mnemosyne_engine::{panes::{PaneRequest, SplitDirection}, EditorPosition, EditorSelection, Engine, FileTarget, OpenOptions};
use serde_json::{json, Value};
use std::collections::BTreeMap;
fn fixture() -> Value { serde_json::from_str(include_str!("fixtures/session_panes.json")).unwrap() }
struct Fixture(std::path::PathBuf);
impl Fixture { fn path(&self) -> &std::path::Path { &self.0 } }
impl Drop for Fixture { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); } }
fn setup() -> (Fixture, Engine) {
    let dir = Fixture(std::env::temp_dir().join(format!("mn-pane-{}-{}",std::process::id(),std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos())));
    std::fs::create_dir_all(dir.path()).unwrap();
    std::fs::write(dir.path().join("A.md"), "saved A\nsecond\n").unwrap();
    std::fs::write(dir.path().join("B.md"), "saved B\n").unwrap();
    let engine = Engine::open(dir.path()).unwrap(); (dir, engine)
}
fn selection(value: &Value) -> EditorSelection { serde_json::from_value(value[0].clone()).unwrap() }
#[test]
fn dirty_same_file_new_tab_has_independent_editor_and_selection() {
    let f = fixture(); let (_dir, mut e) = setup();
    e.open_file(FileTarget::path("A.md"), OpenOptions::default()).unwrap();
    let first = e.session().active_leaf.clone().unwrap();
    let observed = &f["samefile"];
    let before = &observed["before"]["leaves"][0];
    e.editor_mut().unwrap().set_value(before["text"].as_str().unwrap().into());
    e.editor_mut().unwrap().set_selection(selection(&before["selection"])).unwrap();
    e.open_file(FileTarget::path("A.md"), OpenOptions { new_tab: true }).unwrap();
    let second = e.session().active_leaf.clone().unwrap();
    assert_ne!(first, second);
    assert_eq!(e.editor_for_leaf(&first).unwrap().text, before["text"].as_str().unwrap());
    assert_eq!(e.editor_for_leaf(&first).unwrap().selection, selection(&before["selection"]));
    assert_eq!(e.editor_for_leaf(&second).unwrap().text, observed["after"]["leaves"][1]["text"].as_str().unwrap());
    assert_eq!(e.editor_for_leaf(&second).unwrap().selection, selection(&observed["after"]["leaves"][1]["selection"]));
    let next = selection(&observed["selections"]["leaves"][1]["selection"]);
    e.editor_mut().unwrap().set_selection(next).unwrap();
    assert_eq!(e.editor_for_leaf(&first).unwrap().selection, selection(&before["selection"]));
    e.activate_leaf(&first).unwrap();
    assert_eq!(e.editor().unwrap().text, before["text"].as_str().unwrap());
}
// Compare a structural field whitelist. The native raw IDs remain in the
// fixture; an explicit bijection maps them to independently allocated IDs.
fn topology(value: &Value, ids: &mut BTreeMap<String, String>) -> Value {
    if let Some(id) = value["id"].as_str() { let next = format!("node-{}", ids.len()); ids.entry(id.into()).or_insert(next); }
    let children = value["children"].as_array().map(|items| items.iter().map(|child| topology(child, ids)).collect::<Vec<_>>());
    let mut result = json!({"id":ids[value["id"].as_str().unwrap()],"type":value["type"]});
    if let Some(children) = children { result["children"] = json!(children); }
    if let Some(direction) = value.get("direction") { result["direction"] = direction.clone(); }
    if value["type"] == "tabs" { result["currentTab"] = json!(value["currentTab"].as_u64().unwrap_or(0)); }
    if value["type"] == "leaf" {
        result["state"] = json!({"type":value["state"]["type"],"file":value["state"]["state"]["file"]});
    }
    result
}
fn structural(layout: &Value) -> Value {
    let mut ids = BTreeMap::new(); let main = topology(&layout["main"], &mut ids);
    let floating = layout.get("floating").map(|value| topology(value, &mut ids));
    let active = layout["active"].as_str().map(|id| ids.get(id).unwrap().clone());
    json!({"main":main,"floating":floating,"active":active})
}
#[test]
fn pane_requests_match_native_tab_split_window_topology() {
    let f = fixture(); let (_dir, mut e) = setup();
    // Start at the native state after the critical case; this isolates pane
    // allocation from separate asynchronous blur/save and cache notifications.
    e.import_workspace(f["after_cli"]["layout"].clone()).unwrap();
    for row in f["panes"].as_array().unwrap() {
        let pane = match row["pane"].as_str().unwrap() {
            "tab" => PaneRequest::Tab,
            "split" => PaneRequest::Split(if row["direction"] == "horizontal" { SplitDirection::Horizontal } else { SplitDirection::Vertical }),
            "window" => PaneRequest::Window, _ => unreachable!(),
        };
        let path = if pane == PaneRequest::Tab { "A.md" } else { "B.md" };
        e.open_file_in_pane(FileTarget::path(path), pane).unwrap();
        assert_eq!(structural(&e.workspace_layout()), structural(&row["observed"]["layout"]), "{pane:?}");
    }
}
#[test]
fn native_workspace_import_preserves_floating_active_and_resets_selection() {
    let f = fixture(); let (_dir, mut e) = setup();
    e.import_workspace(f["workspace_saved"].clone()).unwrap();
    assert_eq!(structural(&e.workspace_layout()), structural(&f["after_restart"]["layout"]));
    assert_eq!(e.session().active_leaf, f["after_restart"]["active"].as_str().map(str::to_owned));
    assert_eq!(e.editor().unwrap().selection, EditorSelection::default());
    let active = e.session().active_leaf.clone().unwrap();
    e.editor_mut().unwrap().set_selection(EditorSelection { anchor: EditorPosition { line:0,ch:1 }, head: EditorPosition { line:0,ch:4 } }).unwrap();
    let checkpoint = e.session().clone();
    e.restore(checkpoint).unwrap();
    assert_eq!(e.session().active_leaf.as_deref(), Some(active.as_str()));
    assert_eq!(e.editor().unwrap().selection.head.ch, 4); // Explicit internal checkpoint restore preserves ephemeral state.
    assert_eq!(structural(&e.workspace_layout()), structural(&f["after_restart"]["layout"]));
}
#[test]
fn old_private_file_buffers_migrate_without_sharing_leaf_ownership() {
    let (_dir, mut e) = setup();
    e.open_file(FileTarget::path("A.md"), OpenOptions::default()).unwrap();
    e.open_file(FileTarget::path("A.md"), OpenOptions { new_tab:true }).unwrap();
    let mut old = serde_json::to_value(e.session()).unwrap();
    old.as_object_mut().unwrap().remove("leaf_buffers"); old.as_object_mut().unwrap().remove("pane_layout");
    old["buffers"] = json!({"A.md":mnemosyne_engine::EditorBuffer::new("legacy dirty\n".into())});
    e.restore(serde_json::from_value(old).unwrap()).unwrap();
    let first = e.session().leaves[0].id.clone(); let second = e.session().leaves[1].id.clone();
    assert_eq!(e.editor_for_leaf(&first).unwrap().text, "legacy dirty\n");
    e.editor_for_leaf_mut(&second).unwrap().set_value("second only".into());
    assert_eq!(e.editor_for_leaf(&first).unwrap().text, "legacy dirty\n");
}
#[test]
fn graceful_shutdown_matches_native_source_and_hidden_preview_restart() {
    let f = fixture();
    for row in f["shutdown_cases"].as_array().unwrap() {
        let (dir, mut e) = setup();
        std::fs::write(dir.path().join("Shutdown.md"), "saved shutdown\n").unwrap();
        e.refresh_vault().unwrap();
        e.open_file(FileTarget::path("Shutdown.md"), OpenOptions::default()).unwrap();
        e.set_markdown_state(&json!({"mode":row["mode"],"source":true})).unwrap();
        e.editor_mut().unwrap().set_value(row["before"]["editor"].as_str().unwrap().into());
        e.editor_mut().unwrap().set_selection(selection(&row["before"]["selection"])).unwrap();
        assert_eq!(e.markdown_view_data().unwrap(), row["before"]["viewData"].as_str().unwrap());
        let id = e.session().active_leaf.clone().unwrap();
        e.prepare_shutdown().unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("Shutdown.md")).unwrap(), row["saved_after_quit"].as_str().unwrap());
        let restarted = Engine::open(dir.path()).unwrap();
        assert_eq!(restarted.session().active_leaf.as_deref(), Some(id.as_str()));
        assert_eq!(restarted.editor().unwrap().text, row["restart"]["editor"].as_str().unwrap());
        assert_eq!(restarted.editor().unwrap().selection, selection(&row["restart"]["selection"]));
        assert_eq!(serde_json::to_value(restarted.markdown_state().unwrap().mode).unwrap(), row["restart"]["mode"]);
    }
}
#[test]
fn normal_startup_loads_saved_content_while_explicit_restore_keeps_checkpoint() {
    let (dir, mut e) = setup();
    e.open_file(FileTarget::path("A.md"), OpenOptions::default()).unwrap();
    e.editor_mut().unwrap().set_value("not saved".into());
    e.editor_mut().unwrap().set_selection(EditorSelection { anchor: EditorPosition { line:0,ch:1 }, head: EditorPosition { line:0,ch:3 } }).unwrap();
    let checkpoint = e.session().clone();
    e.persist_session().unwrap();
    let mut restarted = Engine::open(dir.path()).unwrap();
    assert_eq!(restarted.editor().unwrap().text, "saved A\nsecond\n");
    assert_eq!(restarted.editor().unwrap().selection, EditorSelection::default());
    restarted.restore(checkpoint).unwrap();
    assert_eq!(restarted.editor().unwrap().text, "not saved");
    assert_eq!(restarted.editor().unwrap().selection.head.ch, 3);
}
#[test]
fn missing_active_and_empty_main_startup_match_native_fallback() {
    let f = fixture();
    for row in f["startup_cases"].as_array().unwrap() {
        let (dir, _) = setup();
        std::fs::create_dir_all(dir.path().join(CONFIG_DIR)).unwrap();
        std::fs::write(dir.path().join("ParityReady.md"), "Isolated parity oracle.\n").unwrap();
        if row["kind"] == "empty" {
            std::fs::write(dir.path().join("Startup.md"), "saved startup\n").unwrap();
        }
        std::fs::write(dir.path().join(CONFIG_DIR).join("workspace.json"), serde_json::to_vec(&row["input_workspace"]).unwrap()).unwrap();
        let e = Engine::open(dir.path()).unwrap();
        assert_eq!(structural(&e.workspace_layout()), structural(&row["observed"]["layout"]), "{}",row["kind"]);
        assert_eq!(e.active_file(), row["observed"]["leaves"][0]["file"].as_str());
        if row["kind"] == "missing" {
            assert_eq!(e.session().active_leaf.as_deref(), row["observed"]["active"].as_str());
            assert!(e.editor().is_none());
        } else {
            assert_ne!(e.workspace_layout()["main"]["id"], row["input_workspace"]["main"]["id"]);
            assert_eq!(e.editor().unwrap().selection, EditorSelection::default());
        }
    }
}
