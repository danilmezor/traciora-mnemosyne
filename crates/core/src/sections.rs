//! The metadata cache's `sections` and `listItems` : top-level
//! blocks and list items with 0-based positions, matching the desktop app.

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

use crate::subpath::trailing_block_id;

/// A 0-based position (line, column, byte offset), matching the plugin API's cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Pos {
    pub line: usize,
    pub col: usize,
    pub offset: usize,
}

/// A start/end position pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Range {
    pub start: Pos,
    pub end: Pos,
}

/// A top-level block (a root child of the parsed document).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Section {
    /// Node type: `heading`, `paragraph`, `list`, `code`, `blockquote`,
    /// `table`, `footnoteDefinition`, `thematicBreak`, `html`.
    pub kind: String,
    pub position: Range,
    pub id: Option<String>,
}

/// A list item with its position, nesting parent, task marker, and id.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ListItem {
    pub position: Range,
    /// The plugin API's `parent`: the enclosing list item's line, or a negative
    /// line / `-1` for a top-level list.
    pub parent: i64,
    /// `Some("x")` for a checked task, `Some(" ")` for unchecked.
    pub task: Option<String>,
    pub id: Option<String>,
}

/// The result of section parsing.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct Sections {
    pub sections: Vec<Section>,
    pub list_items: Vec<ListItem>,
}

struct Frame {
    kind: &'static str,
    is_section: bool,
    top_level: bool,
    start: usize,
    start_line: usize,
    task: Option<String>,
    item_index: Option<usize>,
}

struct ListCtx {
    start_line: usize,
    parent_line: Option<usize>,
}

/// Parses top-level sections and list items from note source.
pub fn parse_sections(source: &str) -> Sections {
    let line_index = LineIndex::new(source);
    let mut sections = Vec::new();
    let mut list_items = Vec::new();

    let mut frames: Vec<Frame> = Vec::new();
    let mut list_stack: Vec<ListCtx> = Vec::new();

    for (event, range) in parser(source).into_offset_iter() {
        match event {
            Event::Start(tag) => {
                if let Some((kind, is_section)) = block_kind(&tag) {
                    let top_level = frames.is_empty();
                    let start_line = line_index.line(range.start);
                    let parent = if kind == "listItem" {
                        compute_parent(&list_stack)
                    } else {
                        0
                    };
                    let mut item_index = None;
                    if kind == "list" {
                        let parent_line = frames.last().and_then(|f| {
                            if f.kind == "listItem" {
                                Some(f.start_line)
                            } else {
                                None
                            }
                        });
                        list_stack.push(ListCtx {
                            start_line,
                            parent_line,
                        });
                    }
                    if kind == "listItem" {
                        let start_pos = line_index.pos(range.start);
                        item_index = Some(list_items.len());
                        list_items.push(ListItem {
                            position: Range {
                                start: start_pos,
                                end: start_pos,
                            },
                            parent,
                            task: None,
                            id: None,
                        });
                    }
                    frames.push(Frame {
                        kind,
                        is_section,
                        top_level,
                        start: range.start,
                        start_line,
                        task: None,
                        item_index,
                    });
                }
            }
            Event::End(tag_end) => {
                if is_block_end(&tag_end) {
                    if let Some(f) = frames.pop() {
                        if f.kind == "list" {
                            list_stack.pop();
                        }
                        let end = range.end;
                        let id = trailing_block_id(&source[f.start..end]);
                        if f.is_section && f.top_level {
                            sections.push(Section {
                                kind: f.kind.to_string(),
                                position: line_index.range(f.start, end),
                                id,
                            });
                        } else if f.kind == "listItem" {
                            if let Some(idx) = f.item_index {
                                list_items[idx].position.end = line_index.pos(end);
                                list_items[idx].id = id;
                                list_items[idx].task = f.task;
                            }
                        }
                    }
                }
            }
            Event::Rule => {
                if frames.is_empty() {
                    sections.push(Section {
                        kind: "thematicBreak".to_string(),
                        position: line_index.range(range.start, range.end),
                        id: None,
                    });
                }
            }
            Event::TaskListMarker(_) => {
                let ch = source
                    .as_bytes()
                    .get(range.start + 1)
                    .copied()
                    .unwrap_or(b' ') as char;
                let ch = if ch == 'X' { 'x' } else { ch };
                if let Some(f) = frames.iter_mut().rev().find(|f| f.kind == "listItem") {
                    f.task = Some(ch.to_string());
                }
            }
            _ => {}
        }
    }

    Sections {
        sections,
        list_items,
    }
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

fn block_kind(tag: &Tag<'_>) -> Option<(&'static str, bool)> {
    match tag {
        Tag::Paragraph => Some(("paragraph", true)),
        Tag::Heading { .. } => Some(("heading", true)),
        Tag::CodeBlock(_) => Some(("code", true)),
        Tag::BlockQuote(_) => Some(("blockquote", true)),
        Tag::HtmlBlock => Some(("html", true)),
        Tag::List(_) => Some(("list", true)),
        Tag::Item => Some(("listItem", false)),
        Tag::FootnoteDefinition(_) => Some(("footnoteDefinition", true)),
        Tag::Table(_) => Some(("table", true)),
        Tag::TableHead => Some(("tableHead", false)),
        Tag::TableRow => Some(("tableRow", false)),
        Tag::TableCell => Some(("tableCell", false)),
        _ => None,
    }
}

fn is_block_end(tag_end: &TagEnd) -> bool {
    matches!(
        tag_end,
        TagEnd::Paragraph
            | TagEnd::Heading(_)
            | TagEnd::CodeBlock
            | TagEnd::BlockQuote(_)
            | TagEnd::HtmlBlock
            | TagEnd::List(_)
            | TagEnd::Item
            | TagEnd::FootnoteDefinition
            | TagEnd::Table
            | TagEnd::TableHead
            | TagEnd::TableRow
            | TagEnd::TableCell
    )
}

fn compute_parent(list_stack: &[ListCtx]) -> i64 {
    match list_stack.last() {
        Some(l) => match l.parent_line {
            Some(line) => line as i64,
            None => {
                let w = -(l.start_line as i64);
                if w == 0 {
                    -1
                } else {
                    w
                }
            }
        },
        None => -1,
    }
}

struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    fn new(s: &str) -> Self {
        let mut starts = vec![0];
        for (i, b) in s.bytes().enumerate() {
            if b == b'\n' {
                starts.push(i + 1);
            }
        }
        LineIndex { starts }
    }

    fn line(&self, offset: usize) -> usize {
        self.starts.partition_point(|&s| s <= offset) - 1
    }

    fn pos(&self, offset: usize) -> Pos {
        let line = self.line(offset);
        Pos {
            line,
            col: offset - self.starts[line],
            offset,
        }
    }

    fn range(&self, start: usize, end: usize) -> Range {
        Range {
            start: self.pos(start),
            end: self.pos(end),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_level_sections() {
        let src = "# Heading\n\nSome paragraph.\n\n- a\n- b\n";
        let s = parse_sections(src);
        let kinds: Vec<&str> = s.sections.iter().map(|s| s.kind.as_str()).collect();
        assert_eq!(kinds, vec!["heading", "paragraph", "list"]);
    }

    #[test]
    fn list_items_with_tasks() {
        let src = "- [ ] todo\n- [x] done\n";
        let s = parse_sections(src);
        assert_eq!(s.list_items.len(), 2);
        assert_eq!(s.list_items[0].task.as_deref(), Some(" "));
        assert_eq!(s.list_items[1].task.as_deref(), Some("x"));
    }

    #[test]
    fn list_item_block_id() {
        let src = "- item ^theid\n";
        let s = parse_sections(src);
        assert_eq!(s.list_items[0].id.as_deref(), Some("theid"));
    }

    #[test]
    fn nested_list_parent() {
        let src = "- parent\n\t- child\n";
        let s = parse_sections(src);
        // parent item at line 0 -> parent = -1 (top-level)
        assert_eq!(s.list_items[0].parent, -1);
        // child item -> parent = enclosing item's line (0)
        assert_eq!(s.list_items[1].parent, 0);
    }

    #[test]
    fn section_block_id() {
        let src = "Some paragraph ^blockid\n";
        let s = parse_sections(src);
        assert_eq!(s.sections[0].id.as_deref(), Some("blockid"));
    }
}
