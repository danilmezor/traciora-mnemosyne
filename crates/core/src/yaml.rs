//! JavaScript-value YAML/JSON boundary used by property commands.
//! Typed parsing lives in yaml_graph; this module retains the shared block
//! formatter and legacy tree conversion needed by remaining callback branches.
use crate::vault_search::ecmascript_number;
use serde_yaml::{Mapping, Value};

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct YamlError {
    pub message: String,
    pub exception_name: Option<&'static str>,
}

impl From<crate::yaml_value::Diagnostic> for YamlError {
    fn from(error: crate::yaml_value::Diagnostic) -> Self {
        let exception_name = match error.name.as_str() {
            "TypeError" => Some("TypeError"),
            "RangeError" => Some("RangeError"),
            "ReferenceError" => Some("ReferenceError"),
            _ => None,
        };
        Self {
            message: error.message,
            exception_name,
        }
    }
}

pub fn parse_js(source: &str) -> Result<Value, YamlError> {
    parse_value(source).map(normalize_value)
}

/// Preserve ordinary internal numbers while correcting captured explicit tags.
/// General YAML schema resolution and warning delivery remain pending.
pub fn parse_value(source: &str) -> Result<Value, YamlError> {
    let prepared = explicit_scalar_tags(source);
    serde_yaml::from_str::<Value>(&prepared).map_err(|error| {
        let backend = error.to_string();
        let message = if (backend.contains("parsing a flow sequence") || backend.contains("parsing a flow mapping")) && error.location().is_some_and(|location| location.line() > source.lines().count()) {
            let location = error.location().unwrap();
            let previous = source.trim_end_matches(['\r', '\n']).rsplit('\n').next().unwrap_or("");
            let (name, closing) = if backend.contains("parsing a flow sequence") {("sequence", "]")} else {("map", "}")};
            format!("Flow {name} in block collection must be sufficiently indented and end with a {closing} at line {}, column {}:\n\n{previous}\n\n^", location.line(), location.column())
        } else if backend.starts_with("duplicate entry with key ") && source.lines().count() == 2 {
            // Captured simple top-level duplicate mapping diagnostic. General
            // YAML node locations and source-context truncation remain pending.
            let lines:Vec<_>=source.lines().collect();
            let key=|line:&str|line.split_once(':').map(|(key,_)|key.trim().to_owned());
            if key(lines[0]).is_some() && key(lines[0])==key(lines[1]) {
                format!("Map keys must be unique at line 2, column 1:\n\n{}\n{}\n^",lines[0],lines[1])
            }else{backend}
        } else {backend};
        YamlError {message, exception_name: None}
    })
}
/// The worker's frontmatter tokenizer removes its separator LF. YAML block
/// scalar resolution then supplies a terminating LF for non-stripped scalars.
/// Preserve internal numeric values while converting object keys like toJS.
pub fn parse_metadata_mapping(source: &str) -> Result<Option<Mapping>, YamlError> {
    crate::yaml_graph::parse_frontmatter(source)
        .and_then(|document| document.metadata_mapping())
        .map_err(|error| YamlError {
            exception_name: None,
            message: error.message,
        })
}
fn property_key(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Sequence(_) | Value::Mapping(_) => flow_key(value),
        Value::Tagged(tag) => property_key(&tag.value),
        value => javascript_string(value),
    }
}
fn flow_key(value: &Value) -> String {
    match value {
        Value::Sequence(values) if !values.is_empty() => format!(
            "[ {} ]",
            values.iter().map(flow_key).collect::<Vec<_>>().join(", ")
        ),
        Value::Mapping(map) if !map.is_empty() => format!(
            "{{ {} }}",
            map.iter()
                .map(|(k, v)| format!("{}: {}", flow_key(k), flow_key(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Null => "null".into(),
        value => scalar(value),
    }
}

fn explicit_scalar_tags(source: &str) -> String {
    source
        .split_inclusive('\n')
        .map(|line| {
            // This captured fallback handles direct top-level scalar fields.
            // Indented block-scalar text and nested tag syntax are preserved.
            if line.starts_with([' ', '\t']) {
                return line.to_owned();
            }
            let Some((key, value)) = line.split_once(':') else {
                return line.to_owned();
            };
            let value = value.trim_start_matches([' ', '\t']);
            let raw = value.trim_end_matches(['\r', '\n']);
            let suffix = if line.ends_with("\r\n") {
                "\r\n"
            } else if line.ends_with('\n') {
                "\n"
            } else {
                ""
            };
            if let Some(bits) = raw.strip_prefix("0b") {
                if !bits.is_empty() && bits.chars().all(|c| matches!(c, '0' | '1')) {
                    return format!("{key}: {}{suffix}", serde_json::to_string(raw).unwrap());
                }
            }
            let digits = raw.strip_prefix(['+', '-']).unwrap_or(raw);
            if digits.len() > 1
                && digits.starts_with('0')
                && digits.chars().all(|c| c.is_ascii_digit())
            {
                let n = if raw.starts_with('-') {
                    raw.parse::<i64>().ok().map(|n| n.to_string())
                } else {
                    raw.trim_start_matches('+')
                        .parse::<u64>()
                        .ok()
                        .map(|n| n.to_string())
                };
                if let Some(n) = n {
                    return format!("{key}: {n}{suffix}");
                }
            }
            let (kind, scalar) = if let Some(value) = value.strip_prefix("!!float ") {
                ("float", value)
            } else if let Some(value) = value.strip_prefix("!!int ") {
                ("int", value)
            } else {
                return line.to_owned();
            };
            let plain = scalar.trim_end_matches(['\r', '\n']);
            let digits = plain.strip_prefix(['+', '-']).unwrap_or(plain);
            if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
                return line.to_owned();
            }
            let suffix = if line.ends_with("\r\n") {
                "\r\n"
            } else if line.ends_with('\n') {
                "\n"
            } else {
                ""
            };
            if kind == "float" {
                format!("{key}: {}{suffix}", serde_json::to_string(plain).unwrap())
            } else {
                let normalized = if plain.starts_with('-') {
                    plain.parse::<i64>().ok().map(|n| n.to_string())
                } else {
                    plain
                        .trim_start_matches('+')
                        .parse::<u64>()
                        .ok()
                        .map(|n| n.to_string())
                };
                normalized.map_or_else(|| line.to_owned(), |n| format!("{key}: {n}{suffix}"))
            }
        })
        .collect()
}

pub fn normalize_value(value: Value) -> Value {
    match value {
        Value::Number(n) => Value::Number(serde_yaml::Number::from(n.as_f64().unwrap())),
        Value::Sequence(values) => {
            Value::Sequence(values.into_iter().map(normalize_value).collect())
        }
        Value::Mapping(map) => {
            let mut normalized = Mapping::new();
            for (key, value) in map {
                normalized.insert(Value::String(property_key(&key)), normalize_value(value));
            }
            Value::Mapping(normalized)
        }
        Value::Tagged(tag) => normalize_value(tag.value),
        value => value,
    }
}
fn array_index(key: &str) -> Option<u32> {
    let value = key.parse::<u32>().ok()?;
    if value == u32::MAX || value.to_string() != key {
        None
    } else {
        Some(value)
    }
}
pub fn ordered_entries(map: &Mapping) -> Vec<(&Value, &Value)> {
    let mut entries: Vec<_> = map.iter().collect();
    entries.sort_by_key(|(key, _)| array_index(key.as_str().unwrap_or(""))); // None first; move integer keys ahead below.
    let mut indexed: Vec<_> = entries
        .iter()
        .copied()
        .filter(|(key, _)| array_index(key.as_str().unwrap_or("")).is_some())
        .collect();
    // Preserve declaration order for ordinary names, as JS Object.keys does.
    indexed.extend(
        map.iter()
            .filter(|(key, _)| array_index(key.as_str().unwrap_or("")).is_none()),
    );
    indexed
}
pub fn javascript_string(value: &Value) -> String {
    match value {
        Value::Null => "null".into(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => ecmascript_number(value.as_f64().unwrap()),
        Value::String(value) => value.clone(),
        Value::Sequence(values) => values
            .iter()
            .map(|value| {
                if value.is_null() {
                    String::new()
                } else {
                    javascript_string(value)
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Mapping(_) => "[object Object]".into(),
        Value::Tagged(value) => javascript_string(&value.value),
    }
}
pub fn stringify(value: &Value) -> String {
    let mut output = String::new();
    emit(value, 0, &mut output);
    output
}
fn scalar(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Number(n) => {
            let value = n.as_f64().unwrap();
            if value.is_nan() {
                ".nan".into()
            } else if value == f64::INFINITY {
                ".inf".into()
            } else if value == f64::NEG_INFINITY {
                "-.inf".into()
            } else {
                ecmascript_number(value)
            }
        }
        Value::Bool(value) => value.to_string(),
        Value::Mapping(map) if map.is_empty() => "{}".into(),
        Value::Sequence(values) if values.is_empty() => "[]".into(),
        Value::String(value) => {
            if value.strip_prefix("0b").is_some_and(|bits| {
                !bits.is_empty() && bits.chars().all(|c| matches!(c, '0' | '1'))
            }) {
                return value.clone();
            }
            if !value.is_empty() && value.chars().all(|c| c == '\n') {
                return serde_json::to_string(value).unwrap();
            }
            let encoded = serde_yaml::to_string(value).unwrap();
            let encoded = encoded.strip_suffix('\n').unwrap_or(&encoded);
            let quoted = (encoded.starts_with('\'') && encoded.ends_with('\''))
                || (encoded.starts_with('"') && encoded.ends_with('"'));
            // Native kf prefers single quotes when a quoted value contains a
            // double quote but no single quote. wf rejects control/newline cases.
            if quoted
                && value.contains('"')
                && !value.contains('\'')
                && !value.chars().any(|c| c < ' ')
            {
                format!("'{value}'")
            } else if encoded.starts_with('\'') && encoded.ends_with('\'') {
                serde_json::to_string(value).unwrap()
            } else {
                encoded.to_owned()
            }
        }
        Value::Tagged(value) if value.tag == serde_yaml::value::Tag::new("__native_wtf16") => {
            let units: Vec<u16> = value
                .value
                .as_sequence()
                .unwrap()
                .iter()
                .map(|value| value.as_u64().unwrap() as u16)
                .collect();
            quote_yaml_units(&units)
        }
        Value::Tagged(value) => scalar(&value.value),
        _ => unreachable!("container handled by emitter"),
    }
}
fn container(value: &Value) -> bool {
    matches!(value,Value::Mapping(map)if !map.is_empty())
        || matches!(value,Value::Sequence(values)if !values.is_empty())
}
fn emit_value(value: &Value, indent: usize, output: &mut String) {
    if container(value) {
        output.push('\n');
        emit(value, indent + 2, output)
    } else {
        let encoded = scalar(value);
        if !encoded.is_empty() {
            output.push(' ')
        }
        let mut lines = encoded.split('\n');
        output.push_str(lines.next().unwrap_or(""));
        output.push('\n');
        for line in lines {
            if !line.is_empty() {
                output.push_str(&" ".repeat(indent));
            }
            output.push_str(line);
            output.push('\n');
        }
    }
}
fn emit(value: &Value, indent: usize, output: &mut String) {
    let prefix = " ".repeat(indent);
    match value {
        Value::Mapping(map) if !map.is_empty() => {
            for (key, value) in ordered_entries(map) {
                output.push_str(&prefix);
                output.push_str(&scalar(key));
                output.push(':');
                emit_value(value, indent, output)
            }
        }
        Value::Sequence(values) if !values.is_empty() => {
            for value in values {
                output.push_str(&prefix);
                output.push('-');
                if value.is_null() {
                    output.push_str(" \n");
                } else if container(value) {
                    let mut nested = String::new();
                    emit(value, indent + 2, &mut nested);
                    output.push(' ');
                    output.push_str(&nested[indent + 2..]);
                } else {
                    emit_value(value, indent, output)
                }
            }
        }
        _ => {
            output.push_str(&prefix);
            output.push_str(&scalar(value));
            output.push('\n')
        }
    }
}
pub fn json_pretty(value: &Value) -> String {
    json(value, 0)
}
fn quote(value: &str) -> String {
    serde_json::to_string(value).unwrap()
}
fn json(value: &Value, indent: usize) -> String {
    let prefix = " ".repeat(indent);
    let child = " ".repeat(indent + 2);
    match value {
        Value::Null => "null".into(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => {
            let value = value.as_f64().unwrap();
            if value.is_finite() {
                ecmascript_number(value)
            } else {
                "null".into()
            }
        }
        Value::String(value) => quote(value),
        Value::Sequence(values) if values.is_empty() => "[]".into(),
        Value::Sequence(values) => format!(
            "[\n{}\n{prefix}]",
            values
                .iter()
                .map(|value| format!("{child}{}", json(value, indent + 2)))
                .collect::<Vec<_>>()
                .join(",\n")
        ),
        Value::Mapping(map) if map.is_empty() => "{}".into(),
        Value::Mapping(map) => format!(
            "{{\n{}\n{prefix}}}",
            ordered_entries(map)
                .into_iter()
                .map(|(key, value)| format!(
                    "{child}{}: {}",
                    quote(&javascript_string(key)),
                    json(value, indent + 2)
                ))
                .collect::<Vec<_>>()
                .join(",\n")
        ),
        Value::Tagged(value) => json(&value.value, indent),
    }
}

/// Native bf control escape spelling over lossless JavaScript units. Line width
/// and multiline folding remain a separate formatting boundary.
pub(crate) fn quote_yaml_units(units: &[u16]) -> String {
    let json = crate::yaml_value::quote_units(units);
    let mut output = String::new();
    let mut chars = json.chars().peekable();
    while let Some(character) = chars.next() {
        if character != '\\' {
            output.push(character);
            continue;
        }
        let Some(next) = chars.next() else {
            output.push(character);
            break;
        };
        if next != 'u' {
            output.push('\\');
            output.push(next);
            continue;
        }
        let hex: String = chars.by_ref().take(4).collect();
        let short = match hex.as_str() {
            "0000" => Some("0"),
            "0007" => Some("a"),
            "000b" => Some("v"),
            "001b" => Some("e"),
            "0085" => Some("N"),
            "00a0" => Some("_"),
            "2028" => Some("L"),
            "2029" => Some("P"),
            _ => None,
        };
        output.push('\\');
        if let Some(short) = short {
            output.push_str(short);
        } else if hex.starts_with("00") {
            output.push('x');
            output.push_str(&hex[2..]);
        } else {
            output.push('u');
            output.push_str(&hex);
        }
    }
    output
}
