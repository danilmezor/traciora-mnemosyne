use mnemosyne_engine::Engine;
use serde_json::Value;
use std::{fs, path::PathBuf, process::Command};
struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
#[test]
fn inventory_matches_73_actual_registered_handler_outputs() {
    let root = std::env::temp_dir().join(format!(
        "mn-inventory-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let scratch = Scratch(root);
    let vault = scratch.0.join("vault");
    fs::create_dir_all(&vault).unwrap();
    let fixture: Value = serde_json::from_str(include_str!("fixtures/inventory.json")).unwrap();
    for directory in fixture["initial"]["directories"].as_array().unwrap() {
        fs::create_dir_all(vault.join(directory.as_str().unwrap())).unwrap();
    }
    for (path, data) in fixture["initial"]["files"].as_object().unwrap() {
        let path = vault.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let bytes: Vec<u8> = if let Some(text) = data.as_str() {
            text.as_bytes().to_vec()
        } else {
            serde_json::from_value(data.clone()).unwrap()
        };
        fs::write(path, bytes).unwrap();
    }
    let mut engine = Engine::open(&vault).unwrap();
    for case in fixture["cases"].as_array().unwrap() {
        let args: Vec<String> = serde_json::from_value(case["args"].clone()).unwrap();
        let expected = case["expected"]["stdout"]
            .as_str()
            .unwrap()
            .replace("$VAULT", vault.to_str().unwrap());
        assert_eq!(
            mnemosyne::app::execute(&mut engine, &args),
            expected,
            "{} retained",
            case["id"]
        );
        let output = Command::new(env!("CARGO_BIN_EXE_mnemosyne"))
            .args(&args)
            .env("MNEMOSYNE_VAULT", &vault)
            .env_remove("MNEMOSYNE_SOCKET")
            .current_dir(&scratch.0)
            .output()
            .unwrap();
        assert_eq!(output.stdout, expected.as_bytes(), "{} process", case["id"]);
        assert_eq!(
            output.stderr,
            case["expected"]["stderr"].as_str().unwrap().as_bytes()
        );
        assert_eq!(
            output.status.code(),
            case["expected"]["exit_code"].as_i64().map(|n| n as i32)
        );
    }
}
