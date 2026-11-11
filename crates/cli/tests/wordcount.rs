use mnemosyne_core::CONFIG_DIR;
use mnemosyne_engine::Engine;
use serde_json::{json, Value};
use std::{fs, path::PathBuf, process::Command};
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "mn-wordcount-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join(CONFIG_DIR)).unwrap();
        Self(root)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
#[test]
fn wordcount_matches_actual_saved_markdown_flags_errors_and_dirty_active_file() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/wordcount.json")).unwrap();
    let scratch = Scratch::new();
    for (path, text) in fixture["initial_files"].as_object().unwrap() {
        let path = scratch.0.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text.as_str().unwrap()).unwrap();
    }
    let mut engine = Engine::open(&scratch.0).unwrap();
    for case in fixture["cases"].as_array().unwrap() {
        if let Some(opened) = case["opened"].as_str() {
            engine
                .open_file(
                    mnemosyne_engine::FileTarget::path(opened),
                    Default::default(),
                )
                .unwrap();
            engine
                .editor_mut()
                .unwrap()
                .set_value(case["dirty"].as_str().unwrap().into());
        }
        let args: Vec<String> = serde_json::from_value(case["args"].clone()).unwrap();
        assert_eq!(
            mnemosyne::app::execute(&mut engine, &args),
            case["expected"]["stdout"],
            "{} retained",
            case["id"]
        );
        // Opening an independent process has no unsaved editor buffer. The dirty
        // active-file sequence above is checked against the retained engine only.
        if case.get("opened").is_none() {
            let output = Command::new(env!("CARGO_BIN_EXE_mnemosyne"))
                .args(&args)
                .env("MNEMOSYNE_VAULT", &scratch.0)
                .env_remove("MNEMOSYNE_SOCKET")
                .current_dir(&scratch.0)
                .output()
                .unwrap();
            assert_eq!(
                output.stdout,
                case["expected"]["stdout"].as_str().unwrap().as_bytes(),
                "{} process",
                case["id"]
            );
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
    assert_eq!(
        fs::read_to_string(scratch.0.join("Other.md")).unwrap(),
        fixture["initial_files"]["Other.md"]
    );
    // Runtime disable itself is still a separate lifecycle contract; this
    // verifies registration against a cold manager with the captured false flag.
    fs::write(
        scratch.0.join(CONFIG_DIR).join("core-plugins.json"),
        serde_json::to_vec(&json!({"word-count":false})).unwrap(),
    )
    .unwrap();
    engine = Engine::open(&scratch.0).unwrap();
    assert_eq!(
        mnemosyne::app::execute(&mut engine, &["wordcount".into(), "path=Other.md".into()]),
        fixture["disabled"]["stdout"]
    );
}
