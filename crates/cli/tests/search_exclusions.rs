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
fn exclusions_match_reference_decisions_and_command_bytes() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/search_exclusions.json")).unwrap();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let scratch =
        Scratch(std::env::temp_dir().join(format!("mn-ignore-{}-{stamp}", std::process::id())));
    fs::create_dir_all(&scratch.0).unwrap();
    let mut engine = Engine::open(&scratch.0).unwrap();
    // Reproduce captured creation chronology rather than asserting that the
    // reference's concurrent cold enumeration is lexically deterministic.
    for (path, text) in fixture["dataset"].as_object().unwrap() {
        let file = scratch.0.join(path);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, text.as_str().unwrap()).unwrap();
        engine.refresh_vault().unwrap();
        if !path.split('/').any(|part| part.starts_with('.')) {
            engine.file_created(path);
        }
    }
    let order: Vec<String> = engine
        .ordered_note_paths()
        .into_iter()
        .map(str::to_owned)
        .collect();
    assert_eq!(
        serde_json::to_value(order).unwrap(),
        fixture["cases"][0]["before_queries"]["markdown"]
    );
    for (number, case) in fixture["cases"].as_array().unwrap().iter().enumerate() {
        engine
            .set_config("userIgnoreFilters", case["filters"].clone())
            .unwrap();
        for (path, expected) in case["decisions"].as_object().unwrap() {
            assert_eq!(
                engine.is_user_ignored(path),
                expected.as_bool().unwrap(),
                "config {number} path {path:?}"
            );
        }
        for command in case["raw"].as_array().unwrap() {
            let args: Vec<String> = serde_json::from_value(command["args"].clone()).unwrap();
            assert_eq!(
                mnemosyne::app::execute(&mut engine, &args),
                command["stdout"].as_str().unwrap(),
                "config {number} {args:?}"
            );
            assert_eq!(command["stderr"], "");
            assert_eq!(command["exit_code"], 0);
        }
    }
}
