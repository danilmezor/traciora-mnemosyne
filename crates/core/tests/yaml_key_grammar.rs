use mnemosyne_core::{
    cache::CachedMetadata,
    yaml_graph,
    yaml_value::{Diagnostic, Node},
};
use serde_json::{json, Value};
fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/yaml_key_grammar.json")).unwrap()
}
fn snapshot(d: &yaml_graph::Document) -> Value {
    let fields:Vec<_>=d.own_fields_units().into_iter().map(|(key,id)|{
  let mut r=json!({"key_units":key,"value_type":match &d.nodes[id]{Node::String(_)|Node::Wtf16String(_)=>"string",Node::Number(_)=>"number",Node::Bool(_)=>"boolean",_=>"object"}});
  if matches!(d.nodes[id],Node::String(_)|Node::Wtf16String(_)){r["value_units"]=json!(d.javascript_units(id).unwrap())}r
 }).collect();
    json!({"fields":fields,"yaml":d.stringify().unwrap(),"json":d.json(false).unwrap()})
}
fn error(e: Diagnostic) -> Value {
    json!({"name":e.name,"message_utf16":e.message_units.as_ref().cloned().unwrap_or_else(||e.message.encode_utf16().collect()),"code":e.code,"pos":e.pos,"linePos":e.line_pos})
}
#[test]
fn twelve_actual_collection_keys_aliases_and_lossless_diagnostics() {
    for c in fixture()["direct_cases"].as_array().unwrap() {
        let actual = match yaml_graph::parse(c["source"].as_str().unwrap()) {
            Ok(d) => {
                json!({"result":snapshot(&d),"warnings":d.console_warnings.iter().map(|u|vec![String::from_utf16(u).unwrap()]).collect::<Vec<_>>()})
            }
            Err(e) => {
                if let Some(units) = &e.message_units {
                    let native = serde_json::to_string(&e).unwrap();
                    assert!(native.contains(&format!(
                        "\"message\":{}",
                        mnemosyne_core::yaml_value::quote_units(units)
                    )));
                }
                json!({"error":error(e),"warnings":[]})
            }
        };
        assert_eq!(actual, c["result"], "{}", c["id"])
    }
}
#[test]
fn twenty_five_actual_constructor_callbacks_keep_live_own_keys_and_saved_effects() {
    for c in fixture()["constructor_cases"].as_array().unwrap() {
        let before = c["before"].as_str().unwrap();
        let b = mnemosyne_core::frontmatter::processing_bounds(before).unwrap();
        let mut d = yaml_graph::parse(&before[b.from..b.to]).unwrap();
        assert_eq!(
            snapshot(&d),
            c["result"]["before"]["snapshot"],
            "before {}",
            c["id"]
        );
        let (key, value) = match c["operation"].as_str().unwrap() {
            "set-high" => (vec![0xd800], Some("high")),
            "set-low" => (vec![0xdc00], Some("low")),
            "set-replacement" => (vec![0xfffd], Some("replacement")),
            "set-ordinary" => ("added".encode_utf16().collect(), Some("ordinary")),
            "remove-high" => (vec![0xd800], None),
            _ => unreachable!(),
        };
        let count = d
            .replace_own_property_units(&key, value.map(|v| serde_yaml::Value::String(v.into())))
            .unwrap()
            .unwrap();
        assert_eq!(
            snapshot(&d),
            c["result"]["live"]["snapshot"],
            "live {}",
            c["id"]
        );
        let after = if count == 0 {
            before[b.to + 4..].to_owned()
        } else {
            format!(
                "{}{}{}",
                &before[..b.from],
                d.stringify().unwrap(),
                &before[b.to..]
            )
        };
        assert_eq!(after, c["after"].as_str().unwrap(), "saved {}", c["id"]);
        assert_eq!(
            serde_json::to_string(&CachedMetadata::try_parse(&after).unwrap()).unwrap(),
            c["result"]["cache_json"].as_str().unwrap(),
            "cache {}",
            c["id"]
        )
    }
}
#[test]
fn eleven_actual_key_metadata_caches() {
    for c in fixture()["cache_cases"].as_array().unwrap() {
        let d = CachedMetadata::try_parse(c["result"]["cached"].as_str().unwrap()).unwrap();
        assert_eq!(
            serde_json::to_string(&d).unwrap(),
            c["result"]["cache_json"].as_str().unwrap(),
            "{}",
            c["id"]
        )
    }
}
struct Scratch(std::path::PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
#[test]
fn eleven_actual_renames_preserve_empty_prefix_source_quirks() {
    let f = fixture();
    let s = Scratch(std::env::temp_dir().join(format!("mnem-key-grammar-{}", std::process::id())));
    std::fs::create_dir(&s.0).unwrap();
    for (path, source) in f["initial_files"].as_object().unwrap() {
        if path.starts_with("Grammar") || path == "Target.md" {
            std::fs::write(s.0.join(path), source.as_str().unwrap()).unwrap()
        }
    }
    let mut vault = mnemosyne_core::vault::Vault::open(&s.0).unwrap();
    vault.rename("Target.md", "Renamed.md").unwrap();
    for (path, source) in f["after_rename"].as_object().unwrap() {
        assert_eq!(
            std::fs::read_to_string(s.0.join(path)).unwrap(),
            source.as_str().unwrap(),
            "{path}"
        )
    }
}

#[test]
fn twelve_actual_constructor_keys_and_literal_alias_children() {
    let f: Value =
        serde_json::from_str(include_str!("fixtures/yaml_constructor_keys.json")).unwrap();
    for c in f["direct_cases"].as_array().unwrap() {
        let actual = match yaml_graph::parse(c["source"].as_str().unwrap()) {
            Ok(d) => {
                json!({"result":snapshot(&d),"warnings":d.console_warnings.iter().map(|u|vec![json!({"string_utf16":u})]).collect::<Vec<_>>()})
            }
            Err(e) => json!({"error":error(e),"warnings":[]}),
        };
        assert_eq!(actual, c["result"], "{}", c["id"]);
    }
}

#[test]
fn nine_actual_key_styles_order_empty_collections_and_raw_warning_units() {
    let f: Value =
        serde_json::from_str(include_str!("fixtures/yaml_key_flow_followup.json")).unwrap();
    for c in f["direct_cases"].as_array().unwrap() {
        let actual = match yaml_graph::parse(c["source"].as_str().unwrap()) {
            Ok(d) => {
                json!({"result":snapshot(&d),"warnings":d.console_warnings.iter().map(|u|vec![json!({"string_utf16":u})]).collect::<Vec<_>>()})
            }
            Err(e) => json!({"error":error(e),"warnings":[]}),
        };
        assert_eq!(actual, c["result"], "{}", c["id"]);
    }
}
