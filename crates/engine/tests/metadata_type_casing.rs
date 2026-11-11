//! Integration of pinned native casing evidence with property identity. The
//! lower mappings are actual Node/Electron observations; manager composition
//! here additionally verifies case collisions and retained display names.
use mnemosyne_engine::{
    json_units::{units, JsonUnits},
    metadata_types::{lower_units, MetadataTypeManager},
};
use serde_json::Value;
fn vec(value: &Value) -> Vec<u16> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n.as_u64().unwrap() as u16)
        .collect()
}
#[test]
fn native_context_mappings_address_one_manager_identity_and_preserve_first_display_name() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../core/tests/fixtures/lowercase_native.json"
    ))
    .unwrap();
    for context in fixture["contexts"].as_array().unwrap() {
        let original = vec(&context["input"]);
        let lower = vec(&context["output"]);
        assert_eq!(lower_units(&original).unwrap(), lower);
        let mut manager = MetadataTypeManager::load(None);
        manager.set_type_units(&original, &units("number")).unwrap();
        manager.rebuild(std::iter::empty()).unwrap();
        assert_eq!(manager.info_units(&lower).unwrap().name, original);
        assert_eq!(
            manager.assigned_widget_units(&lower).unwrap(),
            Some(&JsonUnits::String(units("number")))
        );
        manager.set_type_units(&lower, &units("checkbox")).unwrap();
        manager.rebuild(std::iter::empty()).unwrap();
        assert_eq!(manager.info_units(&original).unwrap().name, original);
        assert_eq!(
            manager.info_units(&original).unwrap().widget,
            JsonUnits::String(units("checkbox"))
        );
        assert_eq!(
            manager.infos_units().len(),
            4,
            "case variants must not create two assigned properties"
        );
    }
}
#[test]
fn expanded_and_contextual_yaml_names_share_lookup_and_occurrence_identity() {
    let cache =
        mnemosyne_core::cache::CachedMetadata::try_parse("---\nİ: true\nΟΣ: 1\nΟΣΑ: text\n---\n")
            .unwrap();
    let mut manager = MetadataTypeManager::load(None);
    manager.rebuild([&cache]).unwrap();
    assert_eq!(
        manager.info_units(&units("i\u{307}")).unwrap().name,
        units("İ")
    );
    assert_eq!(manager.occurrences_units(&units("i\u{307}")).unwrap(), 1);
    assert_eq!(manager.info_units(&units("ος")).unwrap().name, units("ΟΣ"));
    assert_eq!(manager.occurrences_units(&units("οσα")).unwrap(), 1);
    assert_eq!(
        manager.info_units(&units("οσ")).unwrap().widget,
        JsonUnits::String(units("text"))
    ); // final sigma is context-sensitive
}
