use mnemosyne_core::CONFIG_DIR;
use mnemosyne_engine::{Engine, FileTarget};
use serde_json::{json, Value};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn fixture(suite: &Value, case: &Value) -> (Fixture, Engine) {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let root = std::env::temp_dir().join(format!(
        "mn-meta-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(root.join(CONFIG_DIR)).unwrap();
    fs::write(
        root.join(CONFIG_DIR).join("core-plugins.json"),
        r#"{"outline":true}"#,
    )
    .unwrap();
    let mut engine = Engine::open(&root).unwrap();
    // Actual Vault traversal and cache insertion chronology are separate inputs;
    // this replay does not certify their asynchronous cold initialization.
    let note_order: Vec<String> = serde_json::from_value(case["note_order"].clone()).unwrap();
    for (path, text) in suite["initial_files"].as_object().unwrap() {
        if !note_order.contains(path) {
            engine.vault.create(path, text.as_str().unwrap()).unwrap();
            engine.file_created(path);
        }
    }
    for path in note_order.iter().rev() {
        engine
            .vault
            .create(path, suite["initial_files"][path].as_str().unwrap())
            .unwrap();
        engine.file_created(path);
    }
    assert_eq!(engine.ordered_note_paths(), note_order);
    let cache_order: Vec<String> = serde_json::from_value(case["cache_order"].clone()).unwrap();
    engine.import_cache_order(&cache_order).unwrap();
    engine
        .set_config(
            "userIgnoreFilters",
            case.get("ignore").cloned().unwrap_or(json!([])),
        )
        .unwrap();
    if let Some(path) = case["opened"].as_str() {
        engine
            .open_file(FileTarget::path(path), Default::default())
            .unwrap();
    }
    if let Some(dirty) = case["dirty"].as_str() {
        engine.editor_mut().unwrap().set_value(dirty.into());
    }
    (Fixture(root), engine)
}
#[test]
fn tags_aliases_outline_match_180_actual_saved_cache_outputs() {
    let suite: Value =
        serde_json::from_str(include_str!("fixtures/metadata_commands.json")).unwrap();
    let mut failures = Vec::new();
    for case in suite["cases"].as_array().unwrap() {
        let (root, mut engine) = fixture(&suite, case);
        let args: Vec<String> = serde_json::from_value(case["args"].clone()).unwrap();
        let output = mnemosyne::app::execute(&mut engine, &args);
        if output != case["expected"]["stdout"] {
            failures.push(format!(
                "{} actual {:?} expected {}",
                case["id"], output, case["expected"]["stdout"]
            ));
        }
        assert_eq!(case["expected"]["stderr"], "");
        assert_eq!(case["expected"]["exit_code"], 0);
        if let Some(proof) = case.get("dirty_proof") {
            let path = case["opened"].as_str().unwrap();
            assert_eq!(
                fs::read_to_string(root.0.join(path)).unwrap(),
                proof["saved"]
            );
            assert_eq!(engine.editor().unwrap().text, proof["editor"]);
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
#[test]
fn outline_registration_is_absent_in_the_captured_disabled_state() {
    let suite: Value =
        serde_json::from_str(include_str!("fixtures/metadata_commands.json")).unwrap();
    let (root, _) = fixture(&suite, &suite["cases"][0]);
    fs::write(
        root.0.join(CONFIG_DIR).join("core-plugins.json"),
        r#"{"outline":false}"#,
    )
    .unwrap();
    let mut engine = Engine::open(&root.0).unwrap();
    assert_eq!(
        mnemosyne::app::execute(&mut engine, &["outline".into(), "path=Alpha.md".into()]),
        suite["disabled_outline"]["stdout"]
    );
}
