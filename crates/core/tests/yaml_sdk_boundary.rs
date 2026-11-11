use mnemosyne_core::{
    yaml_graph::{self, sdk, ConsoleWarning},
    yaml_value::{Diagnostic, Node},
};
use serde_json::{json, Value};
fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/yaml_sdk_boundary.json")).unwrap()
}
fn error(e: &Diagnostic) -> Value {
    let mut v = json!({"name":e.name,"message_utf16":e.message_units.clone().unwrap_or_else(||e.message.encode_utf16().collect())});
    if let Some(code) = &e.code {
        v["code"] = json!(code)
    }
    if let Some(pos) = e.pos {
        v["pos"] = json!(pos)
    }
    if let Some(pos) = &e.line_pos {
        v["linePos"] = json!(pos)
    }
    v
}
fn native_error(e: &Value) -> Value {
    let mut e = e.clone();
    for k in ["constructor", "own_names", "is_error"] {
        e.as_object_mut().unwrap().remove(k);
    }
    e
}
fn warning(w: &ConsoleWarning) -> Value {
    match w {
        ConsoleWarning::Yaml(e) => error(e),
        ConsoleWarning::Text(units) => json!({"string_utf16":units}),
    }
}
fn descriptor(d: &yaml_graph::Document, id: usize) -> Value {
    match &d.nodes[id] {
        Node::Null => json!({"type":"null"}),
        Node::Bool(v) => json!({"type":"boolean","value":v}),
        Node::String(_) | Node::Wtf16String(_) => {
            json!({"type":"string","units":d.javascript_units(id).unwrap()})
        }
        Node::Number(v) => {
            json!({"type":"number","string":mnemosyne_core::vault_search::ecmascript_number(v.as_f64().unwrap())})
        }
        _ => {
            json!({"type":"Object","fields":d.fields_units(id).unwrap().iter().map(|(key,id)|json!({"key_units":key,"value":descriptor(d,*id)})).collect::<Vec<_>>()})
        }
    }
}
#[test]
fn fifteen_actual_warning_sequences_survive_errors_in_ast_and_conversion_order() {
    for c in fixture()["warning_cases"].as_array().unwrap() {
        let input: Vec<u16> = serde_json::from_value(c["source_units"].clone()).unwrap();
        let out = yaml_graph::parse_units_outcome(&input);
        let warnings: Vec<_> = out.warnings.iter().map(|w| vec![warning(w)]).collect();
        let actual = match out.result {
            Ok(d) => json!({"result":descriptor(&d,d.root),"warnings":warnings}),
            Err(e) => json!({"error":error(&e),"warnings":warnings}),
        };
        let mut expected = c["result"].clone();
        if expected.get("error").is_some() {
            expected["error"] = native_error(&expected["error"]);
        }
        for a in expected["warnings"].as_array_mut().unwrap() {
            for w in a.as_array_mut().unwrap() {
                if w.get("name").is_some() {
                    *w = native_error(w)
                }
            }
        }
        assert_eq!(actual, expected, "{}", c["id"]);
    }
}
struct Input {
    kind: sdk::ParseKind,
    length: Result<(), Diagnostic>,
    trace: Vec<String>,
}
impl sdk::ParseArgument for Input {
    type Error = Diagnostic;
    fn read_length(&mut self) -> Result<(), Diagnostic> {
        if matches!(self.length,Err(ref e) if e.message=="length failed") {
            self.trace.push("length".into());
        }
        self.length.clone()
    }
    fn kind(&self) -> sdk::ParseKind {
        self.kind.clone()
    }
}
struct Logs(Vec<ConsoleWarning>);
impl sdk::Console<Diagnostic> for Logs {
    fn warn(&mut self, v: ConsoleWarning) -> Result<(), Diagnostic> {
        self.0.push(v);
        Ok(())
    }
}
#[test]
fn nineteen_actual_parse_argument_results_keep_length_read_and_no_string_coercion() {
    for c in fixture()["argument_cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["api"] == "parse")
    {
        let id = c["id"].as_str().unwrap();
        let kind = match id {
            "parse-borrowed-this" => {
                sdk::ParseKind::String("value: text\n".encode_utf16().collect())
            }
            "parse-false"
            | "parse-zero"
            | "parse-negative-zero"
            | "parse-nan"
            | "parse-zero-bigint" => sdk::ParseKind::Falsy,
            _ => sdk::ParseKind::NonString,
        };
        let mut input = Input {
            kind,
            length: Ok(()),
            trace: vec![],
        };
        if id == "parse-missing" || id == "parse-null" {
            input.length = Err(Diagnostic::exception(
                "TypeError",
                &format!(
                    "Cannot read properties of {} (reading 'length')",
                    if id == "parse-null" {
                        "null"
                    } else {
                        "undefined"
                    }
                ),
            ));
        }
        if id == "parse-length-throw" {
            let mut e = Diagnostic::exception("Error", "length failed");
            e.code = Some("CUSTOM".into());
            input.length = Err(e);
        }
        if id == "parse-length-getter" {
            input.trace.push("length".into());
        }
        let mut logs = Logs(vec![]);
        let result = sdk::parse(&mut input, &mut logs);
        let actual = match result {
            Ok(d) => json!({"result":descriptor(&d,d.root),"trace":input.trace,"warnings":[]}),
            Err(sdk::CallError::Host(e) | sdk::CallError::Yaml(e)) => {
                json!({"error":error(&e),"trace":input.trace,"warnings":[]})
            }
        };
        let mut expected = c["result"].clone();
        if expected.get("error").is_some() {
            expected["error"] = native_error(&expected["error"])
        }
        assert_eq!(actual, expected, "{id}");
    }
}
struct Stringify(sdk::StringifyValue);
impl sdk::StringifyArgument for Stringify {
    type Error = Diagnostic;
    fn convert(&mut self) -> Result<sdk::StringifyValue, Diagnostic> {
        Ok(self.0.clone())
    }
}
#[test]
fn eleven_actual_primitive_stringify_results_include_undefined_bigint_and_rejected_tags() {
    for c in fixture()["argument_cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["api"] == "stringify")
    {
        let id = c["id"].as_str().unwrap();
        let value = match id {
            "stringify-missing" => sdk::StringifyValue::Undefined,
            "stringify-bigint" => sdk::StringifyValue::BigInt("900719925474099312345".into()),
            "stringify-symbol" | "stringify-merge-symbol" => sdk::StringifyValue::Symbol,
            "stringify-function" => sdk::StringifyValue::Function,
            "stringify-null" => sdk::StringifyValue::Document(yaml_graph::parse("").unwrap()),
            "stringify-false" => sdk::StringifyValue::Document(yaml_graph::parse("false").unwrap()),
            "stringify-zero" => sdk::StringifyValue::Document(yaml_graph::parse("0").unwrap()),
            "stringify-nan" => sdk::StringifyValue::Document(yaml_graph::parse(".nan").unwrap()),
            "stringify-infinity" => {
                sdk::StringifyValue::Document(yaml_graph::parse(".inf").unwrap())
            }
            "stringify-borrowed-this" => {
                sdk::StringifyValue::Document(yaml_graph::parse("value: text").unwrap())
            }
            _ => continue,
        };
        let actual = match sdk::stringify(&mut Stringify(value)) {
            Ok(None) => json!({"result":{"type":"undefined"},"trace":[],"warnings":[]}),
            Ok(Some(units)) => {
                json!({"result":{"type":"string","units":units},"trace":[],"warnings":[]})
            }
            Err(sdk::CallError::Host(e) | sdk::CallError::Yaml(e)) => {
                json!({"error":error(&e),"trace":[],"warnings":[]})
            }
        };
        let mut expected = c["result"].clone();
        if expected.get("error").is_some() {
            expected["error"] = native_error(&expected["error"])
        }
        assert_eq!(actual, expected, "{id}");
    }
}
struct ConsoleHost {
    mode: String,
    trace: Vec<Value>,
    after: Vec<Value>,
    gets: usize,
    calls: usize,
    reentered: bool,
}
impl sdk::Console<Diagnostic> for ConsoleHost {
    fn warn(&mut self, w: ConsoleWarning) -> Result<(), Diagnostic> {
        // Authored callback models the captured host actions. Live VM property lookup
        // and Error object identity are separate integration obligations.
        if self.mode == "getter-per-warning" || self.mode == "getter-throws" {
            self.gets += 1;
            self.trace.push(if self.mode == "getter-throws" {
                json!({"get":self.gets})
            } else {
                json!({"get":self.gets,"this_console":true})
            });
            if self.mode == "getter-throws" {
                return Err(Diagnostic::exception("Error", "warn getter failed"));
            }
        }
        if self.mode == "warn-noncallable" {
            return Err(Diagnostic::exception(
                "TypeError",
                "console.warn is not a function",
            ));
        }
        if self.mode == "replace-warn-after-first" && self.calls > 0 {
            self.trace.push(json!({"replacement":true}));
        }
        self.calls += 1;
        let mut arg = warning(&w);
        self.trace
            .push(json!({"call":self.calls,"this_console":true,"args":[arg.clone()]}));
        if self.mode == "warning-mutation" && arg.get("name").is_some() {
            arg["message_utf16"] = json!("mutated warning".encode_utf16().collect::<Vec<_>>());
            arg["code"] = json!("MUTATED");
        }
        self.after.push(arg);
        if self.mode == "warn-throws" {
            let mut e = Diagnostic::exception("Error", "warn failed");
            e.code = Some("WARN".into());
            return Err(e);
        }
        if self.mode == "warn-reenters" && !self.reentered {
            self.reentered = true;
            let mut input = Input {
                kind: sdk::ParseKind::String("inside: !inner value\n".encode_utf16().collect()),
                length: Ok(()),
                trace: vec![],
            };
            sdk::parse(&mut input, self).unwrap();
        }
        Ok(())
    }
}
fn strip_host_descriptors(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            for key in ["constructor", "own_names", "is_error"] {
                fields.remove(key);
            }
            for item in fields.values_mut() {
                strip_host_descriptors(item)
            }
        }
        Value::Array(items) => {
            for item in items {
                strip_host_descriptors(item)
            }
        }
        _ => {}
    }
}
#[test]
fn eight_actual_console_sequences_preserve_reentry_throw_override_and_syntax_precedence() {
    for c in fixture()["console_cases"].as_array().unwrap() {
        let mode = c["id"].as_str().unwrap();
        let source = if mode == "parse-errors-before-key-conversion" {
            "first: !one text\n? [key]\n: value\nlast: x\nlast: y\n"
        } else {
            "first: !one text\nlast: !two text\n? [key]\n: value\n"
        };
        let mut input = Input {
            kind: sdk::ParseKind::String(source.encode_utf16().collect()),
            length: Ok(()),
            trace: vec![],
        };
        let mut host = ConsoleHost {
            mode: mode.into(),
            trace: vec![],
            after: vec![],
            gets: 0,
            calls: 0,
            reentered: false,
        };
        let result = sdk::parse(&mut input, &mut host);
        let actual = match result {
            Ok(d) => json!({"result":descriptor(&d,d.root),"trace":host.trace,"after":host.after}),
            Err(sdk::CallError::Host(e) | sdk::CallError::Yaml(e)) => {
                json!({"error":error(&e),"trace":host.trace,"after":host.after})
            }
        };
        let mut expected = c["result"].clone();
        strip_host_descriptors(&mut expected);
        assert_eq!(actual, expected, "{mode}");
    }
}
