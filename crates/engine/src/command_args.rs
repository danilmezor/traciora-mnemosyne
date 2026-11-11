//! JavaScript object argument parsing and shared command preparation.
use indexmap::IndexMap;

pub type Params = IndexMap<String, Option<String>>;
use std::cmp::Ordering;

/// Object.prototype members participate in source `in` and truthiness checks.
pub fn is_prototype_key(key: &str) -> bool {
    matches!(
        key,
        "constructor"
            | "__defineGetter__"
            | "__defineSetter__"
            | "hasOwnProperty"
            | "__lookupGetter__"
            | "__lookupSetter__"
            | "isPrototypeOf"
            | "propertyIsEnumerable"
            | "toLocaleString"
            | "toString"
            | "valueOf"
            | "__proto__"
    )
}
fn array_index(key: &str) -> Option<u32> {
    let value = key.parse::<u32>().ok()?;
    (value != u32::MAX && value.to_string() == key).then_some(value)
}
/// JS own-object enumeration lists canonical array indices first, ascending,
/// followed by other properties in insertion order. Equal preserves that order.
pub fn object_key_compare(a: &str, b: &str) -> Ordering {
    match (array_index(a), array_index(b)) {
        (Some(a), Some(b)) => a.cmp(&b),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}
/// Every CLI assignment is a primitive string. The inherited __proto__ setter
/// ignores such assignments instead of creating an ordinary own property.
pub fn assign(params: &mut Params, key: &str, value: &str) {
    if key != "__proto__" {
        params.insert(key.to_owned(), Some(value.to_owned()));
    }
}
pub fn normalize_params(params: &mut Params) {
    params.shift_remove("__proto__");
    for value in params.values_mut() {
        if value.is_none() {
            *value = Some("true".into());
        }
    }
    params.sort_by(|a, _, b, _| object_key_compare(a, b));
}
pub fn parse_argv(argv: &[String]) -> (String, Params) {
    let command = match argv.first().map(String::as_str) {
        None | Some("") | Some("--help") => "help",
        Some(command) => command,
    }
    .to_owned();
    let mut params = Params::new();
    for token in argv.iter().skip(1) {
        let (key, value) = token.split_once('=').unwrap_or((token, "true"));
        assign(&mut params, key, value);
    }
    normalize_params(&mut params);
    (command, params)
}
pub fn property_present(params: &Params, key: &str) -> bool {
    params.contains_key(key) || is_prototype_key(key)
}
pub fn property_truthy(params: &Params, key: &str) -> bool {
    match params.get(key) {
        Some(value) => value.as_ref().is_none_or(|v| !v.is_empty()),
        None => is_prototype_key(key),
    }
}
pub fn apply_format_aliases(params: &mut Params, formats: Option<&str>) {
    if property_truthy(params, "format") {
        return;
    }
    if let Some(formats) = formats {
        for format in formats.split('|') {
            let long = format!("--{format}");
            if property_truthy(params, format) || property_truthy(params, &long) {
                assign(params, "format", format);
                params.shift_remove(format);
                params.shift_remove(&long);
                normalize_params(params);
                break;
            }
        }
    }
}
