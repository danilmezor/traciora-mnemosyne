//! Extension create/reopen cache distinction, backed by the focused same-vault
//! native lifecycle fixture. Empty attachment caches are not Markdown sources.
use mnemosyne_engine::Engine;
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
fn empty(engine: &Engine, path: &str, expected: &Value) {
    assert_eq!(
        serde_json::to_value(engine.file_cache(path)).unwrap(),
        *expected,
        "{path} cache"
    );
    assert!(
        engine.cached_file_paths().contains(&path),
        "{path} cached discovery"
    );
    assert!(
        !engine.ordered_note_paths().contains(&path),
        "{path} is not Markdown"
    );
    assert!(
        !engine.resolved_link_paths().contains(&path),
        "{path} is not a graph source"
    );
}
#[test]
fn native_non_markdown_empty_cache_survives_create_rename_and_reopen_without_graph_membership() {
    let native: Value = serde_json::from_str(include_str!(
        "../../cli/tests/fixtures/file_property.json"
    ))
    .unwrap();
    assert_eq!(native["reference_version"], "1.14.4");
    let root = Fixture(std::env::temp_dir().join(format!(
            "mn-empty-cache-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )));
    fs::create_dir_all(root.0.join("Folder")).unwrap();
    let bytes: Vec<u8> =
        serde_json::from_value(native["initial_files"]["image.png"].clone()).unwrap();
    fs::write(root.0.join("image.png"), &bytes).unwrap();
    fs::write(root.0.join("Note.md"), "saved Markdown\n").unwrap();
    fs::write(root.0.join(".hidden.png"), &bytes).unwrap();
    let mut engine = Engine::open(&root.0).unwrap();
    empty(
        &engine,
        "image.png",
        &native["before"]["metadata"]["cache"]["image.png"],
    );
    for missing in ["missing.png", ".hidden.png", "Folder"] {
        assert!(
            engine.file_cache(missing).is_none(),
            "{missing} must not get an attachment cache"
        );
    }
    engine.vault.create("Board.canvas", "").unwrap();
    engine.file_created("Board.canvas");
    empty(
        &engine,
        "Board.canvas",
        &native["extension_steps"][3]["after"]["metadata"]["cache"]["Board.canvas"],
    );
    engine.vault.rename("image.png", "NewImage.png").unwrap();
    engine.file_renamed("image.png", "NewImage.png");
    assert!(engine.file_cache("image.png").is_none());
    empty(
        &engine,
        "NewImage.png",
        &native["before_restart"]["metadata"]["cache"]["NewImage.png"],
    );
    assert_eq!(engine.resolved_link_paths(), vec!["Note.md"]);
    drop(engine);
    let mut engine = Engine::open(&root.0).unwrap();
    for path in ["NewImage.png", "Board.canvas"] {
        empty(
            &engine,
            path,
            &native["after_restart"]["metadata"]["cache"][path],
        );
    }
    assert_eq!(engine.resolved_link_paths(), vec!["Note.md"]);
    // A save of apparent Markdown syntax into an attachment never runs the
    // Markdown metadata parser. Its cache remains empty after ordinary refresh.
    fs::write(root.0.join("NewImage.png"), "# Heading\n[[Note]] #tag\n").unwrap();
    engine.refresh_vault().unwrap();
    empty(&engine, "NewImage.png", &json!({}));
    engine.vault.delete("NewImage.png").unwrap();
    engine.file_deleted("NewImage.png");
    assert!(engine.file_cache("NewImage.png").is_none());
    assert!(!engine.cached_file_paths().contains(&"NewImage.png"));
}
