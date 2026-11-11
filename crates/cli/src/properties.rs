//! Property command coercion, cached reads and raw frontmatter persistence.
use crate::search::Params;
use anyhow::{bail, Result};
use mnemosyne_core::{
    collation::NaturalCollator,
    extension, frontmatter,
    vault_search::js_whitespace,
    yaml,
    yaml_graph::Document,
    yaml_value::{Node, NodeId},
};
use mnemosyne_engine::{Engine, FileTarget};
use serde_yaml::Value;
fn value<'a>(p: &'a Params, key: &str) -> Option<&'a str> {
    p.get(key).map(|v| v.as_deref().unwrap_or("true"))
}
fn truthy(p: &Params, key: &str) -> bool {
    value(p, key).is_some_and(|value| !value.is_empty())
}
fn target<'a>(p: &'a Params) -> FileTarget<'a> {
    FileTarget {
        file: value(p, "file"),
        path: value(p, "path"),
    }
}
fn cached_frontmatter(engine: &Engine, path: &str) -> Option<Value> {
    engine
        .file_cache(path)?
        .frontmatter
        .as_ref()
        .map(|map| yaml::normalize_value(Value::Mapping(map.clone())))
}
fn array_join(values: &[Value], separator: &str) -> String {
    values
        .iter()
        .map(|value| {
            if value.is_null() {
                String::new()
            } else {
                yaml::javascript_string(value)
            }
        })
        .collect::<Vec<_>>()
        .join(separator)
}

fn native_array_join(document: &Document, values: &[NodeId], separator: &str) -> Result<String> {
    values
        .iter()
        .map(|id| {
            if matches!(document.nodes[*id], Node::Null) {
                Ok(String::new())
            } else {
                Ok(document.javascript_string_lossy(*id)?)
            }
        })
        .collect::<Result<Vec<_>>>()
        .map(|values| values.join(separator))
}
fn native_list(document: &Document, format: Option<&str>) -> Result<String> {
    let fields = document.own_fields_units();
    if fields.is_empty() {
        return Ok("No frontmatter found.".into());
    }
    match format {
        Some("json") => Ok(document.json(true)?),
        Some("tsv") => {
            let mut rows = Vec::new();
            for (key, id) in fields {
                let key = String::from_utf16_lossy(&key);
                let text = match &document.nodes[id] {
                    Node::Null => continue,
                    Node::Array(values) => native_array_join(document, values, ", ")?,
                    _ => document.javascript_string_lossy(id)?,
                };
                rows.push(format!("{key}\t{text}"));
            }
            Ok(rows.join("\n"))
        }
        _ => Ok(document.stringify()?.trim_matches(js_whitespace).to_owned()),
    }
}

pub fn execute(engine: &mut Engine, p: &Params, command: &str) -> Result<String> {
    match command {
        "properties" => list(engine, p),
        "property:read" => read(engine, p),
        "property:set" => set(engine, p),
        "property:remove" => remove(engine, p),
        _ => bail!("Unknown property operation: {command}"),
    }
}
fn list(engine: &mut Engine, p: &Params) -> Result<String> {
    if ["active", "file", "path"].iter().any(|key| truthy(p, key)) {
        let path = engine.resolve_target(target(p), true)?;
        if let Some(document) = engine
            .file_cache(&path)
            .and_then(|cache| cache.native_frontmatter.as_ref())
        {
            return native_list(document, value(p, "format"));
        }
        let Some(data) = cached_frontmatter(engine, &path) else {
            return Ok("No frontmatter found.".into());
        };
        let map = data.as_mapping().unwrap();
        if map.is_empty() {
            return Ok("No frontmatter found.".into());
        }
        if value(p, "format") == Some("json") {
            return Ok(yaml::json_pretty(&data));
        }
        if value(p, "format") == Some("tsv") {
            return Ok(yaml::ordered_entries(map)
                .into_iter()
                .filter_map(|(key, value)| {
                    let text = match value {
                        Value::Sequence(values) => array_join(values, ", "),
                        Value::Null => return None,
                        value => yaml::javascript_string(value),
                    };
                    Some(format!("{}\t{text}", yaml::javascript_string(key)))
                })
                .collect::<Vec<_>>()
                .join("\n"));
        }
        return Ok(yaml::stringify(&data)
            .trim_matches(js_whitespace)
            .to_owned());
    }
    let properties = engine.property_infos_units();
    if let Some(name) = value(p, "name").filter(|value| !value.is_empty()) {
        let wanted =
            mnemosyne_core::collation::lowercase_units(&name.encode_utf16().collect::<Vec<_>>())?;
        let mut found = false;
        for property in &properties {
            if mnemosyne_core::collation::lowercase_units(&property.name)? == wanted {
                found = true;
                break;
            }
        }
        if !found {
            bail!("Property \"{name}\" not found.")
        }
        return Ok(engine.property_occurrences(name)?.to_string());
    }
    if truthy(p, "total") {
        return Ok(properties.len().to_string());
    }
    let mut properties = properties
        .into_iter()
        .map(|property| Ok((engine.property_occurrences_units(&property.name)?, property)))
        .collect::<Result<Vec<_>>>()?;
    if value(p, "sort") == Some("count") {
        properties.sort_by_key(|(count, _)| std::cmp::Reverse(*count));
    } else {
        let collator = NaturalCollator::new(engine.collation_locale())?;
        properties.sort_by(|(_, a), (_, b)| collator.compare_units(&a.name, &b.name));
    }
    if value(p, "format") == Some("json") {
        use mnemosyne_engine::json_units::JsonUnits;
        let result = JsonUnits::Array(
            properties
                .iter()
                .map(|(count, property)| {
                    JsonUnits::Object(vec![
                        (
                            "name".encode_utf16().collect(),
                            JsonUnits::String(property.name.clone()),
                        ),
                        ("type".encode_utf16().collect(), property.widget.clone()),
                        (
                            "count".encode_utf16().collect(),
                            JsonUnits::Number(*count as f64),
                        ),
                    ])
                })
                .collect(),
        );
        return Ok(result.pretty());
    }
    Ok(properties
        .iter()
        .map(|(count, property)| {
            if truthy(p, "counts") {
                format!("{}\t{}", String::from_utf16_lossy(&property.name), count)
            } else {
                String::from_utf16_lossy(&property.name)
            }
        })
        .collect::<Vec<_>>()
        .join("\n"))
}
fn require<'a>(p: &'a Params, command: &str, set: bool) -> Result<&'a str> {
    let missing: Vec<_> = if set {
        vec!["name", "value"]
    } else {
        vec!["name"]
    }
    .into_iter()
    .filter(|key| !p.contains_key(*key))
    .collect();
    if !missing.is_empty() {
        let missing = missing
            .iter()
            .map(|key| format!("{key}=<{key}>"))
            .collect::<Vec<_>>()
            .join(", ");
        let usage = if set {
            " name=<name> value=<value> [type=text|list|number|checkbox|date|datetime] [file=<name>] [path=<path>]"
        } else {
            " name=<name> [file=<name>] [path=<path>]"
        };
        bail!("Missing required parameter: {missing}\nUsage: {command}{usage}")
    }
    let name = value(p, "name").unwrap();
    if name.is_empty() {
        bail!("Missing required parameter: name")
    };
    Ok(name)
}
fn read(engine: &mut Engine, p: &Params) -> Result<String> {
    let name = require(p, "property:read", false)?;
    let path = engine.resolve_target(target(p), true)?;
    if let Some(document) = engine
        .file_cache(&path)
        .and_then(|cache| cache.native_frontmatter.as_ref())
    {
        if let Some(id) = document.own_property(name) {
            return match &document.nodes[id] {
                Node::Null => Ok("(empty)".into()),
                Node::String(text) if text.is_empty() => Ok("(empty)".into()),
                Node::Array(values) if values.is_empty() => Ok("(empty)".into()),
                Node::Array(values) => native_array_join(document, values, "\n"),
                _ => Ok(document.javascript_string_lossy(id)?),
            };
        }
    }
    let Some(data) = cached_frontmatter(engine, &path) else {
        bail!("Property \"{name}\" not found.")
    };
    let map = data.as_mapping().unwrap();
    let key = Value::String(name.into());
    let Some(value) = map.get(&key) else {
        // Cached YAML objects inherit Object.prototype. The native handler
        // reads directly without an own-property check.
        let builtin = match name {
            "constructor" => Some("Object"),
            "toString"
            | "toLocaleString"
            | "valueOf"
            | "hasOwnProperty"
            | "isPrototypeOf"
            | "propertyIsEnumerable"
            | "__defineGetter__"
            | "__defineSetter__"
            | "__lookupGetter__"
            | "__lookupSetter__" => Some(name),
            "__proto__" => return Ok("[object Object]".into()),
            _ => None,
        };
        if let Some(name) = builtin {
            return Ok(format!("function {name}() {{ [native code] }}"));
        }
        bail!("Property \"{name}\" not found.")
    };
    Ok(match value {
        Value::Sequence(values) if values.is_empty() => "(empty)".into(),
        Value::Sequence(values) => array_join(values, "\n"),
        Value::Null => "(empty)".into(),
        Value::String(value) if value.is_empty() => "(empty)".into(),
        value => yaml::javascript_string(value),
    })
}
fn number(text: &str) -> Option<f64> {
    let text = text.trim_matches(js_whitespace);
    if text.is_empty() {
        return Some(0.0);
    }
    for (prefix, radix) in [
        ("0x", 16),
        ("0X", 16),
        ("0b", 2),
        ("0B", 2),
        ("0o", 8),
        ("0O", 8),
    ] {
        if let Some(digits) = text.strip_prefix(prefix) {
            if digits.is_empty() {
                return None;
            };
            let mut value = 0.0;
            for digit in digits.chars() {
                value = value * (radix as f64) + (digit.to_digit(radix)? as f64)
            }
            return Some(value);
        }
    }
    if matches!(text, "Infinity" | "+Infinity") {
        return Some(f64::INFINITY);
    }
    if text == "-Infinity" {
        return Some(f64::NEG_INFINITY);
    }
    if !text
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, '+' | '-' | '.' | 'e' | 'E'))
    {
        return None;
    };
    text.parse::<f64>().ok().filter(|value| !value.is_nan())
}
fn shape(text: &str, datetime: bool) -> bool {
    let valid = |text: &str, pattern: &str| {
        text.len() == pattern.len()
            && text.bytes().zip(pattern.bytes()).all(|(c, p)| {
                if p == b'd' {
                    c.is_ascii_digit()
                } else {
                    c == p
                }
            })
    };
    if datetime {
        valid(text, "dddd-dd-ddTdd:dd") || valid(text, "dddd-dd-ddTdd:dd:dd")
    } else {
        valid(text, "dddd-dd-dd")
    }
}
fn set(engine: &mut Engine, p: &Params) -> Result<String> {
    let name = require(p, "property:set", true)?;
    let path = engine.resolve_target(target(p), true)?;
    let raw = value(p, "value").unwrap();
    let mut kind = value(p, "type");
    let data = if raw.starts_with('[') && raw.ends_with(']') {
        match serde_json::from_str::<serde_json::Value>(raw) {
            Ok(value) if value.is_array() => {
                if kind.is_none_or(|kind| kind.is_empty()) {
                    kind = Some("list")
                };
                yaml::normalize_value(serde_yaml::to_value(value)?)
            }
            _ => Value::String(raw.into()),
        }
    } else {
        match kind {
            Some("number") => {
                let Some(value) = number(raw) else {
                    bail!("Invalid number: {raw}")
                };
                Value::Number(serde_yaml::Number::from(value))
            }
            Some("checkbox") => Value::Bool(raw == "true" || raw == "1"),
            Some("list") => Value::Sequence(
                raw.split(',')
                    .map(|value| Value::String(value.trim_matches(js_whitespace).into()))
                    .collect(),
            ),
            Some("date") => {
                if !shape(raw, false) {
                    bail!("Invalid date format. Use YYYY-MM-DD")
                };
                Value::String(raw.into())
            }
            Some("datetime") => {
                if !shape(raw, true) {
                    bail!("Invalid datetime format. Use YYYY-MM-DDTHH:mm or YYYY-MM-DDTHH:mm:ss")
                };
                Value::String(raw.into())
            }
            _ => Value::String(raw.into()),
        }
    };
    modify(engine, &path, name, Some(data))?;
    if let Some(kind) = kind.filter(|kind| !kind.is_empty()) {
        engine.set_property_type(name, if kind == "list" { "multitext" } else { kind })?;
    }
    Ok(format!("Set {name}: {raw}"))
}
fn remove(engine: &mut Engine, p: &Params) -> Result<String> {
    let name = require(p, "property:remove", false)?;
    let path = engine.resolve_target(target(p), true)?;
    modify(engine, &path, name, None)?;
    Ok(format!("Removed: {name}"))
}
fn modify(engine: &mut Engine, path: &str, name: &str, value: Option<Value>) -> Result<()> {
    if extension(path) != "md" {
        return Ok(());
    }
    let source = engine.read_saved(FileTarget::path(path))?;
    let updated =
        frontmatter::process_property(&source, name, value).map_err(|error| -> anyhow::Error {
            if let Some(kind) = error.exception_name {
                crate::error::ScriptException {
                    kind,
                    message: error.message,
                }
                .into()
            } else {
                error.into()
            }
        })?;
    if updated != source {
        engine.vault.modify(path, &updated)?;
        engine.saved_content_modified(path, &updated)?;
        engine.file_modified(path);
    }
    engine.persist_session()?;
    Ok(())
}
