//! Metadata-cache representation as plugins see it, separate from internal byte spans.
//!
//! Cache offsets and columns count UTF-16 code units in the complete source
//! after a leading BOM is removed, matching the desktop parser;
//! the parsing and rewriting models continue to use UTF-8 byte offsets.

use std::collections::BTreeMap;

use pulldown_cmark::{Event, Options, Parser};
mod persistence;
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use crate::Note;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheLoc {
    pub line: usize,
    pub col: usize,
    pub offset: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachePosition {
    pub start: CacheLoc,
    pub end: CacheLoc,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferenceCache {
    pub position: CachePosition,
    pub link: String,
    pub original: String,
    #[serde(rename = "displayText")]
    pub display_text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontmatterLinkCache {
    pub key: String,
    pub key_units: Option<Vec<u16>>,
    pub link: String,
    pub original: String,
    pub display_text: String,
}

impl Serialize for FrontmatterLinkCache {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(4))?;
        if let Some(units) = &self.key_units {
            let raw =
                serde_json::value::RawValue::from_string(crate::yaml_value::quote_units(units))
                    .map_err(serde::ser::Error::custom)?;
            map.serialize_entry("key", &raw)?
        } else {
            map.serialize_entry("key", &self.key)?
        }
        map.serialize_entry("link", &self.link)?;
        map.serialize_entry("original", &self.original)?;
        map.serialize_entry("displayText", &self.display_text)?;
        map.end()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagCache {
    pub position: CachePosition,
    pub tag: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeadingCache {
    pub position: CachePosition,
    pub heading: String,
    pub level: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SectionCache {
    #[serde(rename = "type")]
    pub kind: String,
    pub position: CachePosition,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListItemCache {
    pub position: CachePosition,
    pub parent: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockCache {
    pub position: CachePosition,
    pub id: String,
}

pub type FootnoteCache = BlockCache;
pub type FootnoteRefCache = BlockCache;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferenceLinkCache {
    pub position: CachePosition,
    pub id: String,
    pub link: String,
}

#[derive(Debug, Clone, Default)]
pub struct CachedMetadata {
    /// Present only on metadata hydrated from a validated persistent record.
    pub version: Option<u32>,
    pub links: Vec<ReferenceCache>,
    pub embeds: Vec<ReferenceCache>,
    pub tags: Vec<TagCache>,
    pub headings: Vec<HeadingCache>,
    pub footnotes: Vec<FootnoteCache>,
    pub footnote_refs: Vec<FootnoteRefCache>,
    pub reference_links: Vec<ReferenceLinkCache>,
    pub sections: Vec<SectionCache>,
    pub list_items: Vec<ListItemCache>,
    pub blocks: BTreeMap<String, BlockCache>,
    pub frontmatter: Option<serde_yaml::Mapping>,
    /// Native constructor types and alias identity, independent of JSON projection.
    pub native_frontmatter: Option<std::sync::Arc<crate::yaml_graph::Document>>,
    pub frontmatter_position: Option<CachePosition>,
    pub frontmatter_links: Option<Vec<FrontmatterLinkCache>>,
}

impl serde::Serialize for CachedMetadata {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(None)?;
        macro_rules! sequence {
            ($field:ident,$name:literal) => {
                if !self.$field.is_empty() {
                    map.serialize_entry($name, &self.$field)?;
                }
            };
        }
        sequence!(links, "links");
        sequence!(embeds, "embeds");
        sequence!(tags, "tags");
        sequence!(headings, "headings");
        sequence!(footnotes, "footnotes");
        sequence!(footnote_refs, "footnoteRefs");
        sequence!(reference_links, "referenceLinks");
        sequence!(sections, "sections");
        sequence!(list_items, "listItems");
        sequence!(blocks, "blocks");
        if let Some(document) = &self.native_frontmatter {
            let text = document.json(false).map_err(serde::ser::Error::custom)?;
            if document.nodes.iter().any(|node| {
                matches!(
                    node,
                    crate::yaml_value::Node::Wtf16String(_)
                        | crate::yaml_value::Node::ObjectWithUtf16Keys(_)
                )
            }) {
                let value = serde_json::value::RawValue::from_string(text)
                    .map_err(serde::ser::Error::custom)?;
                map.serialize_entry("frontmatter", &value)?;
            } else {
                let value: serde_yaml::Value =
                    serde_yaml::from_str(&text).map_err(serde::ser::Error::custom)?;
                map.serialize_entry("frontmatter", &value)?;
            }
        } else if let Some(frontmatter) = &self.frontmatter {
            map.serialize_entry("frontmatter", frontmatter)?;
        }
        if let Some(position) = &self.frontmatter_position {
            map.serialize_entry("frontmatterPosition", position)?;
        }
        if let Some(links) = &self.frontmatter_links {
            map.serialize_entry("frontmatterLinks", links)?;
        }
        if let Some(version) = self.version {
            map.serialize_entry("v", &version)?;
        }
        map.end()
    }
}

/// Convert byte boundaries to full-source UTF-16 locations in logarithmic time.
/// Only non-ASCII width changes need an entry in the offset adjustment table.
pub struct SourcePositions<'a> {
    source: &'a str,
    lines: Vec<usize>,
    adjustments: Vec<(usize, usize)>,
}

impl<'a> SourcePositions<'a> {
    pub fn new(source: &'a str) -> Self {
        let mut lines = vec![0];
        let mut adjustments = Vec::new();
        let mut excess = 0;
        for (byte, c) in source.char_indices() {
            if c == '\n' || (c == '\r' && source.as_bytes().get(byte + 1) != Some(&b'\n')) {
                lines.push(byte + 1);
            }
            if c.len_utf8() != c.len_utf16() {
                excess += c.len_utf8() - c.len_utf16();
                adjustments.push((byte + c.len_utf8(), excess));
            }
        }
        Self {
            source,
            lines,
            adjustments,
        }
    }

    pub fn loc(&self, byte: usize) -> CacheLoc {
        assert!(
            self.source.is_char_boundary(byte),
            "cache location must be a UTF-8 boundary"
        );
        let line = self.lines.partition_point(|&start| start <= byte) - 1;
        let offset = self.utf16_offset(byte);
        CacheLoc {
            line,
            col: offset - self.utf16_offset(self.lines[line]),
            offset,
        }
    }

    pub fn range(&self, start: usize, end: usize) -> CachePosition {
        CachePosition {
            start: self.loc(start),
            end: self.loc(end),
        }
    }

    /// Convert a cache offset back to a UTF-8 boundary for source slicing.
    /// Offsets inside a surrogate pair have no corresponding byte boundary.
    pub fn byte_at_utf16_offset(&self, offset: usize) -> Option<usize> {
        let mut low = if self.source.starts_with('\u{feff}') {
            3
        } else {
            0
        };
        let mut high = self.source.len();
        while low <= high {
            let mut middle = low + (high - low) / 2;
            while !self.source.is_char_boundary(middle) {
                middle -= 1;
            }
            match self.utf16_offset(middle).cmp(&offset) {
                std::cmp::Ordering::Equal => return Some(middle),
                std::cmp::Ordering::Less => {
                    low = middle + self.source[middle..].chars().next()?.len_utf8()
                }
                std::cmp::Ordering::Greater => {
                    if middle == 0 {
                        return None;
                    }
                    high = middle - 1;
                }
            }
        }
        None
    }

    fn utf16_offset(&self, byte: usize) -> usize {
        let index = self.adjustments.partition_point(|&(end, _)| end <= byte);
        byte - if index == 0 {
            0
        } else {
            self.adjustments[index - 1].1
        } - usize::from(self.source.starts_with('\u{feff}') && byte >= 3)
    }
}

/// Failure of the shipped metadata worker's frontmatter traversal or clone.
/// The file remains in cache chronology; its metadata result is absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MetadataWorkerFailure {
    #[error("frontmatter link traversal contains a cycle")]
    FrontmatterCycle,
    #[error("frontmatter shadows hasOwnProperty with a non-function")]
    ShadowedHasOwnProperty,
    #[error("frontmatter contains an uncloneable Symbol")]
    UncloneableSymbol,
}
fn metadata_worker_failure(
    document: &crate::yaml_graph::Document,
) -> Option<MetadataWorkerFailure> {
    use crate::yaml_value::{Node, NodeId};
    fn traverse(
        document: &crate::yaml_graph::Document,
        id: NodeId,
        active: &mut std::collections::BTreeSet<NodeId>,
    ) -> Option<MetadataWorkerFailure> {
        if !matches!(
            document.nodes[id],
            Node::Object(_) | Node::ObjectWithUtf16Keys(_) | Node::Array(_)
        ) {
            return None;
        }
        if !active.insert(id) {
            return Some(MetadataWorkerFailure::FrontmatterCycle);
        }
        match &document.nodes[id] {
            Node::ObjectWithUtf16Keys(fields) => {
                if fields
                    .iter()
                    .any(|(key, _)| key.text() == Some("hasOwnProperty"))
                {
                    return Some(MetadataWorkerFailure::ShadowedHasOwnProperty);
                }
                for (_, id) in fields {
                    if let Some(failure) = traverse(document, *id, active) {
                        return Some(failure);
                    }
                }
            }
            Node::Object(fields) => {
                if fields.iter().any(|(key, _)| key == "hasOwnProperty") {
                    return Some(MetadataWorkerFailure::ShadowedHasOwnProperty);
                }
                for (_, id) in fields {
                    if let Some(failure) = traverse(document, *id, active) {
                        return Some(failure);
                    }
                }
            }
            Node::Array(values) => {
                for id in values {
                    if let Some(failure) = traverse(document, *id, active) {
                        return Some(failure);
                    }
                }
            }
            _ => unreachable!(),
        }
        active.remove(&id);
        None
    }
    fn cloneable(
        document: &crate::yaml_graph::Document,
        id: NodeId,
        seen: &mut std::collections::BTreeSet<NodeId>,
    ) -> bool {
        if !seen.insert(id) {
            return true;
        }
        match &document.nodes[id] {
            Node::Merge => false,
            Node::ObjectWithUtf16Keys(fields) => {
                fields.iter().all(|(_, id)| cloneable(document, *id, seen))
            }
            Node::Object(fields) => fields.iter().all(|(_, id)| cloneable(document, *id, seen)),
            Node::Array(values) | Node::Set(values) => {
                values.iter().all(|id| cloneable(document, *id, seen))
            }
            Node::Map(entries) => entries.iter().all(|(key, value)| {
                cloneable(document, *key, seen) && cloneable(document, *value, seen)
            }),
            _ => true,
        }
    }
    traverse(document, document.root, &mut Default::default()).or_else(|| {
        (!cloneable(document, document.root, &mut Default::default()))
            .then_some(MetadataWorkerFailure::UncloneableSymbol)
    })
}

impl CachedMetadata {
    pub fn frontmatter_strings(&self, property: &str) -> Vec<String> {
        if let Some(document) = &self.native_frontmatter {
            return document.frontmatter_strings(property);
        }
        let Some(mapping) = &self.frontmatter else {
            return Vec::new();
        };
        let Some((_, value)) = mapping.iter().find(|(key, _)| {
            key.as_str()
                .is_some_and(|key| key.eq_ignore_ascii_case(property))
        }) else {
            return Vec::new();
        };
        let strings = match value {
            serde_yaml::Value::String(value) => vec![value.as_str()],
            serde_yaml::Value::Sequence(values) => values
                .iter()
                .filter_map(serde_yaml::Value::as_str)
                .collect(),
            _ => Vec::new(),
        };
        strings
            .into_iter()
            .map(|value| {
                value
                    .trim_matches(crate::vault_search::js_whitespace)
                    .to_owned()
            })
            .collect()
    }

    pub fn try_parse(source: &str) -> Result<Self, MetadataWorkerFailure> {
        Self::try_from_note(&Note::parse("cache.md".into(), source.to_owned()))
    }
    pub fn try_from_note(note: &Note) -> Result<Self, MetadataWorkerFailure> {
        if let Some(frontmatter) = &note.frontmatter {
            if let Ok(document) = crate::yaml_graph::parse_frontmatter(&frontmatter.raw) {
                if matches!(
                    document.nodes[document.root],
                    crate::yaml_value::Node::Object(_)
                        | crate::yaml_value::Node::ObjectWithUtf16Keys(_)
                        | crate::yaml_value::Node::Date(_)
                        | crate::yaml_value::Node::Binary(_)
                        | crate::yaml_value::Node::Map(_)
                        | crate::yaml_value::Node::Set(_)
                ) {
                    if let Some(failure) = metadata_worker_failure(&document) {
                        return Err(failure);
                    }
                }
            }
        }
        Ok(Self::from_note(note))
    }

    pub fn parse(source: &str) -> Self {
        Self::from_note(&Note::parse("cache.md".into(), source.to_string()))
    }

    pub fn from_note(note: &Note) -> Self {
        let positions = SourcePositions::new(&note.source);
        let base = note.body_offset;
        let range =
            |start, end| positions.range(base + start, base + trimmed_end(&note.body, start, end));
        let mut cache = Self::default();
        for reference in note.references() {
            if !crate::link::is_internal_link(&reference.link) {
                continue;
            }
            if escaped_at(&note.body, reference.start) {
                continue;
            }
            let mut display_text = if let Some(wiki) = note
                .wikilinks
                .iter()
                .find(|wiki| wiki.start == reference.start)
            {
                wiki.alias.clone().unwrap_or_else(|| {
                    wiki.target
                        .split('#')
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                        .join(" > ")
                })
            } else {
                note.markdown_links
                    .iter()
                    .chain(&note.markdown_images)
                    .find(|link| link.start == reference.start)
                    .map(|link| link.text.clone())
                    .unwrap_or_default()
            };
            let embed = reference.original.starts_with('!');
            let mut link = reference
                .link
                .strip_suffix('\\')
                .unwrap_or(&reference.link)
                .to_owned();
            if let Some((href, title)) = wiki_cache_fields(&reference.original) {
                link = href;
                display_text = title;
            }
            let reference = ReferenceCache {
                position: range(reference.start, reference.end),
                // The worker normalizes wiki hrefs, while original/display
                // text retain their source spelling and composition.
                link,
                original: reference.original,
                display_text,
            };
            if embed {
                cache.embeds.push(reference);
            } else {
                cache.links.push(reference);
            }
        }
        // Image conversion uses decodeURI with a raw-value fallback; ordinary
        // Markdown links with invalid URI escapes are left unconverted.
        for image in &note.markdown_images {
            if crate::link::is_internal_link(&image.destination)
                && crate::rewrite::decode_uri(&image.destination).is_none()
            {
                cache.embeds.push(ReferenceCache {
                    position: range(image.start, image.end),
                    link: normalized_cache_href(&image.destination),
                    original: note.body[image.start..image.end].to_string(),
                    display_text: image.text.clone(),
                });
            }
        }
        cache.tags = note
            .tags
            .iter()
            .map(|tag| TagCache {
                position: range(tag.start, tag.end),
                tag: tag.name.clone(),
            })
            .collect();
        cache.headings = note
            .headings
            .iter()
            .filter(|heading| {
                note.sections.iter().any(|section| {
                    section.kind == "heading" && section.position.start.offset == heading.start
                })
            })
            .map(|heading| HeadingCache {
                position: range(heading.start, heading.end),
                heading: heading_source(&note.body, heading.start, heading.end),
                level: heading.level,
            })
            .collect();
        cache.sections = note
            .sections
            .iter()
            .map(|section| SectionCache {
                kind: section.kind.clone(),
                position: range(section.position.start.offset, section.position.end.offset),
                id: section.id.clone(),
            })
            .collect();
        cache.list_items = note
            .list_items
            .iter()
            .map(|item| ListItemCache {
                position: range(item.position.start.offset, item.position.end.offset),
                parent: full_source_parent(note, item.parent, item.position.start.offset),
                task: task_marker(&note.body[item.position.start.offset..item.position.end.offset])
                    .or_else(|| item.task.clone()),
                id: item.id.clone(),
            })
            .collect();
        cache.blocks = note
            .blocks
            .iter()
            .map(|(key, block)| {
                (
                    key.clone(),
                    BlockCache {
                        position: range(block.start, block.end),
                        id: block.id.clone(),
                    },
                )
            })
            .collect();
        cache.footnotes = note
            .footnotes
            .iter()
            .map(|footnote| FootnoteCache {
                position: range(footnote.start, footnote.end),
                id: footnote.id.to_lowercase(),
            })
            .collect();
        let parser = Parser::new_ext(&note.body, Options::ENABLE_FOOTNOTES);
        for (id, definition) in parser.reference_definitions().iter() {
            let position = range(definition.span.start, definition.span.end);
            cache.reference_links.push(ReferenceLinkCache {
                position,
                id: id.to_string(),
                link: definition.dest.to_string(),
            });
            cache.sections.push(SectionCache {
                kind: "definition".to_string(),
                position,
                id: None,
            });
        }
        for (event, span) in parser.into_offset_iter() {
            if let Event::FootnoteReference(id) = event {
                cache.footnote_refs.push(FootnoteRefCache {
                    position: range(span.start, span.end),
                    id: id.to_lowercase(),
                });
            }
        }
        cache
            .reference_links
            .sort_by_key(|reference| reference.position.start.offset);
        cache
            .sections
            .sort_by_key(|section| section.position.start.offset);
        normalize_structural_cache(note, &positions, &mut cache);
        normalize_footnotes(note, &positions, &mut cache);
        if let Some(frontmatter) = &note.frontmatter {
            let start = frontmatter.span.map(|(start, _)| start).unwrap_or(0);
            let end = trimmed_end(&note.source, start, base);
            let position = positions.range(start, end);
            cache.sections.insert(
                0,
                SectionCache {
                    kind: "yaml".to_string(),
                    position,
                    id: None,
                },
            );
            if let Ok(document) = crate::yaml_graph::parse_frontmatter(&frontmatter.raw) {
                if matches!(
                    &document.nodes[document.root],
                    crate::yaml_value::Node::Object(_)
                        | crate::yaml_value::Node::ObjectWithUtf16Keys(_)
                        | crate::yaml_value::Node::Date(_)
                        | crate::yaml_value::Node::Binary(_)
                        | crate::yaml_value::Node::Map(_)
                        | crate::yaml_value::Node::Set(_)
                ) {
                    cache.frontmatter_position = Some(position);
                    let mapping = document.metadata_mapping().ok().flatten();
                    let links = crate::frontmatter::links_from_document(&document)
                        .into_iter()
                        .map(|link| {
                            let (href, title) = wiki_cache_fields(&link.original)
                                .unwrap_or_else(|| (link.link.clone(), link.display_text.clone()));
                            FrontmatterLinkCache {
                                key: link.key,
                                key_units: link.key_units,
                                link: href,
                                original: link.original,
                                display_text: title,
                            }
                        })
                        .collect();
                    cache.frontmatter_links = Some(links);
                    cache.frontmatter = mapping;
                    cache.native_frontmatter = Some(std::sync::Arc::new(document));
                }
            }
        }
        cache
    }
}

fn escaped_at(source: &str, start: usize) -> bool {
    source.as_bytes()[..start]
        .iter()
        .rev()
        .take_while(|&&byte| byte == b'\\')
        .count()
        % 2
        == 1
}

fn normalized_cache_href(value: &str) -> String {
    value
        .replace('\u{00a0}', " ")
        .trim_matches(crate::vault_search::js_whitespace)
        .nfc()
        .collect()
}

/// Worker wiki href and title are distinct: href is NFC-normalized, title is
/// trimmed source text, and percent escapes in wiki targets remain literal.
fn wiki_cache_fields(original: &str) -> Option<(String, String)> {
    let inner = original
        .strip_prefix("![[")
        .or_else(|| original.strip_prefix("[["))?
        .strip_suffix("]]")?;
    let trim = |text: &str| {
        text.trim_matches(crate::vault_search::js_whitespace)
            .to_owned()
    };
    let (target, title) = if let Some(split) = inner.find('|').filter(|&split| split > 0) {
        (trim(&inner[..split]), trim(&inner[split + 1..]))
    } else {
        let target = trim(inner);
        let title = trim(
            &target
                .split('#')
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join(" > "),
        );
        (target, title)
    };
    let href = normalized_cache_href(target.strip_suffix('\\').unwrap_or(&target));
    Some((href, title))
}

fn normalize_structural_cache(
    note: &Note,
    positions: &SourcePositions<'_>,
    cache: &mut CachedMetadata,
) {
    let base = note.body_offset;
    let mut cursor = 0;
    while cursor < note.body.len() {
        let line_end = note.body[cursor..]
            .find('\n')
            .map(|i| cursor + i)
            .unwrap_or(note.body.len());
        let line = &note.body[cursor..line_end];
        let trimmed = line.trim_start_matches(' ');
        if note.sections.iter().any(|section| {
            section.kind == "code"
                && section.position.start.offset <= cursor
                && section.position.end.offset > cursor
        }) {
            cursor = if line_end == note.body.len() {
                line_end
            } else {
                line_end + 1
            };
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("%%") {
            if !rest.contains('%') {
                let start = cursor + line.len() - trimmed.len();
                let end = note.body[start + 2..]
                    .find("%%")
                    .map(|i| start + i + 4)
                    .unwrap_or(note.body.len());
                let position = positions.range(base + start, base + end);
                cache.sections.retain(|section| {
                    section.position.start.offset < position.start.offset
                        || section.position.start.offset >= position.end.offset
                });
                cache.headings.retain(|heading| {
                    heading.position.start.offset < position.start.offset
                        || heading.position.start.offset >= position.end.offset
                });
                cache.sections.push(SectionCache {
                    kind: "comment".to_string(),
                    position,
                    id: None,
                });
                cursor = end;
                if note.body.as_bytes().get(cursor) == Some(&b'\n') {
                    cursor += 1;
                }
                continue;
            }
        }
        cursor = if line_end == note.body.len() {
            line_end
        } else {
            line_end + 1
        };
    }
    for section in &mut cache.sections {
        let original = note.sections.iter().find(|candidate| {
            positions.loc(base + candidate.position.start.offset).offset
                == section.position.start.offset
        });
        if section.kind == "blockquote"
            && original.is_some_and(|candidate| {
                note.body[candidate.position.start.offset..]
                    .trim_start_matches('>')
                    .trim_start()
                    .starts_with("[!")
            })
        {
            section.kind = "callout".to_string();
        }
        if section.kind == "code" && section.position.start.col > 0 {
            if let Some(candidate) = original {
                let absolute = base + candidate.position.start.offset;
                let start = note.source[..absolute]
                    .rfind(['\r', '\n'])
                    .map(|i| i + 1)
                    .unwrap_or(0);
                section.position.start = positions.loc(start);
            }
        }
        if section.kind == "list" {
            section.id = None;
        }
    }
    cache
        .sections
        .sort_by_key(|section| section.position.start.offset);
    let mut section_index = 0;
    while section_index < cache.sections.len() {
        let section = &cache.sections[section_index];
        if section.kind == "paragraph" {
            let original = note.sections.iter().find(|candidate| {
                positions.loc(base + candidate.position.start.offset).offset
                    == section.position.start.offset
            });
            if let Some(original) = original {
                let text =
                    note.body[original.position.start.offset..original.position.end.offset].trim();
                if let Some(id) =
                    crate::subpath::trailing_block_id(text).filter(|id| text == format!("^{id}"))
                {
                    if section_index > 0 {
                        cache.sections[section_index - 1].id = Some(id);
                    }
                    cache.sections.remove(section_index);
                    continue;
                }
            }
        }
        section_index += 1;
    }
    for (index, item) in note.list_items.iter().enumerate() {
        let mut end = trimmed_end(
            &note.body,
            item.position.start.offset,
            item.position.end.offset,
        );
        if let Some(child) = note.list_items.iter().skip(index + 1).find(|child| {
            child.parent == item.position.start.line as i64
                && child.position.start.offset < item.position.end.offset
        }) {
            end = item.position.start.offset
                + note.body[item.position.start.offset..child.position.start.offset]
                    .trim_end()
                    .len();
        } else if note.body[item.position.start.offset..item.position.end.offset].ends_with("\n\n")
            && note
                .list_items
                .iter()
                .skip(index + 1)
                .any(|next| next.parent == item.parent)
        {
            end += 1;
        }
        cache.list_items[index].position.end = positions.loc(base + end);
        cache.list_items[index].id =
            crate::subpath::trailing_block_id(&note.body[item.position.start.offset..end]);
    }
    cache.blocks.clear();
    for section in &cache.sections {
        if let Some(id) = &section.id {
            cache.blocks.insert(
                id.to_lowercase(),
                BlockCache {
                    position: section.position,
                    id: id.clone(),
                },
            );
        }
    }
    for item in &cache.list_items {
        if let Some(id) = &item.id {
            cache.blocks.insert(
                id.to_lowercase(),
                BlockCache {
                    position: item.position,
                    id: id.clone(),
                },
            );
        }
    }
}

fn normalize_footnotes(note: &Note, positions: &SourcePositions<'_>, cache: &mut CachedMetadata) {
    let base = note.body_offset;
    // Footnote continuation indentation denotes content rather than code.
    for footnote in &note.footnotes {
        let mut start = footnote.start;
        for line in note.body[footnote.start..footnote.end].split_inclusive('\n') {
            let mut content = line.strip_prefix("    ").unwrap_or(line);
            let mut content_start = start + line.len() - content.len();
            // The general scanner omits reference-definition lines. A
            // footnote definition's first line instead contains normal body
            // references, so scan after its marker while retaining positions.
            if start == footnote.start && content.starts_with("[^") {
                if let Some(marker_end) = content.find("]:") {
                    content = &content[marker_end + 2..];
                    content_start += marker_end + 2;
                }
            }
            let extracted = crate::link::extract(content);
            for wiki in extracted.wikilinks {
                let position = positions.range(
                    base + content_start + wiki.start,
                    base + content_start + wiki.end,
                );
                if cache
                    .links
                    .iter()
                    .chain(&cache.embeds)
                    .any(|reference| reference.position == position)
                {
                    continue;
                }
                let original = content[wiki.start..wiki.end].to_string();
                let (href, title) = wiki_cache_fields(&original).unwrap();
                let reference = ReferenceCache {
                    position,
                    link: href,
                    original,
                    display_text: title,
                };
                if wiki.embed {
                    cache.embeds.push(reference);
                } else {
                    cache.links.push(reference);
                }
            }
            for tag in extracted.tags {
                let position = positions.range(
                    base + content_start + tag.start,
                    base + content_start + tag.end,
                );
                if !cache
                    .tags
                    .iter()
                    .any(|existing| existing.position == position)
                {
                    cache.tags.push(TagCache {
                        position,
                        tag: tag.name,
                    });
                }
            }
            start += line.len();
        }
    }
    let mut inline = Vec::new();
    let inline_code = Parser::new(&note.body)
        .into_offset_iter()
        .filter_map(|(event, range)| matches!(event, Event::Code(_)).then_some(range))
        .collect::<Vec<_>>();
    let mut cursor = 0;
    while let Some(found) = note.body[cursor..].find("^[") {
        let start = cursor + found;
        cursor = start + 2;
        let location = positions.loc(base + start);
        if escaped_at(&note.body, start)
            || inline_code
                .iter()
                .any(|range| range.start <= start && range.end > start)
            || cache.sections.iter().any(|section| {
                section.kind == "code"
                    && section.position.start.offset <= location.offset
                    && section.position.end.offset > location.offset
            })
        {
            continue;
        }
        let mut depth = 1;
        let mut end = None;
        for (relative, c) in note.body[cursor..].char_indices() {
            let absolute = cursor + relative;
            if escaped_at(&note.body, absolute) {
                continue;
            }
            if c == '[' {
                depth += 1;
            }
            if c == ']' {
                depth -= 1;
                if depth == 0 {
                    end = Some(absolute);
                    break;
                }
            }
        }
        if let Some(end) = end {
            inline.push((start, start + 2, end));
            cursor = end + 1;
        }
    }
    let mut events =
        cache
            .footnote_refs
            .iter()
            .map(|reference| {
                (
                    reference.position.start.offset,
                    Some(reference.id.clone()),
                    None,
                )
            })
            .chain(inline.iter().enumerate().map(|(index, (start, _, _))| {
                (positions.loc(base + start).offset, None, Some(index))
            }))
            .collect::<Vec<_>>();
    events.sort_by_key(|event| event.0);
    let mut seen = std::collections::BTreeSet::new();
    let mut number = 0;
    for (_, id, index) in events {
        if let Some(id) = id {
            if seen.insert(id) {
                number += 1;
            }
        }
        if let Some(index) = index {
            let (_, start, end) = inline[index];
            cache.footnotes.push(FootnoteCache {
                position: positions.range(base + start, base + end),
                id: format!("[inline{number}"),
            });
            number += 1;
        }
    }
    cache
        .footnotes
        .sort_by_key(|footnote| footnote.position.start.offset);
    let inline_ranges = inline
        .iter()
        .map(|(_, start, end)| positions.range(base + start, base + end))
        .collect::<Vec<_>>();
    for references in [&mut cache.links, &mut cache.embeds] {
        references.sort_by_key(|reference| reference.position.start.offset);
        let mut postponed = Vec::new();
        references.retain(|reference| {
            if inline_ranges.iter().any(|range| {
                reference.position.start.offset >= range.start.offset
                    && reference.position.end.offset <= range.end.offset
            }) {
                postponed.push(reference.clone());
                false
            } else {
                true
            }
        });
        references.extend(postponed);
    }
    cache.tags.sort_by_key(|tag| tag.position.start.offset);
    let mut postponed_tags = Vec::new();
    cache.tags.retain(|tag| {
        if inline_ranges.iter().any(|range| {
            tag.position.start.offset >= range.start.offset
                && tag.position.end.offset <= range.end.offset
        }) {
            postponed_tags.push(tag.clone());
            false
        } else {
            true
        }
    });
    cache.tags.extend(postponed_tags);
    if !cache.footnote_refs.is_empty() || !inline.is_empty() {
        if let Some(last) = cache.sections.last() {
            let location = CacheLoc {
                line: last.position.end.line + 1,
                col: 0,
                offset: last.position.end.offset,
            };
            for kind in ["text", "element"] {
                cache.sections.push(SectionCache {
                    kind: kind.to_string(),
                    position: CachePosition {
                        start: location,
                        end: location,
                    },
                    id: None,
                });
            }
        }
    }
}

fn heading_source(source: &str, start: usize, end: usize) -> String {
    let raw = source[start..end].trim_end_matches(['\r', '\n']);
    let first = raw.lines().next().unwrap_or("");
    if first.starts_with('#') {
        let text = first.trim_start_matches('#').trim_start();
        let without_closing = text.trim_end_matches('#');
        if without_closing.len() != text.len() && without_closing.ends_with(char::is_whitespace) {
            without_closing.trim_end().to_string()
        } else {
            text.trim_end().to_string()
        }
    } else {
        raw.rsplit_once('\n')
            .map(|(heading, _)| heading.trim())
            .unwrap_or(raw)
            .to_string()
    }
}

fn full_source_parent(note: &Note, parent: i64, item_start: usize) -> i64 {
    let prefix_lines = note.source[..note.body_offset]
        .bytes()
        .filter(|&byte| byte == b'\n')
        .count() as i64;
    if parent >= 0 {
        return parent + prefix_lines;
    }
    let start_line = note
        .sections
        .iter()
        .find(|section| {
            section.kind == "list"
                && section.position.start.offset <= item_start
                && section.position.end.offset >= item_start
        })
        .map(|section| section.position.start.line as i64);
    let first = start_line.unwrap_or(-parent) + prefix_lines;
    if first == 0 {
        -1
    } else {
        -first
    }
}

fn task_marker(item: &str) -> Option<String> {
    let line = item.lines().next()?.trim_start();
    let rest = if matches!(line.as_bytes().first(), Some(b'-' | b'*' | b'+')) {
        line.get(1..)?.trim_start()
    } else {
        let dot = line.find(['.', ')'])?;
        if !line[..dot].bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        line.get(dot + 1..)?.trim_start()
    };
    let inside = rest.strip_prefix('[')?;
    let marker = inside.chars().next()?;
    if inside.get(marker.len_utf8()..)?.starts_with(']') {
        Some(marker.to_string())
    } else {
        None
    }
}

fn trimmed_end(source: &str, start: usize, end: usize) -> usize {
    start + source[start..end].trim_end_matches(['\r', '\n']).len()
}
