//! Shared-engine replay of existing twenty real local-plugin state snapshots.
//! Full plugin backends/global command catalogs are out of scope here; these checks compare enabled state, implemented local registrations,
//! configuration ownership, and exact observed save bytes.
use mnemosyne_core::CONFIG_DIR;
use mnemosyne_engine::{command_registry::CommandSpec, core_plugins::catalog, Engine};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};
static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let p = loop {
            let p = std::env::temp_dir().join(format!(
                "mn-owner-{}-{}",
                std::process::id(),
                NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::create_dir(&p) {
                Ok(()) => break p,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create exclusive fixture {}: {error}", p.display()),
            }
        };
        std::fs::create_dir(p.join(CONFIG_DIR)).unwrap();
        Self(p)
    }
    fn config(&self) -> PathBuf {
        self.0.join(CONFIG_DIR).join("core-plugins.json")
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn write(path: &Path, value: &Value) {
    std::fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
}
fn disabled() -> Value {
    Value::Object(
        catalog()
            .iter()
            .map(|descriptor| (descriptor.id.clone(), json!(false)))
            .collect(),
    )
}
#[test]
fn actual_twenty_owner_transitions_run_through_shared_engine() {
    let receipt: Value =
        serde_json::from_str(include_str!("fixtures/reference_core_plugins.json")).unwrap();
    let f = Fixture::new();
    std::fs::write(
        f.config(),
        receipt["cases"][0]["observed"]["saved"].as_str().unwrap(),
    )
    .unwrap();
    let mut engine = Engine::open(&f.0).unwrap();
    for row in receipt["cases"].as_array().unwrap() {
        let id = row["id"].as_str().unwrap();
        match id {
            "initial-disabled" | "external-edit-after-next-request" => {}
            "external-file-edit" => {
                std::fs::write(f.config(), row["observed"]["saved"].as_str().unwrap()).unwrap();
                engine.refresh_vault().unwrap();
            }
            "manager-enable-local" => {
                engine.reload_core_plugin_settings().unwrap();
                engine.save_core_plugin_config().unwrap();
            }
            "manager-enable-false-retains-live" => {
                std::fs::write(f.config(), row["observed"]["saved"].as_str().unwrap()).unwrap();
                engine.reload_core_plugin_settings().unwrap();
            }
            "save-retained-live-flags" => engine.save_core_plugin_config().unwrap(),
            _ if row.get("input_config").is_some() => {
                for descriptor in catalog() {
                    engine.disable_core_plugin(&descriptor.id);
                }
                write(&f.config(), &row["input_config"]);
                write(
                    &f.0.join(CONFIG_DIR).join("core-plugins-migration.json"),
                    &row["input_migration"],
                );
                engine.reload_core_plugin_settings().unwrap();
                engine.save_core_plugin_config().unwrap();
            }
            _ if id.starts_with("disable-again-") => {
                assert!(engine.disable_core_plugin(&id["disable-again-".len()..]));
                engine.save_core_plugin_config().unwrap();
            }
            _ if id.starts_with("disable-") => {
                assert!(engine.disable_core_plugin(&id["disable-".len()..]));
                engine.save_core_plugin_config().unwrap();
            }
            _ if id.starts_with("enable-") => {
                assert!(engine.enable_core_plugin(&id["enable-".len()..]));
                engine.save_core_plugin_config().unwrap();
            }
            _ => panic!("unknown fixture step {id}"),
        }
        let observed = &row["observed"];
        assert_eq!(
            Value::Object(
                engine
                    .core_plugin_state()
                    .flags()
                    .iter()
                    .map(|(id, enabled)| (id.clone(), json!(enabled)))
                    .collect()
            ),
            observed["enabled"],
            "{id} enabled"
        );
        assert_eq!(
            serde_json::to_value(engine.core_plugin_settings().configured()).unwrap(),
            observed["config"],
            "{id} config"
        );
        assert_eq!(
            json!(engine
                .core_plugin_state()
                .file_creators()
                .collect::<Vec<_>>()),
            observed["creators"],
            "{id} creators"
        );
        let native_local: Vec<_> = observed["cli"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .filter(|name| {
                *name == "daily"
                    || name.starts_with("daily:")
                    || *name == "templates"
                    || name.starts_with("template:")
            })
            .collect();
        assert_eq!(
            engine
                .core_plugin_state()
                .local_cli_handlers()
                .collect::<Vec<_>>(),
            native_local,
            "{id} retained local registrations"
        );
        assert_eq!(
            std::fs::read_to_string(f.config()).unwrap(),
            observed["saved"].as_str().unwrap(),
            "{id} bytes"
        );
    }
    let cold = Engine::open(&f.0).unwrap();
    assert_eq!(
        cold.core_plugin_state().flags(),
        engine.core_plugin_state().flags()
    );
    assert!(!engine.enable_core_plugin("not-a-core-plugin"));
    assert!(!engine.disable_core_plugin("not-a-core-plugin"));
}
#[test]
fn debounced_saves_are_idempotent_and_do_not_replace_foreign_handlers() {
    let f = Fixture::new();
    write(&f.config(), &disabled());
    let mut engine = Engine::open(&f.0).unwrap();
    assert!(engine.enable_core_plugin_at("global-search", 100));
    engine
        .commands
        .register(
            "search",
            CommandSpec::new("builtin", None),
            "builtin:search".into(),
        )
        .unwrap();
    engine
        .commands
        .register(
            "search:context",
            CommandSpec::new("foreign", None),
            "community:foreign".into(),
        )
        .unwrap();
    assert!(engine.disable_core_plugin_at("global-search", 200));
    assert!(engine.commands.get("search").is_none());
    assert!(engine.commands.get("search:context").is_some());
    assert!(engine.enable_core_plugin_at("templates", 300));
    assert!(!engine.enable_core_plugin_at("templates", 400)); // No-op never extends deadline.
    assert!(!engine.process_core_plugin_timers(599).unwrap());
    assert!(engine.process_core_plugin_timers(600).unwrap());
    assert!(!engine.process_core_plugin_timers(601).unwrap());
    let persisted: Value = serde_json::from_slice(&std::fs::read(f.config()).unwrap()).unwrap();
    assert_eq!(persisted["templates"], true);
    assert_eq!(persisted["global-search"], false);
    assert!(Engine::open(&f.0)
        .unwrap()
        .is_core_plugin_enabled("templates"));
}
#[test]
fn missing_startup_defaults_schedule_canonical_save_and_caught_errors_keep_live_config() {
    let f = Fixture::new();
    write(&f.config(), &json!({"extra":7}));
    let mut engine = Engine::open(&f.0).unwrap();
    assert_eq!(
        std::fs::read_to_string(f.config()).unwrap(),
        "{\"extra\":7}"
    );
    assert!(engine
        .poll_core_plugin_config_at(Instant::now() + Duration::from_secs(1))
        .unwrap());
    let document: Value = serde_json::from_slice(&std::fs::read(f.config()).unwrap()).unwrap();
    assert_eq!(document["extra"], 7);
    assert_eq!(document.as_object().unwrap().len(), 31);
    std::fs::remove_file(f.config()).unwrap();
    std::fs::create_dir(f.config()).unwrap();
    assert!(engine.disable_core_plugin("templates"));
    // writeJson swallows adapter failure after manager.config has advanced.
    engine.save_core_plugin_config().unwrap();
    assert_eq!(
        engine.core_plugin_settings().configured()["templates"],
        false
    );
    assert!(f.config().is_dir());
}
