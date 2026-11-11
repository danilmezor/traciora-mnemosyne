//! Actual normal CLI routing, supplementing the synthetic registry formatter corpus.
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::Value;
use std::{fs, path::PathBuf, process::Command};

struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn normal_cli_version_help_and_errors_match_pinned_native_pipes() {
    let manifest: Value = serde_json::from_str(include_str!("fixtures/routing_scenarios.json")).unwrap();
    let scenario = manifest["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["id"] == "normal-cli-routing")
        .unwrap();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let fixture =
        Fixture(std::env::temp_dir().join(format!("mn-routing-{}-{stamp}", std::process::id())));
    let vault = fixture.0.join("vault");
    let home = fixture.0.join("home");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&vault).unwrap();
    for section in ["files", "config_files"] {
        for (path, text) in scenario["initial"][section].as_object().unwrap() {
            let destination = vault.join(path);
            fs::create_dir_all(destination.parent().unwrap()).unwrap();
            fs::write(destination, text.as_str().unwrap()).unwrap();
        }
    }
    for step in scenario["steps"].as_array().unwrap() {
        let args: Vec<String> = serde_json::from_value(step["args"].clone()).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_mnemosyne"))
            .args(&args)
            .env("HOME", &home)
            .env("MNEMOSYNE_VAULT", &vault)
            .env("TZ", "UTC")
            .env_remove("MNEMOSYNE_SOCKET")
            .env_remove("MNEMOSYNE_PROFILE")
            .current_dir(&home)
            .output()
            .unwrap();
        let expected = &step["expected_process"];
        assert_eq!(
            output.status.code(),
            Some(expected["exit_code"].as_i64().unwrap() as i32),
            "{args:?}"
        );
        assert_eq!(
            output.stdout,
            STANDARD
                .decode(expected["stdout"].as_str().unwrap())
                .unwrap(),
            "{args:?} stdout"
        );
        assert_eq!(
            output.stderr,
            STANDARD
                .decode(expected["stderr"].as_str().unwrap())
                .unwrap(),
            "{args:?} stderr"
        );
        assert_eq!(
            fs::read(vault.join("Note.md")).unwrap(),
            b"Hello parity.\n",
            "{args:?} saved source"
        );
        for (path, text) in scenario["initial"]["config_files"].as_object().unwrap() {
            assert_eq!(
                fs::read(vault.join(path)).unwrap(),
                text.as_str().unwrap().as_bytes(),
                "{args:?} config {path}"
            );
        }
    }
}
