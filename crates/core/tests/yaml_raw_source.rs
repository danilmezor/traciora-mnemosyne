use mnemosyne_core::{
    yaml_graph,
    yaml_value::{Diagnostic, Node, NodeId},
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/yaml_raw_source.json")).unwrap()
}
fn descriptor(d: &yaml_graph::Document, id: NodeId, seen: &mut BTreeMap<NodeId, usize>) -> Value {
    match &d.nodes[id] {
        Node::Null => return json!({"type":"null"}),
        Node::String(_) | Node::Wtf16String(_) => {
            return json!({"type":"string","units":d.javascript_units(id).unwrap()})
        }
        Node::Number(v) => {
            return json!({"type":"number","string":mnemosyne_core::vault_search::ecmascript_number(v.as_f64().unwrap())})
        }
        Node::Bool(v) => return json!({"type":"boolean","value":v}),
        _ => {}
    }
    if let Some(id) = seen.get(&id) {
        return json!({"ref":id});
    }
    let index = seen.len();
    seen.insert(id, index);
    match &d.nodes[id] {
        Node::Array(v) => {
            json!({"type":"Array","id":index,"values":v.iter().map(|id|descriptor(d,*id,seen)).collect::<Vec<_>>()})
        }
        _ => {
            json!({"type":"Object","id":index,"fields":d.fields_units(id).unwrap().iter().map(|(key,id)|json!({"key_units":key,"value":descriptor(d,*id,seen)})).collect::<Vec<_>>()})
        }
    }
}
fn error(e: &Diagnostic) -> Value {
    let mut v = json!({"name":e.name,"message_utf16":e.message_units.clone().unwrap_or_else(||e.message.encode_utf16().collect())});
    if let Some(value) = &e.code {
        v["code"] = json!(value)
    }
    if let Some(value) = &e.pos {
        v["pos"] = json!(value)
    }
    if let Some(value) = &e.line_pos {
        v["linePos"] = json!(value)
    }
    v
}
#[test]
fn thirty_three_actual_raw_sources_preserve_values_keys_aliases_and_positions() {
    for c in fixture()["direct_cases"].as_array().unwrap() {
        let input: Vec<u16> = serde_json::from_value(c["source_units"].clone()).unwrap();
        let actual = match yaml_graph::parse_units(&input) {
            Ok(d) => {
                let mut warnings: Vec<_> = d.warnings.iter().map(|e| vec![error(e)]).collect();
                warnings.extend(
                    d.console_warnings
                        .iter()
                        .map(|u| vec![json!({"string_utf16":u})]),
                );
                json!({"received_units":input,"descriptor":descriptor(&d,d.root,&mut BTreeMap::new()),"yaml":{"units":d.stringify_units().unwrap()},"json":{"units":d.json(false).unwrap().encode_utf16().collect::<Vec<_>>()},"warnings":warnings})
            }
            Err(e) => json!({"received_units":input,"error":error(&e),"warnings":[]}),
        };
        assert_eq!(actual, c["result"], "{}", c["id"]);
    }
}
