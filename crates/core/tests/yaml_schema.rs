//! Observed plugin API parseYaml/stringifyYaml outputs, not parser mocks.
use mnemosyne_core::yaml_graph;
use serde_json::{json, Value};
#[test]
fn public_yaml_graph_matches_25_native_typed_values_identity_results_and_errors() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/yaml_schema.json")).unwrap();
    let mut failures = Vec::new();
    for case in fixture["direct_cases"].as_array().unwrap() {
        let source = case["source"].as_str().unwrap();
        let expected = &case["result"];
        let actual = match yaml_graph::parse(source) {
            Err(error) => json!({"error":error}),
            Ok(document) => {
                let yaml = match document.stringify() {
                    Ok(text) => json!({"value":text}),
                    Err(error) => json!({"error":{"name":error.name,"message":error.message}}),
                };
                let json = match document.json(false) {
                    Ok(text) => json!({"value":text}),
                    Err(error) => json!({"error":{"name":error.name,"message":error.message}}),
                };
                json!({"descriptor":document.descriptor(),"yaml":yaml,"json":json})
            }
        };
        if actual != *expected {
            failures.push(format!(
                "{} actual {actual}\nexpected {expected}",
                case["id"]
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn yaml_warnings_match_actual_native_diagnostics_without_becoming_errors() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/yaml_schema.json")).unwrap();
    let mut failures = Vec::new();
    for case in fixture["direct_cases"].as_array().unwrap() {
        if let Ok(document) = yaml_graph::parse(case["source"].as_str().unwrap()) {
            let actual = serde_json::to_value(document.warnings).unwrap();
            if actual != case["warnings"] {
                failures.push(format!(
                    "{} warnings actual {actual}, expected {}",
                    case["id"], case["warnings"]
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn cache_matches_15_native_constructor_and_schema_objects() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/yaml_schema.json")).unwrap();
    let mut failures = Vec::new();
    assert_eq!(fixture["cases"].as_array().unwrap().len(), 15);
    for case in fixture["cases"].as_array().unwrap() {
        let source = case["files"][case["source"].as_str().unwrap()]
            .as_str()
            .unwrap();
        let cache = mnemosyne_core::cache::CachedMetadata::parse(source);
        let actual = serde_json::to_value(&cache).unwrap();
        let mut expected = case["cached_source_proof"]["cache"].clone();
        expected
            .as_object_mut()
            .unwrap()
            .retain(|_, value| !value.is_null());
        if actual != expected {
            failures.push(format!(
                "{} cache actual {actual}, expected {expected}",
                case["id"]
            ));
        }
        let descriptor = cache.native_frontmatter.as_ref().unwrap().descriptor();
        if descriptor != case["cached_source_proof"]["descriptor"] {
            failures.push(format!(
                "{} descriptor actual {descriptor}, expected {}",
                case["id"], case["cached_source_proof"]["descriptor"]
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn yaml_matches_13_unicode_duplicate_and_schema_matrix_diagnostics() {
    verify_diagnostics(include_str!("fixtures/yaml_schema_followup.json"), 13);
}
#[test]
fn yaml_matches_20_tag_directive_merge_and_radix_cases() {
    verify_diagnostics(include_str!("fixtures/yaml_schema_remaining.json"), 20);
}
fn verify_diagnostics(source: &str, count: usize) {
    let fixture: Value = serde_json::from_str(source).unwrap();
    let mut failures = Vec::new();
    assert_eq!(fixture["direct_cases"].as_array().unwrap().len(), count);
    for case in fixture["direct_cases"].as_array().unwrap() {
        let actual = match yaml_graph::parse(case["source"].as_str().unwrap()) {
            Ok(document) => {
                json!({"descriptor":document.descriptor(),"warnings":document.warnings})
            }
            Err(error) => json!({"error":error,"warnings":[]}),
        };
        if actual != case["result"] {
            failures.push(format!(
                "{} actual {actual}, expected {}",
                case["id"], case["result"]
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn metadata_worker_matches_8_actual_failure_and_hidden_cycle_results() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/yaml_worker_failure.json")).unwrap();
    assert_eq!(fixture["cases"].as_array().unwrap().len(), 8);
    for case in fixture["cases"].as_array().unwrap() {
        let built =
            mnemosyne_core::cache::CachedMetadata::try_parse(case["source"].as_str().unwrap());
        if case["expected"]["cache"].is_null() {
            assert!(
                built.is_err(),
                "{} must return no worker metadata",
                case["path"]
            );
        } else {
            let cache = built.unwrap();
            assert_eq!(
                serde_json::to_value(&cache).unwrap(),
                case["expected"]["cache"],
                "{} cache",
                case["path"]
            );
            assert_eq!(
                cache.native_frontmatter.as_ref().unwrap().descriptor(),
                case["expected"]["descriptor"],
                "{} typed identity",
                case["path"]
            );
        }
    }
}

#[test]
fn yaml_matches_3_symbol_projections_and_emoji_duplicate_results() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/yaml_array_callbacks.json")).unwrap();
    assert_eq!(fixture["direct_cases"].as_array().unwrap().len(), 3);
    for case in fixture["direct_cases"].as_array().unwrap() {
        let actual = match yaml_graph::parse(case["source"].as_str().unwrap()) {
            Err(error) => json!({"error":error,"warnings":[]}),
            Ok(document) => {
                let yaml = match document.stringify() {
                    Ok(text) => json!({"value":text}),
                    Err(error) => json!({"error":{"name":error.name,"message":error.message}}),
                };
                let output = match document.json_output(false) {
                    Ok(Some(text)) => json!({"value":text}),
                    Ok(None) => json!({}),
                    Err(error) => json!({"error":{"name":error.name,"message":error.message}}),
                };
                json!({"descriptor":document.descriptor(),"warnings":document.warnings,"yaml":yaml,"json":output})
            }
        };
        assert_eq!(actual, case["result"], "{}", case["id"]);
    }
}

#[test]
fn yaml_matches_6_lossless_string_date_and_coercion_boundaries() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/yaml_value_boundaries.json")).unwrap();
    assert_eq!(fixture["direct_cases"].as_array().unwrap().len(), 6);
    for case in fixture["direct_cases"].as_array().unwrap() {
        let actual = match yaml_graph::parse(case["source"].as_str().unwrap()) {
            Err(error) => json!({"error":error,"warnings":[]}),
            Ok(document) => match document.try_descriptor() {
                Err(error) => json!({"error":error,"warnings":[]}),
                Ok(descriptor) => {
                    let yaml = match document.stringify() {
                        Ok(text) => json!({"value":text}),
                        Err(error) => json!({"error":{"name":error.name,"message":error.message}}),
                    };
                    let output = match document.json_output(false) {
                        Ok(Some(text)) => json!({"value":text}),
                        Ok(None) => json!({}),
                        Err(error) => json!({"error":{"name":error.name,"message":error.message}}),
                    };
                    json!({"descriptor":descriptor,"warnings":document.warnings,"yaml":yaml,"json":output})
                }
            },
        };
        assert_eq!(actual, case["result"], "{}", case["id"]);
    }
}

#[test]
fn cache_json_matches_native_lone_surrogate_and_bom_bytes() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/yaml_surrogate_cache.json")).unwrap();
    let cache = mnemosyne_core::cache::CachedMetadata::try_parse(
        fixture["initial_files"]["Root.md"].as_str().unwrap(),
    )
    .unwrap();
    let expected: Vec<u16> = serde_json::from_value(fixture["cache_json_units"].clone()).unwrap();
    let actual = serde_json::to_string(&cache).unwrap();
    assert_eq!(actual.encode_utf16().collect::<Vec<_>>(), expected);
    let document = cache.native_frontmatter.as_ref().unwrap();
    assert_eq!(
        document
            .javascript_units(document.own_property("value").unwrap())
            .unwrap(),
        vec![0xd800]
    );
}
