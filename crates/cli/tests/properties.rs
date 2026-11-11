use mnemosyne_core::CONFIG_DIR;
use mnemosyne::app;
use mnemosyne_engine::{EditorSelection, Engine, FileTarget, OpenOptions};
use serde_json::Value;
use std::{
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};
static NEXT: AtomicUsize = AtomicUsize::new(0);
struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn fixture(files: &Value, types: &Value) -> Fixture {
    let root = std::env::temp_dir().join(format!(
        "mnemosyne-properties-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(root.join(CONFIG_DIR)).unwrap();
    for (path, text) in files.as_object().unwrap() {
        let target = root.join(path);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(target, text.as_str().unwrap()).unwrap();
    }
    if let Some(types) = types.as_str() {
        std::fs::write(root.join(CONFIG_DIR).join("types.json"), types).unwrap();
    }
    Fixture(root)
}
fn invoke(engine: &mut Engine, args: &Value) -> String {
    app::execute(
        engine,
        &args
            .as_array()
            .unwrap()
            .iter()
            .map(|arg| arg.as_str().unwrap().to_owned())
            .collect::<Vec<_>>(),
    )
}
#[test]
fn properties_match_88_cached_read_outputs() {
    verify_reads(include_str!("fixtures/properties.json"), 88);
}
fn verify_reads(source: &str, count: usize) {
    let manifest: Value = serde_json::from_str(source).unwrap();
    let mut failures = Vec::new();
    assert_eq!(manifest["read_cases"].as_array().unwrap().len(), count);
    for case in manifest["read_cases"].as_array().unwrap() {
        let dir = fixture(&manifest["initial_files"], &manifest["initial_types"]);
        let mut engine = Engine::open(&dir.0).unwrap();
        let order: Vec<String> =
            serde_json::from_value(manifest["warm_creation_order"].clone()).unwrap();
        engine.import_cache_order(&order).unwrap();
        let actual: serde_json::Map<String, Value> = engine
            .property_infos()
            .into_iter()
            .map(|info| {
                (
                    info.name.to_lowercase(),
                    serde_json::json!({"name":info.name,"widget":info.widget}),
                )
            })
            .collect();
        assert_eq!(
            Value::Object(actual),
            case["state"]["properties"],
            "{} warm manager state",
            case["id"]
        );

        if let Some(path) = case["state"]["active"].as_str() {
            engine
                .open_file(FileTarget::path(path), OpenOptions::default())
                .unwrap();
        }
        let actual = invoke(&mut engine, &case["args"]);
        let expected = case["output"]["stdout"].as_str().unwrap();
        if actual != expected {
            failures.push(format!(
                "{}: actual {actual:?}, expected {expected:?}",
                case["id"]
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
#[test]
fn properties_match_38_raw_frontmatter_mutations_and_type_persistence() {
    verify_mutations(include_str!("fixtures/properties.json"), 38);
}
fn verify_mutations(source: &str, count: usize) {
    let manifest: Value = serde_json::from_str(source).unwrap();
    let mut failures = Vec::new();
    assert_eq!(manifest["mutation_cases"].as_array().unwrap().len(), count);
    for case in manifest["mutation_cases"].as_array().unwrap() {
        let dir = fixture(&case["before"]["tree"]["files"], &case["before"]["types"]);
        let mut engine = Engine::open(&dir.0).unwrap();
        if let Some(path) = case["before"]["state"]["active"].as_str() {
            engine
                .open_file(FileTarget::path(path), OpenOptions::default())
                .unwrap();
            let before = &case["before"]["state"]["editor"];
            engine
                .editor_mut()
                .unwrap()
                .set_value(before["text"].as_str().unwrap().into());
            engine
                .editor_mut()
                .unwrap()
                .set_selection(EditorSelection {
                    anchor: serde_json::from_value(before["anchor"].clone()).unwrap(),
                    head: serde_json::from_value(before["head"].clone()).unwrap(),
                })
                .unwrap();
        } else {
            assert_eq!(engine.active_file(), None);
        }
        let actual = invoke(&mut engine, &case["args"]);
        let expected = case["output"]["stdout"].as_str().unwrap();
        if actual != expected {
            failures.push(format!(
                "{} output: actual {actual:?}, expected {expected:?}",
                case["id"]
            ));
        }
        for (path, text) in case["after"]["tree"]["files"].as_object().unwrap() {
            let actual = std::fs::read_to_string(dir.0.join(path)).unwrap();
            let expected = text.as_str().unwrap();
            if actual != expected {
                failures.push(format!(
                    "{} {path}: actual {actual:?}, expected {expected:?}",
                    case["id"]
                ));
            }
        }
        let mut observed_paths: Vec<String> = std::fs::read_dir(&dir.0)
            .unwrap()
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                (!name.starts_with('.')).then_some(name)
            })
            .collect();
        observed_paths.sort();
        let mut expected_paths: Vec<String> = case["after"]["tree"]["files"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        expected_paths.sort();
        assert_eq!(
            observed_paths, expected_paths,
            "{} exact fixture file set",
            case["id"]
        );
        let actual = std::fs::read_to_string(dir.0.join(CONFIG_DIR).join("types.json")).ok();
        let expected = case["after"]["types"].as_str().map(str::to_owned);
        if actual != expected {
            failures.push(format!(
                "{} types: actual {actual:?}, expected {expected:?}",
                case["id"]
            ));
        }
        assert_eq!(
            serde_json::json!(engine.active_file()),
            case["after"]["state"]["active"],
            "{} active",
            case["id"]
        );
        let actual=serde_json::json!(engine.editor().map(|b|serde_json::json!({"text":b.text,"anchor":b.selection.anchor,"head":b.selection.head,"selection":b.selected_text().unwrap()})));
        let expected = &case["after"]["state"]["editor"];
        if actual != *expected {
            failures.push(format!(
                "{} editor: actual {actual}, expected {expected}",
                case["id"]
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn properties_match_27_unicode_nested_yaml_and_array_edge_observations() {
    verify_reads(include_str!("fixtures/properties_edges.json"), 12);
    verify_mutations(include_str!("fixtures/properties_edges.json"), 15);
}

#[test]
fn properties_match_11_active_dirty_prototype_and_parser_error_cases() {
    verify_mutations(include_str!("fixtures/properties_lifecycle.json"), 11);
}

#[test]
fn properties_match_83_numeric_key_and_multiline_cache_reads() {
    verify_schema_reads(
        include_str!("../../core/tests/fixtures/yaml_cache.json"),
        83,
    );
}

#[test]
fn properties_match_68_native_constructor_and_schema_reads() {
    verify_schema_reads(
        include_str!("../../core/tests/fixtures/yaml_schema.json"),
        68,
    );
}

fn verify_schema_reads(source: &str, expected_count: usize) {
    let manifest: Value = serde_json::from_str(source).unwrap();
    let mut failures = Vec::new();
    let mut count = 0;
    for case in manifest["cases"].as_array().unwrap() {
        let dir = fixture(&case["files"], &Value::Null);
        let mut engine = Engine::open(&dir.0).unwrap();
        for read in case["read_cases"].as_array().unwrap() {
            count += 1;
            let actual = invoke(&mut engine, &read["args"]);
            let expected = read["output"]["stdout"].as_str().unwrap();
            if actual != expected {
                failures.push(format!(
                    "{} {:?}: actual {actual:?}, expected {expected:?}",
                    case["id"], read["args"]
                ));
            }
        }
    }
    assert_eq!(count, expected_count);
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn properties_match_38_typed_mutations_and_19_tsv_outputs() {
    verify_typed_mutations(
        include_str!("../../core/tests/fixtures/yaml_schema_followup.json"),
        38,
        19,
    );
}
#[test]
fn properties_match_4_typed_root_array_mutations_and_2_tsv_outputs() {
    verify_typed_mutations(
        include_str!("../../core/tests/fixtures/yaml_schema_remaining.json"),
        4,
        2,
    );
}
fn verify_typed_mutations(source: &str, count: usize, read_count: usize) {
    let manifest: Value = serde_json::from_str(source).unwrap();
    let mut failures = Vec::new();
    assert_eq!(manifest["mutation_cases"].as_array().unwrap().len(), count);
    let mut reads = 0;
    for case in manifest["mutation_cases"].as_array().unwrap() {
        let files = serde_json::json!({case["path"].as_str().unwrap():case["before"]});
        let dir = fixture(&files, &Value::Null);
        let mut engine = Engine::open(&dir.0).unwrap();
        if let Some(read) = case.get("tsv") {
            reads += 1;
            let actual = invoke(&mut engine, &read["args"]);
            if actual != read["output"]["stdout"].as_str().unwrap() {
                failures.push(format!(
                    "{} TSV actual {actual:?}, expected {}",
                    case["id"], read["output"]["stdout"]
                ));
            }
        }
        let output = invoke(&mut engine, &case["args"]);
        let actual = std::fs::read_to_string(dir.0.join(case["path"].as_str().unwrap())).unwrap();
        if output != case["output"]["stdout"].as_str().unwrap()
            || actual != case["after"].as_str().unwrap()
        {
            failures.push(format!(
                "{} output {output:?}, expected {}; saved {actual:?}, expected {}",
                case["id"], case["output"]["stdout"], case["after"]
            ));
        }
    }
    assert_eq!(reads, read_count);
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn properties_match_16_metadata_worker_failure_and_hidden_cycle_outputs() {
    let manifest: Value = serde_json::from_str(include_str!(
        "../../core/tests/fixtures/yaml_worker_failure.json"
    ))
    .unwrap();
    let files: serde_json::Map<String, Value> = manifest["cases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|case| {
            (
                case["path"].as_str().unwrap().into(),
                case["source"].clone(),
            )
        })
        .collect();
    let dir = fixture(&Value::Object(files), &Value::Null);
    let mut engine = Engine::open(&dir.0).unwrap();
    let order: Vec<String> = serde_json::from_value(manifest["setup"]["order"].clone()).unwrap();
    engine.import_cache_order(&order).unwrap();
    let mut count = 0;
    for case in manifest["cases"].as_array().unwrap() {
        for read in case["reads"].as_array().unwrap().iter().take(2) {
            count += 1;
            assert_eq!(
                invoke(&mut engine, &read["args"]),
                read["output"]["stdout"].as_str().unwrap(),
                "{} {:?}",
                case["path"],
                read["args"]
            );
        }
    }
    assert_eq!(count, 16);
}

#[test]
fn properties_match_4_typed_array_index_and_length_callbacks() {
    verify_typed_mutations(
        include_str!("../../core/tests/fixtures/yaml_array_callbacks.json"),
        4,
        2,
    );
}

#[test]
fn properties_match_6_binary_index_and_date_json_callbacks() {
    verify_typed_mutations(
        include_str!("../../core/tests/fixtures/yaml_value_boundaries.json"),
        6,
        3,
    );
}

#[test]
fn properties_match_2_native_lone_surrogate_output_boundaries() {
    let manifest: Value = serde_json::from_str(include_str!(
        "../../core/tests/fixtures/yaml_surrogate_cache.json"
    ))
    .unwrap();
    let dir = fixture(&manifest["initial_files"], &Value::Null);
    let mut engine = Engine::open(&dir.0).unwrap();
    assert_eq!(manifest["property_cases"].as_array().unwrap().len(), 2);
    for case in manifest["property_cases"].as_array().unwrap() {
        assert_eq!(
            invoke(&mut engine, &case["args"]),
            case["expected"]["stdout"].as_str().unwrap()
        );
    }
}
#[test]
fn properties_match_6_native_lone_key_file_and_global_formats() {
    let manifest: Value =
        serde_json::from_str(include_str!("fixtures/properties_lossless_keys.json")).unwrap();
    let dir = fixture(&manifest["initial_files"], &Value::Null);
    let mut engine = Engine::open(&dir.0).unwrap();
    let order: Vec<String> =
        serde_json::from_value(manifest["setup"]["cache_order"].clone()).unwrap();
    engine.import_cache_order(&order).unwrap();
    // Native fixture creates these files after the app is ready; source changed
    // events update unassigned display names, independently of cold rebuild.
    for path in &order {
        engine.apply_metadata_changed_event(path).unwrap();
    }
    let cases = manifest["property_cases"].as_array().unwrap();
    assert_eq!(cases.len(), 6);
    for case in cases {
        assert_eq!(
            invoke(&mut engine, &case["args"]),
            case["expected"]["stdout"].as_str().unwrap()
        );
    }
}
