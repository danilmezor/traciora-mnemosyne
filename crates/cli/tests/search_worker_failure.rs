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
fn failed_metadata_keeps_body_search_and_excludes_property_and_tag_inputs() {
    let data: Value =
        serde_json::from_str(include_str!("fixtures/search_worker_failure.json")).unwrap();
    let root =
        Scratch(std::env::temp_dir().join(format!("mn-search-failure-{}", std::process::id())));
    fs::create_dir_all(root.0.join(CONFIG_DIR)).unwrap();
    fs::write(
        root.0.join(CONFIG_DIR).join("core-plugins.json"),
        r#"{"global-search":true}"#,
    )
    .unwrap();
    let mut engine = Engine::open(&root.0).unwrap();
    for case in data["cases"].as_array().unwrap() {
        let path = case["path"].as_str().unwrap();
        engine
            .vault
            .create(path, case["source"].as_str().unwrap())
            .unwrap();
        engine.file_created(path);
    }
    let mut commands = data["search_cases"]
        .as_array()
        .unwrap()
        .iter()
        .collect::<Vec<_>>();
    for case in data["cases"].as_array().unwrap() {
        commands.extend(
            case["reads"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|row| row["args"][0].as_str() == Some("search")),
        );
    }
    assert_eq!(commands.len(), 25);
    for row in commands {
        let args: Vec<String> = serde_json::from_value(row["args"].clone()).unwrap();
        assert_eq!(
            mnemosyne::app::execute(&mut engine, &args),
            row["output"]["stdout"],
            "{args:?}"
        );
        assert_eq!(row["output"]["exit_code"], 0);
        assert_eq!(row["output"]["stderr"], "");
    }
    for row in data["after_search_cases"].as_array().unwrap() {
        let args: Vec<String> = serde_json::from_value(row["args"].clone()).unwrap();
        assert_eq!(
            mnemosyne::app::execute(&mut engine, &args),
            row["output"]["stdout"]
        );
    }
}
