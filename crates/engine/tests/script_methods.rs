//! Property delivery here is injected to replay actual native method decisions;
//! real Node inspection/SDK wrapper installation is verified separately.
use mnemosyne_engine::script_methods::{call, FileMethod, Properties};
use mnemosyne_runtime_protocol::{
    HostError, HostResult, HostValue, JsReference, JsValueId, ReferenceKind,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;

#[derive(Default)]
struct Reader {
    fields: BTreeMap<String, HostValue>,
    getters: Vec<String>,
    fail: Option<String>,
    log: Vec<String>,
}
impl Properties for Reader {
    fn get(&mut self, _receiver: &HostValue, name: &[u16]) -> HostResult {
        let name = String::from_utf16(name).unwrap();
        if self.getters.contains(&name) {
            self.log.push(name.clone());
        }
        if self.fail.as_ref() == Some(&name) {
            return Err(HostError::new("Error", "owned getter fault"));
        }
        Ok(self
            .fields
            .get(&name)
            .cloned()
            .unwrap_or(HostValue::Undefined))
    }
}
fn reference(index: u64) -> HostValue {
    HostValue::Reference(JsReference {
        id: JsValueId { context: 17, index },
        kind: ReferenceKind::Object,
    })
}
fn describe(value: HostValue, input: &HostValue) -> Value {
    match value {
        HostValue::Null => json!({"type":"null"}),
        HostValue::Undefined => json!({"type":"undefined"}),
        HostValue::Utf16(units) => json!({"type":"string","units":units}),
        HostValue::Number(value) => {
            json!({"type":"number","value":mnemosyne_core::vault_search::ecmascript_number(value)})
        }
        HostValue::Boolean(value) => json!({"type":"boolean","value":value}),
        HostValue::Reference(value) => {
            json!({"type":"object","same_input":HostValue::Reference(value)==*input,"array":false})
        }
        other => panic!("unexpected method return {other:?}"),
    }
}
#[test]
fn rust_selectors_preserve_16_native_borrowed_receiver_decisions_and_read_order() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/plugin_api_receivers.json")).unwrap();
    let mut count = 0;
    for case in fixture["cases"].as_array().unwrap() {
        let id = case["id"].as_str().unwrap();
        let method = match case["method"].as_str().unwrap() {
            "toString" => FileMethod::ToString,
            "getShortName" => FileMethod::GetShortName,
            "isRoot" => FileMethod::IsRoot,
            _ => continue,
        };
        let mut reader = Reader::default();
        let input = reference(99);
        let receiver = match id {
            "short-null" | "toString-null" => HostValue::Null,
            "short-undefined" => HostValue::Undefined,
            _ => reference(1),
        };
        let fields = match id {
            "short-md" => vec![
                ("extension", HostValue::from("md")),
                ("basename", HostValue::from("BASE")),
                ("name", HostValue::from("unused")),
            ],
            "short-uppercase" => vec![
                ("extension", HostValue::from("MD")),
                ("basename", HostValue::from("unused")),
                ("name", HostValue::from("Name.MD")),
            ],
            "short-object" => vec![
                ("extension", HostValue::from("md")),
                ("basename", input.clone()),
            ],
            "toString-text" => vec![("path", HostValue::from("Folder/A.md"))],
            "toString-lone" => vec![("path", HostValue::Utf16(vec![0xd800]))],
            "toString-object" => vec![("path", input.clone())],
            "toString-number" => vec![("path", HostValue::Number(3.))],
            "root-borrowed" => vec![("path", HostValue::from("/"))],
            "root-boxed" => vec![("path", reference(2))],
            _ => Vec::new(),
        };
        reader.fields = fields
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect();
        reader.getters = match id {
            "short-md" | "short-uppercase" => vec!["extension", "basename", "name"],
            "short-extension-throws" => vec!["extension"],
            "toString-text" | "root-borrowed" => vec!["path"],
            _ => Vec::new(),
        }
        .into_iter()
        .map(String::from)
        .collect();
        if id == "short-extension-throws" {
            reader.fail = Some("extension".into());
        }
        match call(method, &receiver, &mut reader) {
            Ok(value) => assert_eq!(describe(value, &input), case["result"], "{id}"),
            Err(error) => assert_eq!(
                json!({"name":error.name,"message":error.message}),
                case["error"],
                "{id}"
            ),
        }
        assert_eq!(json!(reader.log), case["log"], "{id}");
        count += 1;
    }
    assert_eq!(count, 16);
}
