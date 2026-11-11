use mnemosyne_core::CONFIG_DIR;
use mnemosyne_engine::Engine;
use serde_json::{json, Value};
use std::{
    fs,
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "mn-dispatch-{}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
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
fn retained_dispatch_uses_captured_metadata_for_help_completion_and_version() {
    let scratch = Scratch::new();
    let mut engine = Engine::open(&scratch.0).unwrap();
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/application_dispatch.json")).unwrap();
    for case in fixture["help_cases"].as_array().unwrap() {
        let args: Vec<String> = serde_json::from_value(case["args"].clone()).unwrap();
        assert_eq!(
            mnemosyne::app::execute(&mut engine, &args),
            case["stdout"],
            "{} help",
            case["args"]
        );
    }
    let captures: Value =
        serde_json::from_str(include_str!("fixtures/command_metadata.json")).unwrap();
    let mut expected = serde_json::Map::new();
    for snapshot in captures["snapshots"].as_array().unwrap() {
        let rows: Value =
            serde_json::from_str(snapshot["expected_completions_subset"].as_str().unwrap())
                .unwrap();
        expected.extend(rows.as_object().unwrap().clone());
    }
    let output = mnemosyne::app::execute(&mut engine, &["__completions".into()]);
    let actual: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(actual.as_object().unwrap().len(), 37);
    for (name, row) in actual.as_object().unwrap() {
        assert_eq!(
            row.to_string(),
            expected[name].to_string(),
            "{name} ordered completion metadata"
        );
    }
    let version_args: Vec<String> =
        serde_json::from_value(fixture["version"]["args"].clone()).unwrap();
    assert_eq!(
        mnemosyne::app::execute(&mut engine, &version_args),
        fixture["version"]["stdout"]
    );
    let output = Command::new(env!("CARGO_BIN_EXE_mnemosyne"))
        .args(&version_args)
        .env_remove("MNEMOSYNE_SOCKET")
        .current_dir(&scratch.0)
        .output()
        .unwrap();
    assert_eq!(
        output.stdout,
        fixture["version"]["stdout"].as_str().unwrap().as_bytes()
    );
    assert!(output.stderr.is_empty());
    assert_eq!(output.status.code(), Some(0));
    let output = Command::new(env!("CARGO_BIN_EXE_mnemosyne"))
        .args(["help", "create"])
        .env("MNEMOSYNE_VAULT", &scratch.0)
        .env_remove("MNEMOSYNE_SOCKET")
        .current_dir(&scratch.0)
        .output()
        .unwrap();
    assert_eq!(
        output.stdout,
        fixture["help_cases"][0]["stdout"]
            .as_str()
            .unwrap()
            .as_bytes()
    );
    assert!(output.stderr.is_empty());
    assert_eq!(output.status.code(), Some(0));
}
#[test]
fn unknown_command_reaches_the_registry_and_suggests_close_names() {
    let scratch = Scratch::new();
    fs::write(scratch.0.join("Note.md"), "---\naliases: [Other]\n---\n").unwrap();
    let run = |command: &str| {
        Command::new(env!("CARGO_BIN_EXE_mnemosyne"))
            .arg(command)
            .env("MNEMOSYNE_VAULT", &scratch.0)
            .env_remove("MNEMOSYNE_SOCKET")
            .current_dir(&scratch.0)
            .output()
            .unwrap()
    };
    let output = run("alias");
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "Error: Command \"alias\" not found. Did you mean: aliases, links?\n"
    );
    assert_eq!(run("aliases").stdout, b"Other\n");
}
#[test]
fn implemented_search_registration_tracks_loaded_settings_and_keeps_foreign_literal_handlers() {
    let scratch = Scratch::new();
    fs::write(scratch.0.join("Note.md"), "alpha\n").unwrap();
    let mut engine = Engine::open(&scratch.0).unwrap();
    let search = vec!["search".into(), "query=alpha".into()];
    assert_eq!(mnemosyne::app::execute(&mut engine, &search), "Note.md\n");
    // Main-process selection consumes only a leading vault= argument; a
    // trailing parameter remains with the command and does not select a vault.
    assert_eq!(
        mnemosyne::app::execute(
            &mut engine,
            &["read".into(), "path=Note.md".into(), "vault=Missing".into()]
        ),
        "alpha\n"
    );
    fs::write(
        scratch.0.join(CONFIG_DIR).join("core-plugins.json"),
        r#"{"global-search":false}"#,
    )
    .unwrap();
    // Refreshing saved vault contents does not reload the plugin manager.
    assert_eq!(mnemosyne::app::execute(&mut engine, &search), "Note.md\n");
    engine.reload_core_plugin_settings().unwrap();
    // manager.enable() is additive; false desired flags do not disable live
    // wrappers. A cold manager starts with no enabled wrappers.
    assert_eq!(mnemosyne::app::execute(&mut engine, &search), "Note.md\n");
    engine = Engine::open(&scratch.0).unwrap();
    mnemosyne::app::execute(&mut engine, &["__completions".into()]);
    assert!(engine.commands.get("search").is_none());
    assert!(engine.commands.get("search:context").is_none());
    // An independently registered literal fallback keeps its handler identity
    // across built-in config synchronization, then exposes fallback on removal.
    let spec = mnemosyne::registry::CommandSpec::new("Fixture literal", None);
    engine
        .commands
        .register("read:constructor", spec, "builtin:version".into())
        .unwrap();
    assert_eq!(
        mnemosyne::app::execute(
            &mut engine,
            &["read:constructor".into(), "path=Note.md".into()]
        ),
        "1.14.4 (installer 1.14.4)\n"
    );
    engine.commands.unregister("read:constructor");
    assert_eq!(
        mnemosyne::app::execute(
            &mut engine,
            &["read:constructor".into(), "path=Note.md".into()]
        ),
        "alpha\n"
    );
    fs::write(
        scratch.0.join(CONFIG_DIR).join("core-plugins.json"),
        serde_json::to_vec(&json!(["global-search"])).unwrap(),
    )
    .unwrap();
    engine.reload_core_plugin_settings().unwrap();
    assert_eq!(mnemosyne::app::execute(&mut engine, &search), "Note.md\n");
    assert!(engine.commands.get("search").is_some());
}
