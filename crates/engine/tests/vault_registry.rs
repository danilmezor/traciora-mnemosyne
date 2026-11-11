//! Shipped-main.js selection rules, not a native multi-vault lifecycle receipt.
use mnemosyne_engine::vault_registry::{HELP_VAULT, URI_SCHEME, 
    CliDispatchAction, RegistryError, VaultRegistry, VaultWindowState, CLI_DISABLED,
    VAULT_NOT_FOUND,
};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "mnemosyne-registry-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn directory(&self, name: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::create_dir_all(&path).unwrap();
        path
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn source_registry_normalization_filters_without_mutating_profile_files() {
    let fixture = Fixture::new();
    let vault = fixture.directory("Vault");
    let help = fixture.directory(HELP_VAULT);
    // existsSync is the source condition; it does not require a directory here.
    let file = fixture.0.join("FileVault");
    std::fs::write(&file, b"existing file").unwrap();
    let profile = fixture.0.join("registry.json");
    let raw = json!({"cli":true,"language":"fr","future":{"a":1},"vaults":{
        "kept":{"path":"other/../Vault","ts":12,"futureField":[1,2]},
        "missing":{"path":"Missing"},
        "help":{"path":help},
        "file":{"path":file}
    }});
    let bytes = serde_json::to_vec(&raw).unwrap();
    std::fs::write(&profile, &bytes).unwrap();
    std::fs::write(fixture.0.join("missing.json"), b"retained per-vault config").unwrap();
    fixture.directory("Partitions/vault-missing");
    let registry = VaultRegistry::from_profile(&profile, &fixture.0).unwrap();
    let entries: Vec<_> = registry.registrations().collect();
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.id.as_str())
            .collect::<Vec<_>>(),
        ["kept", "file"]
    );
    assert_eq!(entries[0].path, vault);
    assert_eq!(entries[0].metadata["futureField"], json!([1, 2]));
    assert_eq!(registry.document()["language"], "fr");
    assert_eq!(registry.document()["future"], json!({"a":1}));
    assert!(registry.cli_enabled());
    assert_eq!(std::fs::read(&profile).unwrap(), bytes);
    assert!(fixture.0.join("missing.json").exists());
    assert!(fixture.0.join("Partitions/vault-missing").exists());
}

#[test]
fn source_cli_selection_uses_first_root_and_name_match_and_no_explicit_fallback() {
    let fixture = Fixture::new();
    let outer = fixture.directory("Same");
    let nested = fixture.directory("Same/Nested");
    let same = fixture.directory("Elsewhere/Same");
    let mut registry = VaultRegistry::from_document(
        json!({"vaults":{
            "outer":{"path":outer},"nested":{"path":nested},"other":{"path":same}
        }}),
        &fixture.0,
        None,
    )
    .unwrap();
    assert_eq!(
        registry.find_cwd(&nested.join("sub/note.md")),
        Some("outer")
    );
    assert_eq!(
        registry.find_cwd(Path::new("Same/Nested/./sub/../note.md")),
        Some("outer")
    );
    assert_eq!(registry.find_selector("sAmE"), Some("outer"));
    assert_eq!(registry.find_selector("nested"), Some("nested"));
    assert_eq!(registry.find_selector("NESTED"), Some("nested")); // basename match
    assert_eq!(registry.find_selector("OUTER"), None); // IDs are exact
    assert_eq!(registry.find_selector(outer.to_str().unwrap()), None); // path is not a selector
    registry.set_window_state(
        "other",
        VaultWindowState {
            focus_time: Some(100),
            destroyed: false,
        },
    );
    assert_eq!(registry.select_cli(None, &nested), Some("outer"));
    assert_eq!(registry.select_cli(None, &fixture.0), Some("other"));
    assert_eq!(registry.select_cli(Some("missing"), &nested), None);
    // Source Be compares root+separator. A registered filesystem root does not
    // swallow every absolute cwd because its concatenated prefix is "//".
    let root =
        VaultRegistry::from_document(json!({"vaults":{"root":{"path":"/"}}}), &fixture.0, None)
            .unwrap();
    assert_eq!(root.find_cwd(Path::new("/")), Some("root"));
    assert_eq!(root.find_cwd(&fixture.0), None);
}

#[test]
fn source_live_focus_is_insertion_order_on_ties_and_undefined_times() {
    let fixture = Fixture::new();
    let a = fixture.directory("A");
    let b = fixture.directory("B");
    let c = fixture.directory("C");
    let mut registry = VaultRegistry::from_document(
        json!({"vaults":{
            "a":{"path":a,"ts":9999},"b":{"path":b,"ts":0},"c":{"path":c,"ts":1}
        }}),
        &fixture.0,
        None,
    )
    .unwrap();
    assert_eq!(registry.focused_vault(), None); // persisted ts is not live focusTime
    assert!(!registry.set_window_state("unknown", VaultWindowState::default()));
    registry.set_window_state("b", VaultWindowState::default());
    registry.set_window_state(
        "a",
        VaultWindowState {
            focus_time: Some(50),
            destroyed: false,
        },
    );
    assert_eq!(registry.focused_vault(), Some("b")); // 50 > undefined is false
    registry.set_window_state(
        "b",
        VaultWindowState {
            focus_time: Some(50),
            destroyed: false,
        },
    );
    assert_eq!(registry.focused_vault(), Some("b"));
    registry.set_window_state(
        "c",
        VaultWindowState {
            focus_time: Some(51),
            destroyed: false,
        },
    );
    assert_eq!(registry.focused_vault(), Some("c"));
    registry.set_window_state(
        "c",
        VaultWindowState {
            focus_time: Some(52),
            destroyed: true,
        },
    );
    assert_eq!(registry.focused_vault(), Some("b"));
}

#[test]
fn source_registry_json_fallback_and_explicit_unsupported_schemas() {
    let fixture = Fixture::new();
    let profile = fixture.0.join("registry.json");
    std::fs::write(&profile, b"{bad json").unwrap();
    let registry = VaultRegistry::from_profile(&profile, &fixture.0).unwrap();
    assert_eq!(registry.registrations().count(), 0);
    assert_eq!(registry.diagnostics.len(), 1);
    assert_eq!(std::fs::read(&profile).unwrap(), b"{bad json");
    for value in [json!(true), json!(0), json!("legacy")] {
        let registry = VaultRegistry::from_document(value, &fixture.0, None).unwrap();
        assert_eq!(registry.registrations().count(), 0);
    }
    assert!(matches!(
        VaultRegistry::from_document(json!([]), &fixture.0, None),
        Err(RegistryError::Schema)
    ));
    assert!(matches!(
        VaultRegistry::from_document(json!({"vaults":{"a":{}}}), &fixture.0, None),
        Err(RegistryError::Entry(_))
    ));
    assert!(matches!(
        VaultRegistry::from_document(json!({}), Path::new("relative"), None),
        Err(RegistryError::Cwd)
    ));
}

#[test]
fn source_main_last_argument_path_and_uri_precede_cli_gate_and_selector() {
    let fixture = Fixture::new();
    let vault = fixture.directory("vault");
    let outside = fixture.directory("Outside");
    let registry = VaultRegistry::from_document(
        json!({"cli":false,"vaults":{"id":{"path":vault}}}),
        &fixture.0,
        None,
    )
    .unwrap();
    let plan = registry.plan_cli_request(&["vault".into()], false, &outside);
    assert_eq!(plan.action, CliDispatchAction::OpenPath("vault".into()));
    assert!(!plan.open_chooser_first);
    // The identical token is not a path relative to client cwd; existsSync is
    // nevertheless evaluated against MAIN cwd, before disabled CLI handling.
    assert!(!outside.join("vault").exists());
    assert_eq!(
        registry
            .plan_cli_request(
                &["vault=missing".into(), format!("{URI_SCHEME}open?vault=vault")],
                false,
                &outside
            )
            .action,
        CliDispatchAction::ProcessUri(format!("{URI_SCHEME}open?vault=vault"))
    );
    assert_eq!(
        registry
            .plan_cli_request(&["vault".into(), "info=name".into()], false, &outside)
            .action,
        CliDispatchAction::Disabled
    );
    assert!(
        registry
            .plan_cli_request(&[], false, &outside)
            .open_chooser_first
    );
    let registry = VaultRegistry::from_document(
        json!({"cli":true,"vaults":{"id":{"path":vault}}}),
        &fixture.0,
        None,
    )
    .unwrap();
    assert_eq!(
        registry
            .plan_cli_request(&["vault=id".into()], true, &outside)
            .action,
        CliDispatchAction::Interactive {
            vault_id: Some("id".into())
        }
    );
    assert_eq!(
        registry
            .plan_cli_request(&["vault=missing".into(), "eval".into()], false, &vault)
            .action,
        CliDispatchAction::Execute {
            vault_id: None,
            argv: vec!["eval".into()]
        }
    );
    assert_eq!(
        registry
            .plan_cli_request(&["eval".into(), "vault=missing".into()], false, &vault)
            .action,
        CliDispatchAction::Execute {
            vault_id: Some("id".into()),
            argv: vec!["eval".into(), "vault=missing".into()]
        }
    );
}

#[test]
fn actual_fifteen_multivault_cli_selection_and_main_routing_outputs() {
    let fixture = Fixture::new();
    fixture.directory("vault/Inner");
    fixture.directory("area/vault");
    std::fs::write(fixture.0.join("vault/ParityReady.md"), b"owned").unwrap();
    let replace = |text: &str| text.replace("$RUN", fixture.0.to_str().unwrap());
    // Replacing the path token in serialized JSON keeps source unknown global
    // fields and entry metadata without inventing a portable native schema.
    let source = include_str!("fixtures/reference_vault_registry.json");
    let receipt: serde_json::Value = serde_json::from_str(&replace(source)).unwrap();
    let base_document = receipt["profile_after_open"].clone();
    for row in receipt["cases"].as_array().unwrap() {
        let id = row["id"].as_str().unwrap();
        let document = if id.starts_with("cli-disabled-") {
            receipt["profile_after_disable"].clone()
        } else {
            base_document.clone()
        };
        let mut registry = VaultRegistry::from_document(document, &fixture.0, None).unwrap();
        // Native Y insertion is the creation order (base, nested, duplicate).
        // Electron getAllWindows() returned the reverse order in this fixture.
        // Import values by registration ID rather than mistaking that API's
        // enumeration for main Y's ordering.
        if let Some(windows) = row["windows"].as_array() {
            let ids: Vec<_> = registry
                .registrations()
                .map(|entry| entry.id.clone())
                .collect();
            for id in ids {
                if let Some(window) = windows.iter().find(|w| w["id"].as_str() == Some(&id)) {
                    registry.set_window_state(
                        &id,
                        VaultWindowState {
                            focus_time: window["focus_time"].as_i64(),
                            destroyed: window["destroyed"].as_bool().unwrap(),
                        },
                    );
                }
            }
        }
        let args: Vec<String> = row["argv"]
            .as_array()
            .unwrap()
            .iter()
            .map(|arg| arg.as_str().unwrap().into())
            .collect();
        let plan = registry.plan_cli_request(&args, false, Path::new(row["cwd"].as_str().unwrap()));
        assert!(!plan.open_chooser_first, "{id}");
        let expected_stdout = row["process"]["stdout"].as_str().unwrap();
        assert_eq!(row["process"]["exit_code"], 0, "{id}");
        assert_eq!(row["process"]["stderr"], "", "{id}");
        match plan.action {
            CliDispatchAction::Execute {
                vault_id: Some(selected),
                argv,
            } => {
                assert_eq!(
                    Some(selected.as_str()),
                    row["selected"]["id"].as_str(),
                    "{id}"
                );
                let entry = registry
                    .registrations()
                    .find(|entry| entry.id == selected)
                    .unwrap();
                assert_eq!(
                    entry.path.to_str(),
                    row["selected"]["path"].as_str(),
                    "{id}"
                );
                assert_eq!(argv[0], "eval", "{id}");
            }
            CliDispatchAction::Execute { vault_id: None, .. } => {
                assert_eq!(expected_stdout, format!("{VAULT_NOT_FOUND}\n"), "{id}")
            }
            CliDispatchAction::Disabled => {
                assert_eq!(expected_stdout, format!("{CLI_DISABLED}\n"), "{id}")
            }
            CliDispatchAction::OpenPath(path) => {
                assert_eq!(expected_stdout, format!("Opened {path}\n"), "{id}")
            }
            CliDispatchAction::ProcessUri(uri) => {
                assert_eq!(expected_stdout, format!("Processed URI {uri}\n"), "{id}")
            }
            action => panic!("unexpected {id} action {action:?}"),
        }
    }
}

#[test]
fn actual_four_injected_native_focus_events_follow_undefined_then_strict_greater() {
    let fixture = Fixture::new();
    fixture.directory("vault");
    fixture.directory("Second");
    let receipt: serde_json::Value = serde_json::from_str(
        &include_str!("fixtures/reference_focus_ghost.json")
            .replace("$RUN", fixture.0.to_str().unwrap()),
    )
    .unwrap();
    let mut registry = VaultRegistry::from_document(
        json!({"cli":true,"vaults":receipt["registry"]}),
        &fixture.0,
        None,
    )
    .unwrap();
    let ids: Vec<_> = registry
        .registrations()
        .map(|entry| entry.id.clone())
        .collect();
    for id in ids {
        registry.set_window_state(&id, VaultWindowState::default());
    }
    for (index, row) in receipt["focus_cases"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
    {
        if let Some(id) = row["emitted_in_vault"].as_str() {
            // Native listener sets Date.now(). Its exact milliseconds are not
            // exposed through the cached remote proxy; only observed strict
            // ordering is imported here, separately from ordinary OS delivery.
            assert_eq!(row["emit_result"], true);
            registry.set_window_state(
                id,
                VaultWindowState {
                    focus_time: Some(index as i64),
                    destroyed: false,
                },
            );
        }
        assert_eq!(
            registry.select_cli(None, &fixture.0),
            row["selected"]["id"].as_str(),
            "{}",
            row["id"]
        );
    }
}
