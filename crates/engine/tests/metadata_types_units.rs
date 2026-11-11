//! Source-derived lossless manager checks; captured native manager cases are
//! added separately when the combined key oracle finishes.
use mnemosyne_core::CONFIG_DIR;
use mnemosyne_engine::{
    json_units::{units, JsonUnits},
    metadata_types::MetadataTypeManager,
    Engine,
};
use std::sync::atomic::{AtomicU64, Ordering};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(std::path::PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "mnemo-key-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join(CONFIG_DIR)).unwrap();
        Self(root)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
#[test]
fn separate_lone_replacement_and_literal_keys_survive_set_save_reload() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.0.join(CONFIG_DIR).join("types.json"),
        r#"{"types":{"\ud800":"number","�":"checkbox","\\ud800":"date","A\ud800":"text"}}"#,
    )
    .unwrap();
    let mut engine = Engine::open(&fixture.0).unwrap();
    assert_eq!(
        engine.property_info_units(&[0xd800]).unwrap().widget,
        JsonUnits::String(units("number"))
    );
    assert_eq!(engine.property_info("�").unwrap().widget, "checkbox");
    assert_eq!(engine.property_info("\\ud800").unwrap().widget, "date");
    engine
        .set_property_type_units(&[97, 0xd800], &units("multitext"))
        .unwrap();
    assert_eq!(
        engine.property_info_units(&[65, 0xd800]).unwrap().name,
        vec![65, 0xd800]
    );
    assert_eq!(
        engine.property_info_units(&[97, 0xd800]).unwrap().widget,
        JsonUnits::String(units("multitext"))
    );
    let raw = std::fs::read_to_string(fixture.0.join(CONFIG_DIR).join("types.json")).unwrap();
    assert!(raw.contains("A\\ud800"));
    assert!(!raw.ends_with('\n'));
    let reopened = Engine::open(&fixture.0).unwrap();
    assert_eq!(
        reopened.property_infos_units(),
        engine.property_infos_units()
    );
}
#[test]
fn typed_widget_and_case_identity_do_not_drop_escaped_units() {
    let value=JsonUnits::parse(r#"{"types":{"Title":"\udc00","TITLE":"number","10":"text","2":"checkbox","\ud800":false}}"#).unwrap();
    let mut manager = MetadataTypeManager::load_units(Some(&value));
    assert_eq!(
        manager.info_units(&units("title")).unwrap().name,
        units("Title")
    );
    assert_eq!(
        manager.info_units(&units("title")).unwrap().widget,
        JsonUnits::String(vec![0xdc00])
    );
    assert!(manager.try_info_utf8("title").unwrap().is_none());
    assert_eq!(
        &manager.infos_units()[..2]
            .iter()
            .map(|info| info.name.clone())
            .collect::<Vec<_>>(),
        &vec![units("2"), units("10")]
    );
    manager
        .set_type_units(&units("TITLE"), &units("number"))
        .unwrap();
    manager.rebuild(std::iter::empty()).unwrap();
    let config = manager.config_units().pretty();
    assert!(config.contains("\"Title\": \"number\""));
    assert!(config.contains("\"\\ud800\": false"));
}
fn descriptor(value: &JsonUnits) -> serde_json::Value {
    if let JsonUnits::String(units) = value {
        serde_json::json!({"units":units})
    } else {
        value.to_value().unwrap()
    }
}
fn manager_snapshot(
    manager: &MetadataTypeManager,
    expected: &serde_json::Value,
) -> serde_json::Value {
    let all=manager.infos_units().iter().map(|info|serde_json::json!({"key_units":mnemosyne_engine::metadata_types::lower_units(&info.name).unwrap(),"info":{"name_units":info.name,"widget":descriptor(&info.widget)}})).collect::<Vec<_>>();
    let occurrences = manager
        .occurrence_entries_units()
        .iter()
        .map(|(key, count)| serde_json::json!({"key_units":key,"count":count}))
        .collect::<Vec<_>>();
    let types=expected["types"].as_array().unwrap().iter().map(|row|{
        let name=row["key_units"].as_array().unwrap().iter().map(|v|v.as_u64().unwrap() as u16).collect::<Vec<_>>();let info=manager.info_units(&name).unwrap();
        serde_json::json!({"key_units":name,"assigned_widget":manager.assigned_widget_units(&name).unwrap().map(descriptor),"info":{"name_units":info.name,"widget":descriptor(&info.widget)}})
    }).collect::<Vec<_>>();
    serde_json::json!({"all":all,"occurrences":occurrences,"types":types})
}
#[test]
fn actual_manager_lone_keys_occurrences_assignment_load_events_and_exact_config_bytes() {
    let receipt: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/reference_metadata_types_units.json")).unwrap();
    let paths = receipt["conditions"]["source_compatible_explicit_cache_rebuild_order"]
        .as_array()
        .unwrap();
    let caches = paths
        .iter()
        .filter_map(|path| {
            mnemosyne_core::cache::CachedMetadata::try_parse(
                receipt["after_rename"][path.as_str().unwrap()]
                    .as_str()
                    .unwrap(),
            )
            .ok()
        })
        .collect::<Vec<_>>();
    let mut manager = MetadataTypeManager::load_units(None);
    manager.rebuild(&caches).unwrap();
    assert_eq!(
        manager_snapshot(&manager, &receipt["manager_before"]),
        receipt["manager_before"]
    );
    for (name, widget) in [
        (vec![0xd800], "text"),
        (vec![0xfffd], "number"),
        (units("CASE"), "checkbox"),
    ] {
        manager.set_type_units(&name, &units(widget)).unwrap();
        manager.rebuild(&caches).unwrap();
    }
    assert_eq!(
        manager_snapshot(&manager, &receipt["manager_set"]),
        receipt["manager_set"]
    );
    assert_eq!(
        manager.config_units().pretty(),
        receipt["types_after_set"].as_str().unwrap()
    );
    let config = JsonUnits::parse(receipt["types_initial_config"].as_str().unwrap()).unwrap();
    manager.load_data_units(Some(&config), &caches).unwrap();
    assert_eq!(
        manager_snapshot(&manager, &receipt["manager_loaded"]),
        receipt["manager_loaded"]
    );
    manager
        .set_type_units(&units("other"), &units("text"))
        .unwrap();
    manager.rebuild(&caches).unwrap();
    assert_eq!(
        manager_snapshot(&manager, &receipt["manager_post_loaded_set"]),
        receipt["manager_post_loaded_set"]
    );
    assert_eq!(
        manager.config_units().pretty(),
        receipt["types_after_loaded_set"].as_str().unwrap()
    );
}
#[test]
fn deep_valid_widget_is_not_rejected_or_lost_by_load_clone_write_or_drop() {
    let nesting = 600;
    let source = format!(
        "{{\"types\":{{\"Deep\":{}0{}}}}}",
        "[".repeat(nesting),
        "]".repeat(nesting)
    );
    let config = JsonUnits::parse(&source).unwrap();
    let manager = MetadataTypeManager::load_units(Some(&config));
    let saved = manager.config_units();
    let restored = JsonUnits::parse(&saved.pretty()).unwrap();
    assert_eq!(saved, restored);
    let cloned = manager.info_units(&units("deep")).unwrap().widget.clone();
    assert_eq!(manager.info_units(&units("deep")).unwrap().widget, cloned);
    // Compact parsing/clone/drop stresses depth independently of pretty output size.
    let very_deep =
        JsonUnits::parse(&format!("{}0{}", "[".repeat(20000), "]".repeat(20000))).unwrap();
    assert_eq!(very_deep, very_deep.clone());
    drop(very_deep);
}
fn widget_shape(value: &JsonUnits) -> serde_json::Value {
    match value {
        JsonUnits::String(text) => serde_json::json!({"units":text}),
        JsonUnits::Number(n) => {
            serde_json::json!({"number":mnemosyne_core::vault_search::ecmascript_number(*n)})
        }
        JsonUnits::Array(_) => {
            let mut value = value;
            let mut depth = 0;
            while let JsonUnits::Array(a) = value {
                depth += 1;
                value = &a[0];
            }
            serde_json::json!({"array":true,"depth":depth})
        }
        _ => value.to_value().unwrap(),
    }
}
fn load_snapshot(manager: &MetadataTypeManager) -> serde_json::Value {
    let entries = |properties: Vec<mnemosyne_engine::metadata_types::PropertyInfoUnits>| {
        properties.iter().map(|info|serde_json::json!({"key_units":mnemosyne_engine::metadata_types::lower_units(&info.name).unwrap(),"name_units":info.name,"widget":widget_shape(&info.widget)})).collect::<Vec<_>>()
    };
    let assigned = manager.config_units();
    let assigned = if let Some(JsonUnits::Object(fields)) = assigned.property(&units("types")) {
        mnemosyne_engine::json_units::enumerate(fields).iter().map(|(name,widget)|serde_json::json!({"key_units":mnemosyne_engine::metadata_types::lower_units(name).unwrap(),"name_units":name,"widget":widget_shape(widget)})).collect::<Vec<_>>()
    } else {
        panic!("type config shape")
    };
    serde_json::json!({"assigned":assigned,"properties":entries(manager.infos_units())})
}
#[test]
fn actual_fourteen_config_loads_preserve_partial_failure_and_event_order() {
    let receipt: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/reference_metadata_type_loads.json")).unwrap();
    for case in receipt["cases"].as_array().unwrap() {
        let baseline = JsonUnits::parse(r#"{"types":{"Seed":"number"}}"#).unwrap();
        let mut manager = MetadataTypeManager::load_units(Some(&baseline));
        assert_eq!(
            load_snapshot(&manager),
            case["result"]["before"],
            "{} before",
            case["id"]
        );
        let config = JsonUnits::parse(case["source"].as_str().unwrap()).unwrap();
        let outcome = manager.load_data_units(Some(&config), std::iter::empty());
        assert_eq!(
            outcome
                .err()
                .map(|error| serde_json::to_value(error).unwrap())
                .unwrap_or(serde_json::Value::Null),
            case["result"]["error"],
            "{} error",
            case["id"]
        );
        assert_eq!(
            load_snapshot(&manager),
            case["result"]["immediate"],
            "{} immediate",
            case["id"]
        );
        assert_eq!(
            case["result"]["immediate"], case["result"]["after"],
            "{} observedstable",
            case["id"]
        );
    }
}
#[test]
fn actual_nineteen_json_parse_cases_match_success_units_and_syntax_errors() {
    let receipt: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/reference_metadata_type_loads.json")).unwrap();
    for case in receipt["diagnostics"].as_array().unwrap() {
        let source = case["source"].as_str().unwrap();
        match JsonUnits::parse(source) {
            Ok(JsonUnits::String(ref text)) => assert_eq!(
                serde_json::json!({"ok":true,"value":{"units":text}}),
                case["result"]
            ),
            Ok(_) => panic!("unexpected successful scalar {source:?}"),
            Err(error) => assert_eq!(
                serde_json::json!({"error":{"name":"SyntaxError","message":error.native_message(source).unwrap()}}),
                case["result"],
                "{source:?}"
            ),
        }
    }
}
#[test]
fn engine_reload_preserves_native_fault_partial_state_and_exposes_diagnostic() {
    let fixture = Fixture::new();
    let path = fixture.0.join(CONFIG_DIR).join("types.json");
    std::fs::write(&path, r#"{"types":{"Seed":"number"}}"#).unwrap();
    let mut engine = Engine::open(&fixture.0).unwrap();
    std::fs::write(
        &path,
        r#"{"types":{"hasOwnProperty":"text","Seed":"text"}}"#,
    )
    .unwrap();
    engine.reload_property_types();
    assert_eq!(engine.property_info("Seed").unwrap().widget, "number");
    assert!(engine
        .assigned_property_widget_units(&units("Seed"))
        .unwrap()
        .is_none());
    let error = engine.property_type_load_error().unwrap();
    assert_eq!(error.name, "TypeError");
    assert_eq!(error.message, "n.hasOwnProperty is not a function");
    // Native setType after a failed load starts with the empty assigned map.
    engine.set_property_type("After", "checkbox").unwrap();
    assert!(engine
        .assigned_property_widget_units(&units("aliases"))
        .unwrap()
        .is_none());
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "{\n  \"types\": {\n    \"After\": \"checkbox\"\n  }\n}"
    );
}
