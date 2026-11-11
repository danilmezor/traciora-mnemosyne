use mnemosyne::{
    builtins::{metadata_catalog, Availability},
    registry::{frame_output, CommandRegistry},
    search::Params,
};
use serde_json::Value;

#[test]
fn every_literal_metadata_entry_preserves_usage_and_order() {
    let raw: Value =
        serde_json::from_str(include_str!("../data/command_metadata.json")).unwrap();
    let catalog = metadata_catalog();
    assert_eq!(catalog.commands().len(), 117);
    for (index, (metadata, raw)) in catalog
        .commands()
        .iter()
        .zip(raw["commands"].as_array().unwrap())
        .enumerate()
    {
        assert_eq!(metadata.source_declaration_index, index);
        assert_eq!(metadata.name, raw["command"]);
        assert_eq!(metadata.spec.usage(), raw["usage"], "{}", metadata.name);
        let keys: Vec<_> = metadata
            .spec
            .flags
            .iter()
            .flat_map(|flags| flags.keys())
            .map(String::as_str)
            .collect();
        let expected: Vec<_> = raw["flag_order"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect();
        assert_eq!(keys, expected, "{}", metadata.name);
    }
    assert!(catalog.get("__commands").unwrap().spec.flags.is_none());
    assert_eq!(
        catalog.get("tasks").unwrap().spec.description,
        "List tasks in the vault"
    );
    assert_eq!(
        catalog.get("search").unwrap().availability,
        Availability::CorePlugin("global-search".into())
    );
    assert_eq!(
        catalog.get("sync").unwrap().policy_feature.as_deref(),
        Some("sync")
    );
    assert!(catalog.get("web").unwrap().desktop_required);
}

#[test]
fn catalog_does_not_advertise_missing_implementations_or_disabled_owners() {
    let catalog = metadata_catalog();
    let mut registry = CommandRegistry::new();
    let added = catalog
        .register_implemented(
            &mut registry,
            false,
            |_| false,
            |command| matches!(command.name.as_str(), "read" | "search" | "eval").then_some(()),
        )
        .unwrap();
    assert_eq!(added, ["read"]);
    assert_eq!(registry.names().collect::<Vec<_>>(), ["read"]);
    catalog
        .register_plugin(&mut registry, "global-search", |command| {
            (command.name == "search").then_some(())
        })
        .unwrap();
    assert!(registry.get("search").is_some());
    assert!(registry.get("search:open").is_none());
    let mut desktop = CommandRegistry::new();
    catalog
        .register_implemented(
            &mut desktop,
            true,
            |owner| owner == "global-search",
            |command| matches!(command.name.as_str(), "read" | "search" | "eval").then_some(()),
        )
        .unwrap();
    assert!(desktop.get("read").is_some());
    assert!(desktop.get("search").is_some());
    assert!(desktop.get("eval").is_some());
}

#[test]
fn metadata_help_and_completions_match_all_live_builtin_snapshots() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/command_metadata.json")).unwrap();
    let catalog = metadata_catalog();
    let snapshots = fixture["snapshots"].as_array().unwrap();
    assert_eq!(snapshots.len(), 15);
    for snapshot in snapshots {
        let label = snapshot["label"].as_str().unwrap();
        let mut registry = CommandRegistry::new();
        // Reproduce recorded lifecycle insertion order rather than asserting
        // that static declaration order defines every live plugin schedule.
        for name in snapshot["active_handlers"].as_array().unwrap() {
            let name = name.as_str().unwrap();
            let metadata = catalog.get(name).unwrap();
            registry.register(name, metadata.spec.clone(), ()).unwrap();
        }
        for recorded in snapshot["metadata"].as_array().unwrap() {
            let name = recorded["command"].as_str().unwrap();
            let spec = &registry.get(name).unwrap().spec;
            assert_eq!(spec.description, recorded["description"], "{label}: {name}");
            let mut flags = serde_json::Map::new();
            if let Some(declared) = &spec.flags {
                for (key, flag) in declared {
                    flags.insert(key.clone(), Value::Object(flag.metadata.clone()));
                }
            }
            let flags = if spec.flags.is_some() {
                Value::Object(flags)
            } else {
                Value::Null
            };
            // String comparison also checks both flag and attribute order.
            assert_eq!(
                flags.to_string(),
                recorded["flags"].to_string(),
                "{label}: {name}"
            );
        }
        assert_eq!(
            frame_output(&registry.help(&Params::new())),
            snapshot["expected_help"],
            "{label}"
        );
        let completions = registry.completions();
        let mut subset = serde_json::Map::new();
        for name in snapshot["completion_names"].as_array().unwrap() {
            let name = name.as_str().unwrap();
            if let Some(value) = completions.get(name) {
                subset.insert(name.into(), value.clone());
            }
        }
        assert_eq!(
            frame_output(&Value::Object(subset).to_string()),
            snapshot["expected_completions_subset"],
            "{label}"
        );
        for recorded in snapshot["focused_help"].as_array().unwrap() {
            let argv: Vec<_> = recorded["args"]
                .as_array()
                .unwrap()
                .iter()
                .map(|arg| arg.as_str().unwrap().to_owned())
                .collect();
            let prepared = registry.prepare(&argv).unwrap();
            assert_eq!(
                frame_output(&registry.help(&prepared.params)),
                recorded["stdout"],
                "{label}: {argv:?}"
            );
            assert_eq!(recorded["stderr"], "");
            assert_eq!(recorded["exit_code"], 0);
        }
    }
}
