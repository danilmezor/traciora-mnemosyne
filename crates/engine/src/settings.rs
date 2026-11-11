//! Complete shipped Vault defaults and app/appearance configuration partition.
//! Runtime settings, event/debounce and migration evidence is tracked separately.
use serde_json::{Map, Value};
use std::sync::LazyLock;
static DEFAULTS: LazyLock<Map<String, Value>> = LazyLock::new(|| {
    serde_json::from_str(include_str!("settings_defaults.json"))
        .expect("validated shipped defaults")
});
pub fn defaults() -> &'static Map<String, Value> {
    &DEFAULTS
}
pub const APPEARANCE_KEYS: &[&str] = &[
    "accentColor",
    "theme",
    "cssTheme",
    "enabledCssSnippets",
    "showViewHeader",
    "showRibbon",
    "nativeMenus",
    "translucency",
    "textFontFamily",
    "interfaceFontFamily",
    "monospaceFontFamily",
    "baseFontSize",
    "baseFontSizeAction",
    "slidingSidebar",
    "floatingNavigation",
    "autoFullScreen",
];
/// ECMAScript Object.assign's enumerable own properties for ordinary JSON data.
/// Surrogate-only string indices/prototype side effects require the plugin VM.
pub(crate) fn array_index(key: &str) -> Option<u32> {
    let index = key.parse::<u32>().ok()?;
    (index != u32::MAX && index.to_string() == key).then_some(index)
}
pub fn enumerable(value: &Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => {
            let mut entries = map.iter().collect::<Vec<_>>();
            entries.sort_by(|(a, _), (b, _)| match (array_index(a), array_index(b)) {
                (Some(a), Some(b)) => a.cmp(&b),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                _ => std::cmp::Ordering::Equal,
            });
            entries
                .into_iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        }
        Value::Array(items) => items
            .iter()
            .enumerate()
            .map(|(i, value)| (i.to_string(), value.clone()))
            .collect(),
        Value::String(text) => text
            .chars()
            .enumerate()
            .map(|(i, c)| (i.to_string(), Value::String(c.to_string())))
            .collect(),
        _ => Map::new(),
    }
}
pub fn merge(appearance: Option<&Value>, app: Option<&Value>) -> Map<String, Value> {
    let mut merged = appearance.map(enumerable).unwrap_or_default();
    if let Some(app) = app {
        for (key, value) in enumerable(app) {
            merged.insert(key, value);
        }
    }
    enumerable(&Value::Object(merged))
}
/// setupConfig applies this once; reloadConfig deliberately does not.
pub fn migrate_initial(config: &mut Map<String, Value>) {
    if let Some(old) = config.shift_remove("editorFontFamily") {
        if !config.contains_key("textFontFamily") && js_truthy(&old) {
            config.insert("textFontFamily".into(), old);
        }
    }
}
pub(crate) fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::String(s) => !s.is_empty(),
        Value::Number(n) => n.as_f64() != Some(0.0),
        _ => true,
    }
}
pub fn partition(config: &Map<String, Value>) -> (Map<String, Value>, Map<String, Value>) {
    let mut app = Map::new();
    let mut appearance = Map::new();
    for (key, value) in enumerable(&Value::Object(config.clone())) {
        if APPEARANCE_KEYS.contains(&key.as_str()) {
            appearance.insert(key.clone(), value.clone());
        } else {
            app.insert(key.clone(), value.clone());
        }
    }
    (app, appearance)
}
