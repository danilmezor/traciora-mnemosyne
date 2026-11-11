use mnemosyne_core::CONFIG_DIR;
use mnemosyne_engine::{settings, Engine};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(std::path::PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn fixture(app: &Value, appearance: &Value) -> Fixture {
    let root = std::env::temp_dir().join(format!(
        "mnemosyne-settings-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(root.join(CONFIG_DIR)).unwrap();
    for (name, data) in [("app", app), ("appearance", appearance)] {
        std::fs::write(
            root.join(CONFIG_DIR).join(format!("{name}.json")),
            serde_json::to_vec(data).unwrap(),
        )
        .unwrap();
    }
    Fixture(root)
}
#[test]
fn source_settings_merge_migration_clone_and_partition_preserve_unknown_values() {
    let dir = fixture(
        &json!({"theme":"light","editorFontFamily":"Migrated","unknown":{"nested":[1,2]},"newFileLocation":"current"}),
        &json!({"theme":"dark","accentColor":"red","appearanceUnknown":false}),
    );
    let mut engine = Engine::open(&dir.0).unwrap();
    assert_eq!(engine.get_config("theme"), "light");
    assert_eq!(engine.get_config("textFontFamily"), "Migrated");
    assert!(engine.get_config_optional("editorFontFamily").is_none());
    let mut copy = engine.get_config_value("unknown").unwrap();
    copy["nested"][0] = json!(99);
    assert_eq!(engine.get_config("unknown")["nested"][0], 1);
    engine.set_config("theme", json!(null)).unwrap();
    assert!(engine.get_config("theme").is_null());
    engine.unset_config("theme").unwrap();
    assert_eq!(engine.get_config("theme"), "system");
    let app: Value =
        serde_json::from_slice(&std::fs::read(dir.0.join(CONFIG_DIR).join("app.json")).unwrap()).unwrap();
    let appearance: Value =
        serde_json::from_slice(&std::fs::read(dir.0.join(CONFIG_DIR).join("appearance.json")).unwrap())
            .unwrap();
    assert_eq!(app["appearanceUnknown"], false);
    assert_eq!(app["unknown"], json!({"nested":[1,2]}));
    assert!(app.get("accentColor").is_none());
    assert_eq!(appearance["accentColor"], "red");
    assert_eq!(appearance["textFontFamily"], "Migrated");
    // reloadConfig retains legacy names; the font migration is startup-only.
    std::fs::write(
        dir.0.join(CONFIG_DIR).join("app.json"),
        r#"{"editorFontFamily":"ReloadOnly"}"#,
    )
    .unwrap();
    engine.refresh_vault().unwrap();
    assert_eq!(engine.get_config("editorFontFamily"), "ReloadOnly");
}
#[test]
fn source_font_migration_keeps_existing_target_even_null_and_drops_falsy_old_value() {
    for app in [
        json!({"editorFontFamily":"Old","textFontFamily":null}),
        json!({"editorFontFamily":""}),
        json!({"editorFontFamily":false}),
    ] {
        let dir = fixture(&app, &json!({}));
        let engine = Engine::open(&dir.0).unwrap();
        assert!(engine.get_config_optional("editorFontFamily").is_none());
        if app.get("textFontFamily").is_some() {
            assert!(engine.get_config("textFontFamily").is_null());
        } else {
            assert_eq!(engine.get_config("textFontFamily"), "");
        }
    }
    assert_eq!(
        settings::enumerable(&json!({"3":3,"1":1,"01":"text"}))
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["1", "3", "01"]
    );
}

#[test]
fn actual_complete_defaults_load_migration_and_saved_app_appearance_bytes() {
    let report: Value =
        serde_json::from_str(include_str!("fixtures/reference_settings.json")).unwrap();
    for row in report["cases"].as_array().unwrap() {
        let dir = fixture(&json!({}), &json!({}));
        std::fs::write(
            dir.0.join(CONFIG_DIR).join("app.json"),
            row["input_app"].as_str().unwrap(),
        )
        .unwrap();
        std::fs::write(
            dir.0.join(CONFIG_DIR).join("appearance.json"),
            row["input_appearance"].as_str().unwrap(),
        )
        .unwrap();
        let engine = Engine::open(&dir.0).unwrap();
        let observed = &row["observed"];
        assert_eq!(
            serde_json::to_value(engine.explicit_config()).unwrap(),
            observed["config"],
            "{}",
            row["id"]
        );
        for (key, value) in observed["values"].as_object().unwrap() {
            assert_eq!(engine.get_config(key), value, "{} {key}", row["id"]);
        }
        engine.save_config().unwrap();
        for name in ["app", "appearance"] {
            assert_eq!(
                std::fs::read_to_string(dir.0.join(CONFIG_DIR).join(format!("{name}.json"))).unwrap(),
                observed[name].as_str().unwrap(),
                "{} {name}",
                row["id"]
            );
        }
    }
    let dir = fixture(&json!({}), &json!({}));
    let engine = Engine::open(&dir.0).unwrap();
    assert_eq!(
        serde_json::to_value(engine.config_defaults()).unwrap(),
        report["defaults_and_clones"]["values"]
    );
    assert!(engine.get_config_value("unknown-no-default").is_none());
    let mut copy = engine.get_config_value("mobileToolbarCommands").unwrap();
    copy.as_array_mut().unwrap().push(json!("MUTATED"));
    assert!(!engine
        .get_config("mobileToolbarCommands")
        .as_array()
        .unwrap()
        .contains(&json!("MUTATED")));
}
#[test]
fn observed_failed_app_write_is_swallowed_while_memory_and_appearance_advance() {
    let report: Value =
        serde_json::from_str(include_str!("fixtures/reference_settings.json")).unwrap();
    let observed = &report["injected_app_write_failure"];
    let dir = fixture(&json!({"object":{"a":1}}), &json!({}));
    let mut engine = Engine::open(&dir.0).unwrap();
    engine.unset_config("object").unwrap();
    // An owned directory at the file destination provides a real deterministic
    // adapter-write failure instead of changing process-wide permissions.
    std::fs::remove_file(dir.0.join(CONFIG_DIR).join("app.json")).unwrap();
    std::fs::create_dir(dir.0.join(CONFIG_DIR).join("app.json")).unwrap();
    engine.set_config("theme", json!("dark")).unwrap();
    engine.set_config("unknown", json!(123)).unwrap();
    assert_eq!(
        serde_json::to_value(engine.explicit_config()).unwrap(),
        observed["config"]
    );
    assert!(dir.0.join(CONFIG_DIR).join("app.json").is_dir());
    assert_eq!(
        std::fs::read_to_string(dir.0.join(CONFIG_DIR).join("appearance.json")).unwrap(),
        observed["appearance"].as_str().unwrap()
    );
    assert!(!std::fs::read_dir(dir.0.join(CONFIG_DIR))
        .unwrap()
        .any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp")));
}
