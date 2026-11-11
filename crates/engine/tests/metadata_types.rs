use mnemosyne_core::CONFIG_DIR;
use mnemosyne_core::cache::CachedMetadata;
use mnemosyne_engine::{metadata_types::MetadataTypeManager, Engine};
use serde_json::{Map, Value};
use std::sync::atomic::{AtomicU64, Ordering};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(std::path::PathBuf);
impl Fixture {
    fn new(files: &Map<String, Value>, types: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "mnemosyne-types-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join(CONFIG_DIR)).unwrap();
        for (name, text) in files {
            std::fs::write(root.join(name), text.as_str().unwrap()).unwrap();
        }
        std::fs::write(root.join(CONFIG_DIR).join("types.json"), types).unwrap();
        Self(root)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn report() -> Value {
    serde_json::from_str(include_str!("fixtures/reference_metadata_types.json")).unwrap()
}
#[test]
fn actual_property_inference_counts_defaults_and_saved_type_bytes() {
    let data = report();
    let files = data["initial_files"].as_object().unwrap();
    let fixture = Fixture::new(files, data["initial_types"].as_str().unwrap());
    let engine = Engine::open(&fixture.0).unwrap();
    // Values are certified here; warm/cold asynchronous cache insertion chronology
    // remains separate from the manager's caller-supplied ordered rebuild rule.
    for (key, expected) in data["initial_state"]["properties"].as_object().unwrap() {
        assert_eq!(
            serde_json::to_value(engine.property_info(key).unwrap()).unwrap(),
            *expected,
            "{key}"
        );
    }
    for (key, expected) in data["initial_state"]["occurrences"].as_object().unwrap() {
        assert_eq!(
            engine.property_occurrences(key).unwrap(),
            expected.as_u64().unwrap() as usize,
            "{key}"
        );
    }
    for case in data["type_cases"].as_array().unwrap() {
        let fixture = Fixture::new(files, case["before_types"].as_str().unwrap());
        let mut engine = Engine::open(&fixture.0).unwrap();
        for change in case["changes"].as_array().unwrap() {
            engine
                .set_property_type(change[0].as_str().unwrap(), change[1].as_str().unwrap())
                .unwrap();
        }
        assert_eq!(
            std::fs::read_to_string(fixture.0.join(CONFIG_DIR).join("types.json")).unwrap(),
            case["after_types"].as_str().unwrap(),
            "{}",
            case["id"]
        );
    }
}
#[test]
fn manager_retains_first_assigned_display_name_and_raw_widgets_until_explicit_changes() {
    let config = serde_json::json!({"types":{"Foo":"number","foo":"checkbox","ALIASES":"date","Null":null,"Raw":false,"Object":{"widget":1}}});
    let mut manager = MetadataTypeManager::load(Some(&config));
    assert_eq!(manager.info("foo").unwrap().name, "Foo");
    assert_eq!(manager.info("foo").unwrap().widget, "number");
    assert_eq!(manager.info("aliases").unwrap().name, "aliases");
    assert_eq!(manager.info("aliases").unwrap().widget, "aliases");
    assert_eq!(manager.info("null").unwrap().widget, "text");
    assert_eq!(manager.info("raw").unwrap().widget, false);
    assert_eq!(
        manager.info("object").unwrap().widget,
        serde_json::json!({"widget":1})
    );
    manager.set_type("FOO", "checkbox").unwrap();
    manager.rebuild(std::iter::empty()).unwrap();
    assert_eq!(manager.config().unwrap()["types"]["Foo"], "checkbox");
    assert!(manager.config().unwrap()["types"].get("FOO").is_none());
}
#[test]
fn changed_metadata_retains_removed_properties_but_rebuild_uses_first_nonnull_type() {
    let first = CachedMetadata::parse("---\nTitle:\nold: before\nnumber: 1\n---\n");
    let second = CachedMetadata::parse("---\ntitle: false\nnumber: text\n---\n");
    let changed = CachedMetadata::parse("---\nTITLE: 2\n---\n");
    let mut manager = MetadataTypeManager::load(None);
    manager.rebuild([&first, &second]).unwrap();
    assert_eq!(manager.info("title").unwrap().name, "Title");
    assert_eq!(manager.info("title").unwrap().widget, "checkbox");
    assert_eq!(manager.info("number").unwrap().widget, "number");
    manager.cache_changed(&changed).unwrap();
    manager.recount([&changed, &second]).unwrap();
    assert_eq!(manager.info("title").unwrap().name, "TITLE");
    assert_eq!(manager.info("title").unwrap().widget, "number");
    assert_eq!(manager.info("old").unwrap().name, "old");
    assert_eq!(manager.occurrences("old").unwrap(), 0);
    manager.rebuild([&changed, &second]).unwrap();
    assert!(!manager.infos().iter().any(|info| info.name == "old"));
}
#[test]
fn external_types_json_changes_and_ignore_config_rebuild_are_loaded_without_reopening() {
    let fixture = Fixture::new(
        &serde_json::from_value(serde_json::json!({"Note.md":"---\nA: 1\n---\n"})).unwrap(),
        "{}",
    );
    let mut engine = Engine::open(&fixture.0).unwrap();
    assert_eq!(engine.property_info("a").unwrap().widget, "number");
    engine
        .set_config("userIgnoreFilters", serde_json::json!(["Note"]))
        .unwrap();
    assert_eq!(engine.property_occurrences("a").unwrap(), 0);
    assert!(!engine.property_infos().iter().any(|info| info.name == "A"));
    std::fs::write(
        fixture.0.join(CONFIG_DIR).join("types.json"),
        r#"{"types":{"Assigned":"date"}}"#,
    )
    .unwrap();
    engine.refresh_vault().unwrap();
    assert_eq!(engine.property_info("Assigned").unwrap().widget, "date");
}

#[test]
fn source_widget_inference_preserves_native_yaml_constructor_and_cycle_types() {
    // The constructors/identity are actual yaml_schema.json observations; widget
    // classification is the shipped XR rule, pending direct manager fixtures.
    let source="---\nts_date: !!timestamp 2024-01-02\nbin_data: !!binary SGVsbG8=\nset_data: !!set {red: null, blue: null}\nomap_data: !!omap [{one: x}, {two: 2}]\nnested_data: [{date: !!timestamp 2024-01-02}]\nquoted_date: '2024-01-02'\nstrings: [one, null]\nmixed: [one, !!timestamp 2024-01-02]\ncycle: &cycle {self: *cycle}\n---\n";
    let cache = CachedMetadata::parse(source);
    assert!(cache.native_frontmatter.is_some());
    let mut manager = MetadataTypeManager::load(None);
    manager.rebuild([&cache]).unwrap();
    for name in [
        "ts_date",
        "bin_data",
        "set_data",
        "omap_data",
        "nested_data",
        "mixed",
        "cycle",
    ] {
        assert_eq!(manager.info(name).unwrap().widget, "unknown", "{name}");
        assert_eq!(manager.occurrences(name).unwrap(), 1, "{name}");
    }
    assert_eq!(manager.info("quoted_date").unwrap().widget, "date");
    assert_eq!(manager.info("strings").unwrap().widget, "multitext");
    let null = CachedMetadata::parse("---\ncycle: null\n---\n");
    manager.rebuild([&null, &cache]).unwrap();
    assert_eq!(manager.info("cycle").unwrap().widget, "unknown");
    manager.cache_changed(&null).unwrap();
    assert_eq!(manager.info("cycle").unwrap().widget, "unknown");
}

#[test]
fn actual_typed_yaml_manager_properties_occurrences_and_enumeration_order() {
    let data: Value = serde_json::from_str(include_str!(
        "../../core/tests/fixtures/yaml_schema_followup.json"
    ))
    .unwrap();
    let files: Map<String, Value> = data["mutation_cases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|case| {
            (
                case["path"].as_str().unwrap().to_owned(),
                case["before"].clone(),
            )
        })
        .collect();
    let fixture = Fixture::new(&files, "{}");
    let mut engine = Engine::open(&fixture.0).unwrap();
    let order: Vec<String> = serde_json::from_value(data["manager"]["order"].clone()).unwrap();
    engine.import_cache_order(&order).unwrap();
    let properties = &data["manager"]["properties"];
    assert_eq!(
        engine
            .property_infos()
            .iter()
            .map(|p| p.name.as_str())
            .collect::<Vec<_>>(),
        properties
            .as_object()
            .unwrap()
            .values()
            .map(|p| p["name"].as_str().unwrap())
            .collect::<Vec<_>>()
    );
    for (key, expected) in properties.as_object().unwrap() {
        assert_eq!(
            serde_json::to_value(engine.property_info(key).unwrap()).unwrap(),
            *expected,
            "{key}"
        );
    }
    for (key, expected) in data["manager"]["occurrences"].as_object().unwrap() {
        assert_eq!(
            engine.property_occurrences(key).unwrap(),
            expected.as_u64().unwrap() as usize,
            "{key}"
        );
    }
}
