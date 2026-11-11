//! Block IDs, footnotes, and subpath resolution (`[[Note#heading]]`,
//! `[[Note#^block]]`, `[[Note#^footnote]]`), matching the desktop app.

use std::collections::BTreeMap;

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

use crate::link::strip_heading;
use crate::markdown::Heading;

/// A `^block-id` reference with its byte span in the note.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct BlockRef {
    pub id: String,
    pub start: usize,
    pub end: usize,
}

/// A `[^id]` footnote definition with its byte span.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct FootnoteRef {
    pub id: String,
    pub start: usize,
    pub end: usize,
}

/// The result of resolving a link subpath.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedSubpath {
    Heading {
        text: String,
        level: u8,
        start: usize,
        /// Start of the next heading at an equal-or-higher level, if any.
        end: Option<usize>,
    },
    Block {
        id: String,
        start: usize,
        end: usize,
    },
    Footnote {
        id: String,
        start: usize,
        end: usize,
    },
}

fn parser(source: &str) -> Parser<'_> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_FOOTNOTES);
    options.insert(Options::ENABLE_HEADING_ATTRIBUTES);
    Parser::new_ext(source, options)
}

/// Extracts `^block-id` references. A block id is `^[a-zA-Z0-9-]+` either at
/// the end of a block (preceded by whitespace) or as a standalone block.
pub fn parse_blocks(source: &str) -> BTreeMap<String, BlockRef> {
    let mut blocks = BTreeMap::new();
    let mut stack: Vec<usize> = Vec::new();

    for (event, range) in parser(source).into_offset_iter() {
        match event {
            Event::Start(tag) if is_block_tag(&tag) => stack.push(range.start),
            Event::End(tag_end) if is_block_end(&tag_end) => {
                if let Some(start) = stack.pop() {
                    check_block_id(source, start, range.end, &mut blocks);
                }
            }
            _ => {}
        }
    }
    blocks
}

fn is_block_tag(tag: &Tag<'_>) -> bool {
    matches!(
        tag,
        Tag::Paragraph
            | Tag::Heading { .. }
            | Tag::CodeBlock(_)
            | Tag::BlockQuote(_)
            | Tag::Item
            | Tag::Table(_)
    )
}

fn is_block_end(tag_end: &TagEnd) -> bool {
    matches!(
        tag_end,
        TagEnd::Paragraph
            | TagEnd::Heading(_)
            | TagEnd::CodeBlock
            | TagEnd::BlockQuote(_)
            | TagEnd::Item
            | TagEnd::Table
    )
}

fn check_block_id(source: &str, start: usize, end: usize, blocks: &mut BTreeMap<String, BlockRef>) {
    if let Some(id) = trailing_block_id(&source[start..end]) {
        blocks.insert(id.to_lowercase(), BlockRef { id, start, end });
    }
}

/// The `^block-id` at the end of `text`, if any.
pub fn trailing_block_id(text: &str) -> Option<String> {
    let trimmed = text.trim_end();
    if trimmed.is_empty() {
        return None;
    }
    let id = if let Some(ws) = trimmed.rfind(char::is_whitespace) {
        let len = trimmed[ws..]
            .chars()
            .next()
            .map(char::len_utf8)
            .unwrap_or(1);
        trimmed[ws + len..].strip_prefix('^')
    } else {
        trimmed.strip_prefix('^')
    }?;
    if !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        Some(id.to_string())
    } else {
        None
    }
}

/// Extracts `[^id]: ...` footnote definitions.
pub fn parse_footnotes(source: &str) -> Vec<FootnoteRef> {
    let mut footnotes = Vec::new();
    let mut stack: Vec<(String, usize)> = Vec::new();

    for (event, range) in parser(source).into_offset_iter() {
        match event {
            Event::Start(Tag::FootnoteDefinition(label)) => {
                stack.push((label.into_string(), range.start));
            }
            Event::End(TagEnd::FootnoteDefinition) => {
                if let Some((id, start)) = stack.pop() {
                    footnotes.push(FootnoteRef {
                        id,
                        start,
                        end: range.end,
                    });
                }
            }
            _ => {}
        }
    }
    footnotes
}

/// Resolves a link subpath (`#heading`, `#a#b`, `#^block`, `#[^footnote]`)
/// against a note's headings, blocks, and footnotes.
pub fn resolve_subpath(
    headings: &[Heading],
    blocks: &BTreeMap<String, BlockRef>,
    footnotes: &[FootnoteRef],
    subpath: &str,
) -> Option<ResolvedSubpath> {
    if subpath.is_empty() {
        return None;
    }
    let parts: Vec<&str> = subpath.split('#').filter(|s| !s.is_empty()).collect();
    if parts.is_empty() {
        return None;
    }

    if parts.len() == 1 {
        let p = parts[0];
        if let Some(id) = p.strip_prefix('^') {
            if let Some(b) = blocks.get(&id.to_lowercase()) {
                return Some(ResolvedSubpath::Block {
                    id: b.id.clone(),
                    start: b.start,
                    end: b.end,
                });
            }
        }
        if let Some(inner) = p.strip_prefix("[^").and_then(|s| s.strip_suffix(']')) {
            for f in footnotes {
                if f.id == inner {
                    return Some(ResolvedSubpath::Footnote {
                        id: f.id.clone(),
                        start: f.start,
                        end: f.end,
                    });
                }
            }
        }
    }

    let mut g = 0usize;
    let mut y = 0u8;
    let mut found: Option<usize> = None;
    let mut next: Option<usize> = None;

    for (idx, h) in headings.iter().enumerate() {
        if found.is_some() && h.level <= y {
            next = Some(idx);
            break;
        }
        if found.is_none()
            && h.level > y
            && strip_heading(&h.text).to_lowercase() == strip_heading(parts[g]).to_lowercase()
        {
            g += 1;
            y = h.level;
            if g == parts.len() {
                found = Some(idx);
            }
        }
    }

    let fi = found?;
    let h = &headings[fi];
    let end = next.map(|ni| headings[ni].start);
    Some(ResolvedSubpath::Heading {
        text: h.text.clone(),
        level: h.level,
        start: h.start,
        end,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown;

    fn headings(source: &str) -> Vec<Heading> {
        markdown::parse(source).headings
    }

    #[test]
    fn block_id_trailing() {
        let src = "Some text ^abc-def\n";
        let blocks = parse_blocks(src);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks["abc-def"].id, "abc-def");
    }

    #[test]
    fn block_id_case_insensitive_key() {
        let blocks = parse_blocks("text ^MyId\n");
        assert!(blocks.contains_key("myid"));
    }

    #[test]
    fn block_id_on_heading() {
        let blocks = parse_blocks("## Heading ^h1\n");
        assert!(blocks.contains_key("h1"));
    }

    #[test]
    fn block_id_requires_whitespace_before() {
        let blocks = parse_blocks("text^id\n");
        assert!(blocks.is_empty());
    }

    #[test]
    fn footnotes_parsed() {
        let src = "Text[^1]\n\n[^1]: the note\n";
        let footnotes = parse_footnotes(src);
        assert_eq!(footnotes.len(), 1);
        assert_eq!(footnotes[0].id, "1");
    }

    #[test]
    fn resolve_heading() {
        let src = "# One\n\n## Two\n\ntext\n\n# Three\n";
        let hs = headings(src);
        let blocks = parse_blocks(src);
        let footnotes = parse_footnotes(src);
        match resolve_subpath(&hs, &blocks, &footnotes, "#Two").unwrap() {
            ResolvedSubpath::Heading { text, level, .. } => {
                assert_eq!(text, "Two");
                assert_eq!(level, 2);
            }
            _ => panic!("expected heading"),
        }
    }

    #[test]
    fn resolve_nested_heading() {
        let src = "## Parent\n\n### Child\n";
        let hs = headings(src);
        let blocks = parse_blocks(src);
        let footnotes = parse_footnotes(src);
        assert!(matches!(
            resolve_subpath(&hs, &blocks, &footnotes, "#Parent#Child"),
            Some(ResolvedSubpath::Heading { level: 3, .. })
        ));
    }

    #[test]
    fn resolve_block() {
        let src = "paragraph ^theblock\n";
        let hs = headings(src);
        let blocks = parse_blocks(src);
        let footnotes = parse_footnotes(src);
        match resolve_subpath(&hs, &blocks, &footnotes, "#^theblock").unwrap() {
            ResolvedSubpath::Block { id, .. } => assert_eq!(id, "theblock"),
            _ => panic!("expected block"),
        }
    }

    #[test]
    fn resolve_footnote() {
        let src = "Text[^a]\n\n[^a]: note\n";
        let hs = headings(src);
        let blocks = parse_blocks(src);
        let footnotes = parse_footnotes(src);
        match resolve_subpath(&hs, &blocks, &footnotes, "#[^a]").unwrap() {
            ResolvedSubpath::Footnote { id, .. } => assert_eq!(id, "a"),
            _ => panic!("expected footnote"),
        }
    }
}
