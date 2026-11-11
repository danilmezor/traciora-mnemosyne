//! Retained MetadataTypeManager rules from the pinned application.
//! Event debounce and metadata iteration chronology are independent contracts;
//! caller supplies the current ordered cache entries to rebuild/update methods.
use crate::json_units::{array_index_units, units, JsonUnits};
use indexmap::IndexMap;
use mnemosyne_core::{cache::CachedMetadata, collation::UnicodeCaseError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_yaml::Value as Yaml;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PropertyInfo {
    pub name: String,
    pub widget: Value,
}
#[derive(Clone, Debug, Default)]
pub struct MetadataTypeManager {
    assigned: IndexMap<Vec<u16>, PropertyInfoUnits>,
    properties: IndexMap<Vec<u16>, PropertyInfoUnits>,
    occurrences: IndexMap<Vec<u16>, usize>,
    loaded: bool,
    last_load_error: Option<TypeLoadError>,
}
#[derive(Clone, Debug, PartialEq)]
pub struct PropertyInfoUnits {
    pub name: Vec<u16>,
    pub widget: JsonUnits,
}
impl PropertyInfoUnits {
    pub fn try_utf8(&self) -> Option<PropertyInfo> {
        Some(PropertyInfo {
            name: String::from_utf16(&self.name).ok()?,
            widget: self.widget.to_value()?,
        })
    }
}
/// Pinned V8 non-Latin1 casing uses ICU root UTF16 conversion. The same
/// bundled ICU data also preserves lone units and contextual/expanded mappings.
pub fn lower_units(name: &[u16]) -> Result<Vec<u16>, UnicodeCaseError> {
    mnemosyne_core::collation::lowercase_units(name)
}
#[derive(Debug, thiserror::Error)]
pub enum MetadataTypeError {
    #[error("{0}")]
    UnicodeCase(#[from] UnicodeCaseError),
    #[error("Property data requires the lossless UTF16 API")]
    Utf8Projection,
}
fn defaults() -> [(Vec<u16>, PropertyInfoUnits); 3] {
    [
        ("aliases", "aliases"),
        ("cssclasses", "multitext"),
        ("tags", "tags"),
    ]
    .map(|(name, widget)| {
        (
            units(name),
            PropertyInfoUnits {
                name: units(name),
                widget: JsonUnits::String(units(widget)),
            },
        )
    })
}
fn keys<T>(map: &IndexMap<Vec<u16>, T>) -> Vec<&[u16]> {
    let mut result = map.keys().map(Vec::as_slice).collect::<Vec<_>>();
    result.sort_by(|a, b| match (array_index_units(a), array_index_units(b)) {
        (Some(a), Some(b)) => a.cmp(&b),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        _ => std::cmp::Ordering::Equal,
    });
    result
}
fn field_key(value: &Yaml) -> String {
    match value {
        Yaml::String(s) => s.clone(),
        Yaml::Null => "null".into(),
        Yaml::Bool(b) => b.to_string(),
        Yaml::Number(n) => {
            let s = n.to_string();
            match s.as_str() {
                ".inf" => "Infinity".into(),
                "-.inf" => "-Infinity".into(),
                ".nan" => "NaN".into(),
                _ => s,
            }
        }
        Yaml::Sequence(items) => items
            .iter()
            .map(|v| {
                if matches!(v, Yaml::Null) {
                    String::new()
                } else {
                    field_key(v)
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        Yaml::Mapping(_) => "[object Object]".into(),
        Yaml::Tagged(t) => field_key(&t.value),
    }
}
struct Field {
    name: Vec<u16>,
    is_null: bool,
    widget: &'static str,
}
fn fields(cache: &CachedMetadata) -> Vec<Field> {
    let mut result = if let Some(document) = &cache.native_frontmatter {
        document
            .own_fields_units()
            .iter()
            .map(|(name, id)| Field {
                name: name.clone(),
                is_null: matches!(document.nodes[*id], mnemosyne_core::yaml_value::Node::Null),
                widget: document.inferred_widget(*id),
            })
            .collect::<Vec<_>>()
    } else {
        let Some(mapping) = &cache.frontmatter else {
            return Vec::new();
        };
        mapping
            .iter()
            .map(|(key, value)| Field {
                name: units(&field_key(key)),
                is_null: matches!(value, Yaml::Null),
                widget: infer_widget(value),
            })
            .collect::<Vec<_>>()
    };
    result.sort_by(
        |a, b| match (array_index_units(&a.name), array_index_units(&b.name)) {
            (Some(a), Some(b)) => a.cmp(&b),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            _ => std::cmp::Ordering::Equal,
        },
    );
    result
}
fn date_shape(text: &str, datetime: bool) -> bool {
    let b = text.as_bytes();
    let length = if datetime { 16 } else { 10 };
    if b.len() != length {
        return false;
    }
    let date = b[..4].iter().all(u8::is_ascii_digit)
        && b[4] == b'-'
        && matches!(b[5], b'0' | b'1')
        && b[6].is_ascii_digit()
        && b[7] == b'-'
        && matches!(b[8], b'0'..=b'3')
        && b[9].is_ascii_digit();
    date && (!datetime
        || (b[10] == b'T'
            && matches!(b[11], b'0'..=b'2')
            && b[12].is_ascii_digit()
            && b[13] == b':'
            && matches!(b[14], b'0'..=b'5')
            && b[15].is_ascii_digit()))
}
pub fn infer_widget(value: &Yaml) -> &'static str {
    match value {
        Yaml::Null => "text",
        Yaml::String(s) => {
            if date_shape(s, true) {
                "datetime"
            } else if date_shape(s, false) {
                "date"
            } else {
                "text"
            }
        }
        Yaml::Number(_) => "number",
        Yaml::Bool(_) => "checkbox",
        Yaml::Sequence(items)
            if items
                .iter()
                .all(|v| matches!(v, Yaml::String(_) | Yaml::Null)) =>
        {
            "multitext"
        }
        Yaml::Tagged(tag) => infer_widget(&tag.value),
        _ => "unknown",
    }
}
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, Serialize)]
#[error("{message}")]
pub struct TypeLoadError {
    pub name: &'static str,
    pub message: String,
}
fn config_entries(value: &JsonUnits) -> Vec<(Vec<u16>, JsonUnits)> {
    match value {
        JsonUnits::Object(fields) => crate::json_units::enumerate(fields)
            .into_iter()
            .cloned()
            .collect(),
        JsonUnits::Array(items) => items
            .iter()
            .enumerate()
            .map(|(i, value)| (units(&i.to_string()), value.clone()))
            .collect(),
        JsonUnits::String(text) => text
            .iter()
            .enumerate()
            .map(|(i, unit)| (units(&i.to_string()), JsonUnits::String(vec![*unit])))
            .collect(),
        _ => Vec::new(),
    }
}
fn same_js_primitive(a: &JsonUnits, b: &JsonUnits) -> bool {
    match (a, b) {
        (JsonUnits::Null, JsonUnits::Null) => true,
        (JsonUnits::Bool(a), JsonUnits::Bool(b)) => a == b,
        (JsonUnits::Number(a), JsonUnits::Number(b)) => a == b,
        (JsonUnits::String(a), JsonUnits::String(b)) => a == b,
        _ => false,
    }
}
impl MetadataTypeManager {
    pub fn load(config: Option<&Value>) -> Self {
        Self::load_units(config.map(JsonUnits::from_value).as_ref())
    }
    pub fn load_units(config: Option<&JsonUnits>) -> Self {
        let mut result = Self::default();
        if result.load_data_units(config, std::iter::empty()).is_ok() {
            if let Err(error) = result.rebuild(std::iter::empty()) {
                result.last_load_error = Some(TypeLoadError {
                    name: "Error",
                    message: error.to_string(),
                });
            }
        }
        result
    }
    pub fn last_load_error(&self) -> Option<&TypeLoadError> {
        self.last_load_error.as_ref()
    }
    fn record_case_failure(&mut self, error: UnicodeCaseError) -> TypeLoadError {
        let error = TypeLoadError {
            name: "Error",
            message: error.to_string(),
        };
        self.last_load_error = Some(error.clone());
        error
    }
    pub fn is_loaded(&self) -> bool {
        self.loaded
    }
    /// Pinned loadData emits changed/rebuild during the config loop, before
    /// defaults are appended. A final rebuild is not implied by its completion.
    pub fn load_data_units<'a>(
        &mut self,
        config: Option<&JsonUnits>,
        caches: impl IntoIterator<Item = &'a CachedMetadata>,
    ) -> Result<(), TypeLoadError> {
        let caches = caches.into_iter().collect::<Vec<_>>();
        let old = std::mem::take(&mut self.assigned);
        self.last_load_error = None;
        let types = config.and_then(|v| v.property(&units("types")));
        let shadow = if config.is_some_and(|v| v.property(&units("hasOwnProperty")).is_some()) {
            Some("e.hasOwnProperty is not a function")
        } else if types.is_some_and(|v| v.property(&units("hasOwnProperty")).is_some()) {
            Some("n.hasOwnProperty is not a function")
        } else {
            None
        };
        if let Some(message) = shadow {
            let error = TypeLoadError {
                name: "TypeError",
                message: message.into(),
            };
            self.last_load_error = Some(error.clone());
            return Err(error);
        }
        if let Some(types) = types {
            for (name, widget) in config_entries(types) {
                let lower = lower_units(&name).map_err(|error| self.record_case_failure(error))?;
                // Assignment to __proto__ is not an own assigned widget, but it
                // still emits the source changed/rebuild event during loading.
                if lower != units("__proto__") {
                    self.assigned
                        .entry(lower.clone())
                        .or_insert_with(|| PropertyInfoUnits {
                            name,
                            widget: widget.clone(),
                        });
                }
                let unchanged = old
                    .get(&lower)
                    .is_some_and(|info| same_js_primitive(&info.widget, &widget));
                if self.loaded && !unchanged {
                    self.rebuild(caches.iter().copied())
                        .map_err(|error| self.record_case_failure(error))?;
                }
            }
        }
        for (key, default) in defaults() {
            self.assigned.insert(key, default);
        }
        self.loaded = true;
        Ok(())
    }
    pub fn assigned_widget_units(
        &self,
        name: &[u16],
    ) -> Result<Option<&JsonUnits>, UnicodeCaseError> {
        Ok(self
            .assigned
            .get(&lower_units(name)?)
            .map(|info| &info.widget))
    }
    pub fn rebuild<'a>(
        &mut self,
        caches: impl IntoIterator<Item = &'a CachedMetadata>,
    ) -> Result<(), UnicodeCaseError> {
        self.rebuild_with(caches, lower_units)
    }
    fn rebuild_with<'a>(
        &mut self,
        caches: impl IntoIterator<Item = &'a CachedMetadata>,
        mut lowercase: impl FnMut(&[u16]) -> Result<Vec<u16>, UnicodeCaseError>,
    ) -> Result<(), UnicodeCaseError> {
        let mut properties = self.assigned.clone();
        let mut occurrences = IndexMap::new();
        let mut undefined = std::collections::BTreeSet::new();
        for cache in caches {
            for Field {
                name,
                is_null,
                widget,
            } in fields(cache)
            {
                let lower = lowercase(&name)?;
                if lower == units("__proto__") {
                    continue;
                }
                *occurrences.entry(lower.clone()).or_default() += 1;
                if !properties.contains_key(&lower) {
                    if is_null {
                        undefined.insert(lower.clone());
                    }
                    properties.insert(
                        lower.clone(),
                        PropertyInfoUnits {
                            name,
                            widget: JsonUnits::String(units(widget)),
                        },
                    );
                } else if undefined.contains(&lower) && !is_null {
                    properties.get_mut(&lower).unwrap().widget = JsonUnits::String(units(widget));
                    undefined.remove(&lower);
                }
            }
        }
        for info in properties.values_mut() {
            if matches!(info.widget, JsonUnits::Null) {
                info.widget = JsonUnits::String(units("text"));
            }
        }
        self.properties = properties;
        self.occurrences = occurrences;
        Ok(())
    }
    pub fn recount<'a>(
        &mut self,
        caches: impl IntoIterator<Item = &'a CachedMetadata>,
    ) -> Result<(), UnicodeCaseError> {
        let mut occurrences = IndexMap::new();
        for cache in caches {
            for Field { name, .. } in fields(cache) {
                let lower = lower_units(&name)?;
                if lower != units("__proto__") {
                    *occurrences.entry(lower).or_default() += 1;
                }
            }
        }
        self.occurrences = occurrences;
        Ok(())
    }
    pub fn cache_changed(&mut self, cache: &CachedMetadata) -> Result<(), UnicodeCaseError> {
        for Field {
            name,
            is_null,
            widget,
        } in fields(cache)
        {
            let lower = lower_units(&name)?;
            if lower == units("__proto__") || self.assigned.contains_key(&lower) {
                continue;
            }
            if let Some(info) = self.properties.get_mut(&lower) {
                if !is_null {
                    info.name = name;
                    info.widget = JsonUnits::String(units(widget));
                }
            } else {
                self.properties.insert(
                    lower,
                    PropertyInfoUnits {
                        name,
                        widget: JsonUnits::String(units(widget)),
                    },
                );
            }
        }
        Ok(())
    }
    /// Assigned state only; inferred properties must not become saved overrides.
    pub fn assigned_infos_units(&self) -> Vec<PropertyInfoUnits> {
        keys(&self.assigned)
            .into_iter()
            .map(|key| self.assigned[key].clone())
            .collect()
    }
    pub fn inferred_entries(
        cache: &CachedMetadata,
    ) -> Vec<crate::live_metadata_types::InferredProperty> {
        fields(cache)
            .into_iter()
            .map(|field| crate::live_metadata_types::InferredProperty {
                name: field.name,
                nonnull: !field.is_null,
                widget: field.widget,
            })
            .collect()
    }
    pub fn infos_units(&self) -> Vec<PropertyInfoUnits> {
        keys(&self.properties)
            .into_iter()
            .map(|key| self.properties[key].clone())
            .collect()
    }
    pub fn info_units(&self, name: &[u16]) -> Result<PropertyInfoUnits, UnicodeCaseError> {
        Ok(self
            .properties
            .get(&lower_units(name)?)
            .cloned()
            .unwrap_or_else(|| PropertyInfoUnits {
                name: name.to_vec(),
                widget: JsonUnits::String(units("text")),
            }))
    }
    pub fn occurrence_entries_units(&self) -> Vec<(Vec<u16>, usize)> {
        keys(&self.occurrences)
            .into_iter()
            .map(|key| (key.to_vec(), self.occurrences[key]))
            .collect()
    }
    pub fn occurrences_units(&self, name: &[u16]) -> Result<usize, UnicodeCaseError> {
        Ok(*self.occurrences.get(&lower_units(name)?).unwrap_or(&0))
    }
    pub fn set_type_units(&mut self, name: &[u16], widget: &[u16]) -> Result<(), UnicodeCaseError> {
        self.set_type_value_units(name, JsonUnits::String(widget.to_vec()))
    }
    pub fn set_type_value_units(
        &mut self,
        name: &[u16],
        widget: JsonUnits,
    ) -> Result<(), UnicodeCaseError> {
        let lower = lower_units(name)?;
        if let Some(info) = self.assigned.get_mut(&lower) {
            info.widget = widget;
        } else if lower != units("__proto__") && lower != units("constructor") {
            self.assigned.insert(
                lower,
                PropertyInfoUnits {
                    name: name.to_vec(),
                    widget,
                },
            );
        }
        Ok(())
    }
    /// Compatibility projection for UTF8-representable data. Use infos_units to
    /// enumerate every native name; unpaired keys are never collapsed to U+FFFD.
    pub fn infos(&self) -> Vec<PropertyInfo> {
        self.infos_units()
            .iter()
            .filter_map(PropertyInfoUnits::try_utf8)
            .collect()
    }
    pub fn try_info_utf8(&self, name: &str) -> Result<Option<PropertyInfo>, UnicodeCaseError> {
        Ok(self.info_units(&units(name))?.try_utf8())
    }
    pub fn info(&self, name: &str) -> Result<PropertyInfo, MetadataTypeError> {
        self.try_info_utf8(name)?
            .ok_or(MetadataTypeError::Utf8Projection)
    }
    pub fn occurrences(&self, name: &str) -> Result<usize, UnicodeCaseError> {
        self.occurrences_units(&units(name))
    }
    pub fn set_type(&mut self, name: &str, widget: &str) -> Result<(), UnicodeCaseError> {
        self.set_type_units(&units(name), &units(widget))
    }
    pub fn config_units(&self) -> JsonUnits {
        let mut types: Vec<(Vec<u16>, JsonUnits)> = Vec::new();
        for key in keys(&self.assigned) {
            let info = &self.assigned[key];
            if let Some((_, old)) = types.iter_mut().find(|(name, _)| *name == info.name) {
                *old = info.widget.clone();
            } else {
                types.push((info.name.clone(), info.widget.clone()));
            }
        }
        JsonUnits::Object(vec![(units("types"), JsonUnits::Object(types))])
    }
    pub fn config(&self) -> Result<Value, MetadataTypeError> {
        self.config_units()
            .to_value()
            .ok_or(MetadataTypeError::Utf8Projection)
    }
}

#[cfg(test)]
mod casing_failure_tests {
    use super::*;
    #[test]
    fn failed_case_conversion_keeps_previous_completed_rebuild_state() {
        let old = CachedMetadata::parse("---\nOld: 1\n---\n");
        let replacement = CachedMetadata::parse("---\nNew: true\nUnavailable: text\n---\n");
        let mut manager = MetadataTypeManager::load(None);
        manager.rebuild([&old]).unwrap();
        let before = manager.infos_units();
        let counts = manager.occurrence_entries_units();
        let mut calls = 0;
        let outcome = manager.rebuild_with([&replacement], |name| {
            calls += 1;
            if calls == 2 {
                Err(UnicodeCaseError(
                    "injected pinned ICU initialization failure".into(),
                ))
            } else {
                lower_units(name)
            }
        });
        assert_eq!(
            outcome.unwrap_err().to_string(),
            "injected pinned ICU initialization failure"
        );
        assert_eq!(manager.infos_units(), before);
        assert_eq!(manager.occurrence_entries_units(), counts);
    }
}
