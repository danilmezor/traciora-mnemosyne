//! Values crossing the native YAML/JavaScript boundary. Aliases are node IDs.
use serde::{Deserialize, Serialize};
pub type NodeId = usize;
/// An own property name. Unpaired UTF16 units retain their native identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ObjectKey {
    String(String),
    Wtf16(Vec<u16>),
}
impl ObjectKey {
    pub fn from_units(units: Vec<u16>) -> Self {
        match String::from_utf16(&units) {
            Ok(value) => Self::String(value),
            Err(_) => Self::Wtf16(units),
        }
    }
    pub fn units(&self) -> Vec<u16> {
        match self {
            Self::String(value) => value.encode_utf16().collect(),
            Self::Wtf16(units) => units.clone(),
        }
    }
    pub fn text(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            Self::Wtf16(_) => None,
        }
    }
}

#[derive(Clone, Debug)]
pub enum Node {
    Null,
    Bool(bool),
    Number(serde_yaml::Number),
    String(String),
    /// A JavaScript string containing at least one unpaired UTF16 surrogate.
    Wtf16String(Vec<u16>),
    Object(Vec<(String, NodeId)>),
    ObjectWithUtf16Keys(Vec<(ObjectKey, NodeId)>),
    Array(Vec<NodeId>),
    Date(i64),
    Binary(Vec<u8>),
    Set(Vec<NodeId>),
    Map(Vec<(NodeId, NodeId)>),
    /// Explicit merge-key resolver, consumed when constructing its mapping.
    Merge,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct Diagnostic {
    pub name: String,
    /// UTF8 presentation boundary. Native messages use message_units when present.
    pub message: String,
    #[serde(skip)]
    pub message_units: Option<Vec<u16>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pos: Option<[usize; 2]>,
    #[serde(rename = "linePos", skip_serializing_if = "Option::is_none")]
    pub line_pos: Option<[LinePosition; 2]>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct LinePosition {
    pub line: usize,
    pub col: usize,
}
impl Diagnostic {
    pub fn exception(name: &str, message: &str) -> Self {
        Self {
            name: name.into(),
            message: message.into(),
            message_units: None,
            code: None,
            pos: None,
            line_pos: None,
        }
    }
}

/// ECMAScript JSON string quoting over UTF16, including unpaired surrogates.
pub fn quote_units(units: &[u16]) -> String {
    use std::fmt::Write;
    let mut output = String::from("\"");
    let mut index = 0;
    while index < units.len() {
        let unit = units[index];
        index += 1;
        match unit {
            0x22 => output.push_str("\\\""),
            0x5c => output.push_str("\\\\"),
            0x08 => output.push_str("\\b"),
            0x0c => output.push_str("\\f"),
            0x0a => output.push_str("\\n"),
            0x0d => output.push_str("\\r"),
            0x09 => output.push_str("\\t"),
            0..=0x1f => {
                write!(output, "\\u{unit:04x}").unwrap();
            }
            0xd800..=0xdbff
                if units
                    .get(index)
                    .is_some_and(|next| (0xdc00..=0xdfff).contains(next)) =>
            {
                let low = units[index];
                index += 1;
                output.push(
                    char::from_u32(
                        0x10000 + ((unit as u32 - 0xd800) << 10) + (low as u32 - 0xdc00),
                    )
                    .unwrap(),
                );
            }
            0xd800..=0xdfff => {
                write!(output, "\\u{unit:04x}").unwrap();
            }
            _ => output.push(char::from_u32(unit as u32).unwrap()),
        }
    }
    output.push('"');
    output
}

impl Serialize for Diagnostic {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("name", &self.name)?;
        if let Some(units) = &self.message_units {
            let raw = serde_json::value::RawValue::from_string(quote_units(units))
                .map_err(serde::ser::Error::custom)?;
            map.serialize_entry("message", &raw)?;
        } else {
            map.serialize_entry("message", &self.message)?;
        }
        if let Some(v) = &self.code {
            map.serialize_entry("code", v)?;
        }
        if let Some(v) = &self.pos {
            map.serialize_entry("pos", v)?;
        }
        if let Some(v) = &self.line_pos {
            map.serialize_entry("linePos", v)?;
        }
        map.end()
    }
}
