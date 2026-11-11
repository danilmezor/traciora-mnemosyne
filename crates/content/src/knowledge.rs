//! Read-only Markdown projections. Section IDs are ordinal and revision-scoped:
//! s0 is the body preamble; s1.. are headings (duplicates remain distinct).
//! A heading section includes descendants up to the next same-or-lower heading.
//! Byte ranges are half-open UTF-8 offsets; line ranges are one-based inclusive
//! (empty spans identify their insertion line). CommonMark ATX/Setext headings
//! are supported; fenced code is excluded and optional Markdown extensions are
//! disabled. YAML frontmatter starts with an exact `---` first line after an
//! optional BOM and closes with `---` or `...`; malformed frontmatter fails closed.
//! It supports JSON-compatible mappings, arrays and scalars, with unique string
//! keys, at most 4096 values, 64 KiB of decoded strings and 32 nesting levels. YAML tags/nonfinite numbers
//! are unsupported. Text is capped at 2 MiB, frontmatter at 64 KiB, headings at
//! 4096. Projections carry provenance only and never grant authority.
use crate::{auth::TrustedIdentity, err, ReadResult, Result, Service};
use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use serde::{
    de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor},
    Deserialize, Serialize,
};
use serde_json::{Map, Value};
use std::{cell::Cell, rc::Rc};

pub const MAX_MARKDOWN_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_FRONTMATTER_BYTES: usize = 64 * 1024;
pub const MAX_HEADINGS: usize = 4096;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KnowledgeSource {
    pub path: String,
    pub revision_id: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_line: usize,
    pub end_line: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PropertiesResult {
    pub source: KnowledgeSource,
    pub properties: Map<String, Value>,
    pub frontmatter: Option<KnowledgeSource>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Heading {
    pub section_id: String,
    pub level: u8,
    pub title: String,
    pub source: KnowledgeSource,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OutlineResult {
    pub source: KnowledgeSource,
    pub preamble: KnowledgeSource,
    pub headings: Vec<Heading>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SectionResult {
    pub source: KnowledgeSource,
    pub section_id: String,
    pub content: String,
}

impl Service {
    pub fn properties(
        &self,
        who: &TrustedIdentity,
        path: &str,
        revision: Option<&str>,
    ) -> Result<PropertiesResult> {
        let read = self.read(who, path, revision)?;
        let parsed = Parsed::new(&read)?;
        Ok(PropertiesResult {
            source: source(&read, 0, read.content.len()),
            properties: parsed.properties,
            frontmatter: (parsed.body_start
                > usize::from(read.content.starts_with('\u{feff}')) * 3)
                .then(|| source(&read, 0, parsed.body_start)),
        })
    }
    pub fn outline(
        &self,
        who: &TrustedIdentity,
        path: &str,
        revision: Option<&str>,
    ) -> Result<OutlineResult> {
        let read = self.read(who, path, revision)?;
        Parsed::new(&read)?.outline(&read)
    }
    /// One section by `section_id` or by `section`, a section id or else the exact
    /// title of a heading, the first in document order when several share it
    ///. Exactly one of them, non-empty and at most 1,024 bytes.
    pub fn read_section(
        &self,
        who: &TrustedIdentity,
        path: &str,
        revision: Option<&str>,
        section_id: Option<&str>,
        section: Option<&str>,
    ) -> Result<SectionResult> {
        let (selector, by_title) = match (section_id, section) {
            (Some(id), None) => (id, false),
            (None, Some(section)) => (section, true),
            _ => return Err(err("invalid_section")),
        };
        if selector.is_empty() || selector.len() > MAX_SECTION_SELECTOR {
            return Err(err("invalid_section"));
        }
        let read = self.read(who, path, revision)?;
        let outline = Parsed::new(&read)?.outline(&read)?;
        let by_id = |id: &str| {
            if id == "s0" {
                return Some(outline.preamble.clone());
            }
            outline
                .headings
                .iter()
                .find(|h| h.section_id == id)
                .map(|h| h.source.clone())
        };
        let (section_id, location) = by_id(selector)
            .map(|l| (selector.to_string(), l))
            .or_else(|| {
                by_title
                    .then(|| outline.headings.iter().find(|h| h.title == selector))
                    .flatten()
                    .map(|h| (h.section_id.clone(), h.source.clone()))
            })
            .ok_or_else(|| err("section_not_found"))?;
        Ok(SectionResult {
            content: read.content[location.start_byte..location.end_byte].into(),
            source: location,
            section_id,
        })
    }
}

/// `section`/`section_id` of `read_section` at most this long.
pub const MAX_SECTION_SELECTOR: usize = 1024;

/// The section of a search hit: the deepest heading whose range holds its offset.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HitSection {
    pub section_id: String,
    pub title: String,
}

/// The outline of a text already read, as `outline` computes it.
pub(crate) fn outline_of(read: &ReadResult) -> Result<OutlineResult> {
    Parsed::new(read)?.outline(read)
}

/// The deepest heading containing byte `offset`: nested headings start later and end no
/// later, so it is the last one in document order that contains it. `None` in the
/// preamble.
pub(crate) fn section_at(outline: &OutlineResult, offset: usize) -> Option<HitSection> {
    outline
        .headings
        .iter()
        .rfind(|h| h.source.start_byte <= offset && offset < h.source.end_byte)
        .map(|h| HitSection {
            section_id: h.section_id.clone(),
            title: h.title.clone(),
        })
}

fn source(read: &ReadResult, start: usize, end: usize) -> KnowledgeSource {
    let line = |offset: usize| {
        1 + read.content.as_bytes()[..offset]
            .iter()
            .filter(|b| **b == b'\n')
            .count()
    };
    KnowledgeSource {
        path: read.path.clone(),
        revision_id: read.revision_id.clone(),
        start_byte: start,
        end_byte: end,
        start_line: line(start),
        end_line: line(if end > start { end - 1 } else { start }),
    }
}
struct Parsed {
    properties: Map<String, Value>,
    body_start: usize,
}
impl Parsed {
    fn new(read: &ReadResult) -> Result<Self> {
        if read.content.len() > MAX_MARKDOWN_BYTES {
            return Err(err("knowledge_limit"));
        }
        // Parsing any governed text is intentional: extensions do not establish type or access.
        let (properties, body_start) = front_matter(&read.content);
        Ok(Self {
            properties: properties?,
            body_start,
        })
    }
    fn outline(&self, read: &ReadResult) -> Result<OutlineResult> {
        let starts: Vec<usize> = std::iter::once(0)
            .chain(
                read.content
                    .bytes()
                    .enumerate()
                    .filter_map(|(i, b)| (b == b'\n').then_some(i + 1)),
            )
            .collect();
        let at = |start: usize, end: usize| KnowledgeSource {
            path: read.path.clone(),
            revision_id: read.revision_id.clone(),
            start_byte: start,
            end_byte: end,
            start_line: starts.partition_point(|n| *n <= start),
            end_line: starts.partition_point(|n| *n <= if end > start { end - 1 } else { start }),
        };
        let found = scan_headings(&read.content[self.body_start..], MAX_HEADINGS);
        if found.len() > MAX_HEADINGS {
            return Err(err("knowledge_limit"));
        }
        let mut headings: Vec<Heading> = found
            .into_iter()
            .enumerate()
            .map(|(i, (level, start, title))| Heading {
                section_id: format!("s{}", i + 1),
                level,
                title,
                source: at(self.body_start + start, read.content.len()),
            })
            .collect();
        for i in 0..headings.len() {
            let end = headings[i + 1..]
                .iter()
                .find(|h| h.level <= headings[i].level)
                .map(|h| h.source.start_byte)
                .unwrap_or(read.content.len());
            headings[i].source = at(headings[i].source.start_byte, end);
        }
        Ok(OutlineResult {
            source: at(0, read.content.len()),
            preamble: at(
                self.body_start,
                headings
                    .first()
                    .map(|h| h.source.start_byte)
                    .unwrap_or(read.content.len()),
            ),
            headings,
        })
    }
}

/// The frontmatter properties of a text and where its body starts. The body starts
/// after a closing fence even when the YAML between the fences is invalid; a text
/// whose fence never closes, or whose frontmatter is over the bound, has its body at
/// the start (after any BOM).
pub(crate) fn front_matter(content: &str) -> (Result<Map<String, Value>>, usize) {
    let text = content.strip_prefix('\u{feff}').unwrap_or(content);
    let bom = content.len() - text.len();
    let mut lines = text.split_inclusive('\n');
    if lines.next().map(|l| l.trim_end_matches(['\r', '\n'])) != Some("---") {
        return (Ok(Map::new()), bom);
    }
    let opening = text.find('\n').map(|n| n + 1).unwrap_or(text.len());
    let mut end = opening;
    for line in lines {
        if end - opening > MAX_FRONTMATTER_BYTES {
            return (Err(err("knowledge_limit")), bom);
        }
        if matches!(line.trim_end_matches(['\r', '\n']), "---" | "...") {
            let body_start = bom + end + line.len();
            return (yaml_properties(&text[opening..end]), body_start);
        }
        end += line.len();
    }
    (Err(err("invalid_frontmatter")), bom)
}

fn yaml_properties(yaml: &str) -> Result<Map<String, Value>> {
    let mut documents = serde_yaml::Deserializer::from_str(yaml);
    let properties = if let Some(doc) = documents.next() {
        let value = Seed {
            remaining: Rc::new(Cell::new(4096)),
            bytes: Rc::new(Cell::new(MAX_FRONTMATTER_BYTES)),
            depth: 0,
        }
        .deserialize(doc)
        .map_err(|_| err("invalid_frontmatter"))?;
        match value {
            Value::Object(map) => map,
            Value::Null => Map::new(),
            _ => return Err(err("invalid_frontmatter")),
        }
    } else {
        Map::new()
    };
    if documents.next().is_some() {
        return Err(err("invalid_frontmatter"));
    }
    Ok(properties)
}

/// The headings of a body in document order as `(level, start byte in body, title)`;
/// at most `cap + 1` of them, so a caller can tell that the cap was passed. Fenced and
/// indented code is not a heading.
pub(crate) fn scan_headings(body: &str, cap: usize) -> Vec<(u8, usize, String)> {
    let mut headings = Vec::new();
    let mut active: Option<(u8, usize, String)> = None;
    for (event, range) in Parser::new(body).into_offset_iter() {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                if headings.len() > cap {
                    break;
                }
                active = Some((level as u8, range.start, String::new()));
            }
            Event::Text(text) | Event::Code(text) => {
                if let Some((_, _, title)) = &mut active {
                    title.push_str(&text);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if let Some((_, _, title)) = &mut active {
                    title.push(' ');
                }
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some(heading) = active.take() {
                    headings.push(heading);
                }
            }
            _ => {}
        }
    }
    headings
}

// Budget allocations while deserializing, including YAML aliases; never first
// expand an unbounded YAML tree. Only JSON-compatible, unique string keys survive.
#[derive(Clone)]
struct Seed {
    remaining: Rc<Cell<usize>>,
    bytes: Rc<Cell<usize>>,
    depth: usize,
}
impl<'de> DeserializeSeed<'de> for Seed {
    type Value = Value;
    fn deserialize<D: de::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> std::result::Result<Value, D::Error> {
        if self.depth > 32 || self.remaining.get() == 0 {
            return Err(de::Error::custom("frontmatter limit"));
        }
        self.remaining.set(self.remaining.get() - 1);
        deserializer.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for Seed {
    type Value = Value;
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("bounded JSON-compatible YAML")
    }
    fn visit_bool<E: de::Error>(self, v: bool) -> std::result::Result<Value, E> {
        Ok(v.into())
    }
    fn visit_i64<E: de::Error>(self, v: i64) -> std::result::Result<Value, E> {
        Ok(v.into())
    }
    fn visit_u64<E: de::Error>(self, v: u64) -> std::result::Result<Value, E> {
        Ok(v.into())
    }
    fn visit_f64<E: de::Error>(self, v: f64) -> std::result::Result<Value, E> {
        serde_json::Number::from_f64(v)
            .map(Value::Number)
            .ok_or_else(|| E::custom("nonfinite number"))
    }
    fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<Value, E> {
        charge_bytes::<E>(&self.bytes, v.len())?;
        Ok(v.into())
    }
    fn visit_string<E: de::Error>(self, v: String) -> std::result::Result<Value, E> {
        charge_bytes::<E>(&self.bytes, v.len())?;
        Ok(v.into())
    }
    fn visit_unit<E: de::Error>(self) -> std::result::Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_none<E: de::Error>(self) -> std::result::Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<Value, A::Error> {
        let mut values = vec![];
        let child = Seed {
            remaining: self.remaining,
            bytes: self.bytes.clone(),
            depth: self.depth + 1,
        };
        while let Some(v) = seq.next_element_seed(child.clone())? {
            values.push(v);
        }
        Ok(Value::Array(values))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Value, A::Error> {
        let mut values = Map::new();
        let child = Seed {
            remaining: self.remaining,
            bytes: self.bytes.clone(),
            depth: self.depth + 1,
        };
        while let Some(key) = map.next_key_seed(KeySeed(self.bytes.clone()))? {
            if values.contains_key(&key) {
                return Err(de::Error::custom("duplicate property"));
            }
            values.insert(key, map.next_value_seed(child.clone())?);
        }
        Ok(Value::Object(values))
    }
}

struct KeySeed(Rc<Cell<usize>>);
impl<'de> DeserializeSeed<'de> for KeySeed {
    type Value = String;
    fn deserialize<D: de::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> std::result::Result<String, D::Error> {
        deserializer.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for KeySeed {
    type Value = String;
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a string property key")
    }
    fn visit_str<E: de::Error>(self, value: &str) -> std::result::Result<String, E> {
        charge_bytes::<E>(&self.0, value.len())?;
        Ok(value.into())
    }
    fn visit_string<E: de::Error>(self, value: String) -> std::result::Result<String, E> {
        charge_bytes::<E>(&self.0, value.len())?;
        Ok(value)
    }
}

fn charge_bytes<E: de::Error>(budget: &Cell<usize>, length: usize) -> std::result::Result<(), E> {
    let remaining = budget
        .get()
        .checked_sub(length)
        .ok_or_else(|| E::custom("frontmatter string budget"))?;
    budget.set(remaining);
    Ok(())
}
