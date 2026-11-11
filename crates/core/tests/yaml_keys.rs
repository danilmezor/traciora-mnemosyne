use mnemosyne_core::{cache::CachedMetadata, yaml_graph, yaml_value::Node};
use serde_json::{json, Value};
struct Scratch(std::path::PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/yaml_key_boundaries.json")).unwrap()
}
#[test]
fn actual_public_keys_keep_identity_order_errors_and_projections() {
    for case in fixture()["direct_cases"].as_array().unwrap() {
        let actual = match yaml_graph::parse(case["source"].as_str().unwrap()) {
            Err(error) => json!({"error":error}),
            Ok(document) => {
                let fields:Vec<_>=document.own_fields_units().into_iter().map(|(key,id)|{
    let mut row=json!({"key_units":key,"value_type":match document.nodes[id]{Node::Null|Node::Object(_)|Node::ObjectWithUtf16Keys(_)=>"object",Node::String(_)|Node::Wtf16String(_)=>"string",_=>unreachable!()}});
    if matches!(document.nodes[id],Node::String(_)|Node::Wtf16String(_)){row["value_units"]=json!(document.javascript_units(id).unwrap())}row
   }).collect();
                json!({"fields":fields,"yaml":document.stringify().unwrap(),"json":document.json(false).unwrap()})
            }
        };
        assert_eq!(actual, case["result"], "{}", case["id"]);
    }
}
#[test]
fn actual_metadata_json_and_key_paths_remain_lossless() {
    for case in fixture()["cache_cases"].as_array().unwrap() {
        let source = case["result"]["cached"].as_str().unwrap();
        let cache = CachedMetadata::try_parse(source);
        let actual = cache
            .as_ref()
            .ok()
            .map(|cache| serde_json::to_string(cache).unwrap())
            .unwrap_or("null".into());
        assert_eq!(
            actual,
            case["result"]["cache_json"].as_str().unwrap(),
            "{}",
            case["path"]
        );
    }
}
#[test]
fn actual_frontmatter_callbacks_preserve_other_key_identities() {
    for case in fixture()["callbacks"].as_array().unwrap() {
        let before = case["before"].as_str().unwrap();
        let bounds = mnemosyne_core::frontmatter::processing_bounds(before).unwrap();
        let mut document = yaml_graph::parse(&before[bounds.from..bounds.to]).unwrap();
        let (key, value) = match case["id"].as_str().unwrap() {
            "set-lone" => (vec![0xd800], Some("changed")),
            "set-other" => (vec![0xd801], Some("added")),
            "remove-lone" => (vec![0xd800], None),
            _ => unreachable!(),
        };
        document
            .replace_own_property_units(&key, value.map(|v| serde_yaml::Value::String(v.into())))
            .unwrap();
        let after = format!(
            "{}{}{}",
            &before[..bounds.from],
            document.stringify().unwrap(),
            &before[bounds.to..]
        );
        assert_eq!(after, case["after"].as_str().unwrap(), "{}", case["id"]);
    }
}
#[test]
fn actual_rename_updates_nested_lone_key_references() {
    let fixture = fixture();
    let dir =
        Scratch(std::env::temp_dir().join(format!("mnemosyne-yaml-keys-{}", std::process::id())));
    std::fs::create_dir(&dir.0).unwrap();
    for (path, source) in fixture["initial_files"].as_object().unwrap() {
        std::fs::write(dir.0.join(path), source.as_str().unwrap()).unwrap()
    }
    let mut vault = mnemosyne_core::vault::Vault::open(&dir.0).unwrap();
    vault.rename("Target.md", "Renamed.md").unwrap();
    for (path, source) in fixture["after_rename"].as_object().unwrap() {
        if path == "Callback.md" {
            continue;
        }
        assert_eq!(
            std::fs::read_to_string(dir.0.join(path)).unwrap(),
            source.as_str().unwrap(),
            "{path}"
        )
    }
}
