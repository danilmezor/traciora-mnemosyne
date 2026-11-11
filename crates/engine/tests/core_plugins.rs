//! Desired flag resolution tests. Actual lifecycle verification is separate.
use mnemosyne_core::CONFIG_DIR;
use mnemosyne_engine::{
    core_plugins::{catalog, CorePluginConfigError, CorePluginSettings, CorePluginState},
    Engine,
};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
static NEXT: AtomicU64 = AtomicU64::new(0);

#[test]
fn actual_descriptor_catalogue_and_source_default_truthiness_resolution() {
    let receipt: Value =
        serde_json::from_str(include_str!("fixtures/reference_settings.json")).unwrap();
    let observed = receipt["plugin_descriptors"].as_array().unwrap();
    assert_eq!(observed.len(), 30);
    assert_eq!(catalog().len(), observed.len());
    for (descriptor, native) in catalog().iter().zip(observed) {
        assert_eq!(descriptor.id, native["id"].as_str().unwrap());
        assert_eq!(
            descriptor.default_on,
            native["defaultOn"].as_bool().unwrap_or(false)
        );
        assert_eq!(
            CorePluginSettings::default().enabled(&descriptor.id),
            descriptor.default_on
        );
    }
    let config = json!({"daily-notes":false,"templates":[],"canvas":0,"bases":"false","graph":{},"unknown":42});
    let flags = CorePluginSettings::resolve(Some(&config), None).unwrap();
    assert!(!flags.enabled("daily-notes"));
    assert!(flags.enabled("templates"));
    assert!(!flags.enabled("canvas"));
    assert!(flags.enabled("bases"));
    assert!(flags.enabled("graph"));
    assert!(!flags.enabled("unknown"));
    assert_eq!(flags.save_document()["unknown"], 42);
    assert_eq!(flags.save_document()["templates"], true);
}

#[test]
fn source_legacy_array_applies_old_catalogue_only_and_preserves_new_migration_flags() {
    let legacy = json!([
        "daily-notes",
        "templates",
        "canvas",
        "starred",
        "does-not-exist"
    ]);
    let migration = json!({"canvas":false,"bases":false,"properties":false,"bookmarks":false,"graph":true,"custom":7});
    let flags = CorePluginSettings::resolve(Some(&legacy), Some(&migration)).unwrap();
    assert!(flags.enabled("daily-notes"));
    assert!(flags.enabled("templates"));
    assert!(!flags.enabled("graph")); // legacy membership overwrites migration
    assert!(!flags.enabled("canvas")); // a newer plugin is not in the legacy list
    assert!(!flags.enabled("bases"));
    assert!(!flags.enabled("bookmarks"));
    assert!(!flags.configured().contains_key("starred")); // no current descriptor
    assert_eq!(flags.save_document()["custom"], 7);
    assert!(CorePluginSettings::resolve(Some(&json!([])), None)
        .unwrap()
        .enabled("canvas"));
    assert!(matches!(
        CorePluginSettings::resolve(Some(&json!({"hasOwnProperty":false})), None),
        Err(CorePluginConfigError::PropertyShadow)
    ));
    assert!(matches!(
        CorePluginSettings::resolve(Some(&legacy), Some(&json!([]))),
        Err(CorePluginConfigError::ArrayMigration)
    ));
}

#[test]
fn actual_engine_external_flags_and_additive_manager_reload() {
    let path = std::env::temp_dir().join(format!(
        "mnemosyne-plugins-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(path.clone());
    std::fs::create_dir_all(path.join(CONFIG_DIR)).unwrap();
    let config = path.join(CONFIG_DIR).join("core-plugins.json");
    std::fs::write(&config, br#"{"daily-notes":true,"templates":false}"#).unwrap();
    let mut engine = Engine::open(&path).unwrap();
    assert!(engine.is_core_plugin_enabled("daily-notes"));
    assert!(!engine.is_core_plugin_enabled("templates"));
    std::fs::write(&config, br#"{"daily-notes":false,"templates":true}"#).unwrap();
    engine.refresh_vault().unwrap();
    assert!(engine.is_core_plugin_enabled("daily-notes"));
    assert!(!engine.is_core_plugin_enabled("templates"));
    engine.reload_core_plugin_settings().unwrap();
    assert!(engine.is_core_plugin_enabled("daily-notes"));
    assert!(engine.is_core_plugin_enabled("templates"));
    let cold = Engine::open(&path).unwrap();
    assert!(!cold.is_core_plugin_enabled("daily-notes"));
    assert!(cold.is_core_plugin_enabled("templates"));
}

#[test]
fn actual_twenty_retained_plugin_states_resources_and_save_bytes() {
    let receipt: Value =
        serde_json::from_str(include_str!("fixtures/reference_core_plugins.json")).unwrap();
    let mut state = CorePluginState::default();
    let all_false: serde_json::Map<String, Value> = catalog()
        .iter()
        .map(|p| (p.id.clone(), json!(false)))
        .collect();
    let mut settings = CorePluginSettings::resolve(Some(&Value::Object(all_false)), None).unwrap();
    for row in receipt["cases"].as_array().unwrap() {
        let id = row["id"].as_str().unwrap();
        match id {
            "initial-disabled"
            | "external-file-edit"
            | "external-edit-after-next-request"
            | "save-retained-live-flags" => {}
            "manager-enable-local" | "manager-enable-false-retains-live" => {
                settings = CorePluginSettings::resolve(
                    row.get("input_config"),
                    row.get("input_migration"),
                )
                .unwrap();
                state.apply_enable_configuration(&settings);
            }
            _ if row.get("input_config").is_some() => {
                for descriptor in catalog() {
                    state.record_disable(&descriptor.id);
                }
                settings = CorePluginSettings::resolve(
                    row.get("input_config"),
                    row.get("input_migration"),
                )
                .unwrap();
                state.apply_enable_configuration(&settings);
            }
            _ if id.starts_with("disable-again-") => {
                assert!(state.record_disable(&id["disable-again-".len()..]));
            }
            _ if id.starts_with("disable-") => {
                assert!(state.record_disable(&id["disable-".len()..]));
            }
            _ if id.starts_with("enable-") => {
                assert!(state.record_enable(&id["enable-".len()..]));
            }
            _ => panic!("unrecognized fixture operation {id}"),
        }
        let observed = &row["observed"];
        assert_eq!(
            Value::Object(
                state
                    .flags()
                    .iter()
                    .map(|(key, value)| (key.clone(), json!(value)))
                    .collect()
            ),
            observed["enabled"],
            "{id} flags"
        );
        assert_eq!(
            serde_json::to_value(state.file_creators().collect::<Vec<_>>()).unwrap(),
            observed["creators"],
            "{id} creators"
        );
        let local: Vec<_> = observed["cli"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .filter(|name| {
                *name == "templates"
                    || name.starts_with("template:")
                    || *name == "daily"
                    || name.starts_with("daily:")
            })
            .collect();
        assert_eq!(
            state.local_cli_handlers().collect::<Vec<_>>(),
            local,
            "{id} CLI registration order"
        );
        if id == "save-retained-live-flags"
            || id.starts_with("disable-")
            || id.starts_with("enable-")
            || row.get("input_migration").is_some()
            || id == "missing-local-defaults"
        {
            assert_eq!(
                serde_json::to_string_pretty(&state.save_document(&settings)).unwrap(),
                observed["saved"].as_str().unwrap(),
                "{id} saved config"
            );
        }
    }
    assert!(!state.record_enable("not-a-core-plugin"));
}
