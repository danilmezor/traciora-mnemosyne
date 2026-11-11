use mnemosyne_core::CONFIG_DIR;
use mnemosyne_engine::Engine;
use serde_json::Value;
use std::{fs, path::PathBuf};
struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
#[test]
fn property_search_matches_100_native_constructor_coercion_outputs() {
    let suite: Value = serde_json::from_str(include_str!("fixtures/typed_search.json")).unwrap();
    let root =
        Scratch(std::env::temp_dir().join(format!("mn-typed-search-{}", std::process::id())));
    fs::create_dir_all(root.0.join(CONFIG_DIR)).unwrap();
    fs::write(
        root.0.join(CONFIG_DIR).join("core-plugins.json"),
        r#"{"global-search":true}"#,
    )
    .unwrap();
    let mut engine = Engine::open(&root.0).unwrap();
    let order: Vec<String> = serde_json::from_value(suite["setup"]["files"].clone()).unwrap();
    for path in order.iter().rev() {
        engine
            .vault
            .create(path, suite["initial_files"][path].as_str().unwrap())
            .unwrap();
        engine.file_created(path);
    }
    let cache_order: Vec<String> =
        serde_json::from_value(suite["setup"]["cache_order"].clone()).unwrap();
    engine.import_cache_order(&cache_order).unwrap();
    assert_eq!(engine.ordered_note_paths(), order);
    let mut failures = Vec::new();
    for case in suite["cases"].as_array().unwrap() {
        let args: Vec<String> = serde_json::from_value(case["args"].clone()).unwrap();
        let output = mnemosyne::app::execute(&mut engine, &args);
        if output != case["expected"]["stdout"] {
            failures.push(format!(
                "{args:?}: actual={output:?} expected={}",
                case["expected"]["stdout"]
            ));
        }
        assert_eq!(case["expected"]["exit_code"], 0);
        assert_eq!(case["expected"]["stderr"], "");
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
