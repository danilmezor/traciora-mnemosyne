//! Authored source-contract cases, not additional native captures. Shipped
//! getUnpinnedLeaf confines fallback to visible leaves in the active container;
//! canNavigate requires the view's navigation capability and an unpinned leaf.
use mnemosyne_engine::{panes::PaneRequest, Engine};
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn setup() -> (Fixture, Engine) {
    let root = loop {
        let root = std::env::temp_dir().join(format!(
            "mn-selector-{}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        match std::fs::create_dir(&root) {
            Ok(()) => break root,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => panic!("create exclusive fixture {}: {error}", root.display()),
        }
    };
    let f = Fixture(root);
    for p in ["A.md", "B.md", "C.md"] {
        std::fs::write(f.0.join(p), p).unwrap();
    }
    let e = Engine::open(&f.0).unwrap();
    (f, e)
}
fn leaf(id: &str, kind: &str, file: Option<&str>, pinned: bool) -> Value {
    json!({"id":id,"type":"leaf","pinned":pinned,"state":{"type":kind,"state":file.map(|p|json!({"file":p})).unwrap_or(json!({}))}})
}
fn layout(active: &str, groups: Vec<Value>) -> Value {
    json!({"main":{"id":"root","type":"split","direction":"vertical","children":[{"id":"main-tabs","type":"tabs","children":[leaf("main","markdown",Some("A.md"),false)]}]},"floating":{"id":"floating","type":"floating","children":[{"id":"window","type":"window","direction":"vertical","children":groups}]},"active":active})
}
#[test]
fn pinned_floating_reuse_appends_in_its_container_without_reusing_hidden_empty() {
    let (_f, mut e) = setup();
    e.import_workspace(layout("float",vec![json!({"id":"float-tabs","type":"tabs","children":[leaf("float","markdown",Some("B.md"),true),leaf("hidden-empty","empty",None,false)]})])).unwrap();
    assert!(
        e.session()
            .leaves
            .iter()
            .find(|l| l.id == "float")
            .unwrap()
            .pinned
    );
    let id = e.get_leaf(PaneRequest::Reuse).unwrap();
    assert_ne!(id, "main");
    assert_ne!(id, "float");
    assert_ne!(id, "hidden-empty");
    let w = e.workspace_layout();
    let children = w["floating"]["children"][0]["children"][0]["children"]
        .as_array()
        .unwrap();
    assert_eq!(children.len(), 3);
    assert_eq!(children[2]["id"], id);
    assert_eq!(w["active"], id);
    assert_eq!(
        w["floating"]["children"][0]["children"][0]["children"][0]["pinned"],
        true
    );
}
#[test]
fn nonnavigable_item_view_fallback_uses_visible_graph_in_same_window() {
    let (_f, mut e) = setup();
    e.import_workspace(layout("local",vec![json!({"id":"local-tabs","type":"tabs","children":[leaf("local","localgraph",None,false)]}),json!({"id":"graph-tabs","type":"tabs","children":[leaf("global-graph","graph",None,false)]})])).unwrap();
    // A more recent mainland leaf must not escape the active window container.
    e.activate_leaf("main").unwrap();
    e.activate_leaf("local").unwrap();
    assert_eq!(e.get_leaf(PaneRequest::Reuse).unwrap(), "global-graph");
    assert_eq!(e.session().active_leaf.as_deref(), Some("global-graph"));
}
#[test]
fn stacked_group_allows_visible_unpinned_file_and_unknown_item_views_do_not_navigate() {
    let (_f, mut e) = setup();
    let mut w = layout(
        "custom",
        vec![
            json!({"id":"stacked-tabs","type":"tabs","stacked":true,"children":[leaf("custom","foreign-item-view",None,false),leaf("file","markdown",Some("C.md"),false)]}),
        ],
    );
    e.import_workspace(w.clone()).unwrap();
    assert_eq!(e.get_leaf(PaneRequest::Reuse).unwrap(), "file");
    w["floating"]["children"][0]["children"][0]["stacked"] = json!(false);
    e.import_workspace(w).unwrap();
    let id = e.get_leaf(PaneRequest::Reuse).unwrap();
    assert_ne!(id, "custom");
    assert_ne!(id, "file");
    assert_ne!(id, "main");
}
#[test]
fn normal_startup_resets_activity_ranks_but_internal_restore_retains_them() {
    let (f, mut e) = setup();
    e.import_workspace(layout("float", vec![json!({"id":"float-tabs","type":"tabs","children":[leaf("float","markdown",Some("B.md"),true)]})])).unwrap();
    let mut state = e.session().clone();
    for leaf in &mut state.leaves {
        leaf.active_order = if leaf.id == "main" { 41 } else { 90 };
    }
    state
        .leaves
        .iter_mut()
        .find(|leaf| leaf.id == "float")
        .unwrap()
        .back_history
        .push("A.md".into());
    state.next_activation = 91;
    e.restore(state.clone()).unwrap();
    assert_eq!(
        e.session()
            .leaves
            .iter()
            .find(|l| l.id == "main")
            .unwrap()
            .active_order,
        41
    );
    e.persist_session().unwrap();
    let mut restarted = Engine::open(&f.0).unwrap();
    assert_eq!(
        restarted
            .session()
            .leaves
            .iter()
            .find(|l| l.id == "main")
            .unwrap()
            .active_order,
        0
    );
    assert_eq!(
        restarted
            .session()
            .leaves
            .iter()
            .find(|l| l.id == "float")
            .unwrap()
            .active_order,
        1
    );
    assert!(restarted
        .session()
        .leaves
        .iter()
        .all(|leaf| leaf.back_history.is_empty()));
    restarted.restore(state).unwrap();
    assert_eq!(
        restarted
            .session()
            .leaves
            .iter()
            .find(|leaf| leaf.id == "float")
            .unwrap()
            .back_history,
        vec!["A.md"]
    );
    assert_eq!(
        restarted
            .session()
            .leaves
            .iter()
            .find(|l| l.id == "main")
            .unwrap()
            .active_order,
        41
    );
}
