//! Index-time projections of a text for the graph index: its title, a one-paragraph
//! snippet and its heading titles. Pure functions over the text; they never error, so
//! malformed frontmatter falls through to the body.
use crate::knowledge::{front_matter, scan_headings, MAX_HEADINGS, MAX_MARKDOWN_BYTES};
use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use serde_json::Value;

pub(crate) const MAX_TITLE_CHARS: usize = 200;
pub(crate) const MAX_SNIPPET_CHARS: usize = 240;

/// The title a text gives itself: its frontmatter `title` when that is a non-empty
/// string, else its first level-1 heading; trimmed and cut to 200 characters. `None`
/// (also for a text over the bound) falls back to `basename_title`. The two halves are
/// apart because the graph index caches the first by blob and the second follows the
/// path.
pub(crate) fn content_title(text: &str) -> Option<String> {
    if text.len() > MAX_MARKDOWN_BYTES {
        return None;
    }
    let (properties, body_start) = front_matter(text);
    let from_frontmatter = properties.ok().and_then(|p| match p.get("title") {
        Some(Value::String(t)) if !t.trim().is_empty() => Some(t.trim().to_string()),
        _ => None,
    });
    from_frontmatter
        .or_else(|| {
            scan_headings(&text[body_start..], MAX_HEADINGS)
                .into_iter()
                .take(MAX_HEADINGS)
                .find(|(level, _, title)| *level == 1 && !title.trim().is_empty())
                .map(|(_, _, title)| title.trim().to_string())
        })
        .map(|t| cut(&t, MAX_TITLE_CHARS))
}

/// The title of a binary, a folder or a text that gives none: the basename without
/// its last extension (the whole basename when that would be empty), cut to 200
/// characters.
pub(crate) fn basename_title(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    let stem = match name.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem,
        _ => name,
    };
    cut(stem.trim(), MAX_TITLE_CHARS)
}

fn cut(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// The first non-empty paragraph of the body after any frontmatter, else its first
/// heading, as plain text with markup dropped and whitespace collapsed. Longer than 240
/// characters, it is cut at the last space at or before the 240th character and ends
/// with `…`, at most 240 characters in all. `""` when there is nothing.
pub(crate) fn snippet_of(text: &str) -> String {
    if text.len() > MAX_MARKDOWN_BYTES {
        return String::new();
    }
    let (_, body_start) = front_matter(text);
    let mut paragraph: Option<String> = None;
    let mut heading: Option<String> = None;
    let mut first_heading: Option<String> = None;
    let mut code = 0usize;
    for event in Parser::new(&text[body_start..]) {
        match event {
            Event::Start(Tag::Paragraph) => paragraph = Some(String::new()),
            Event::End(TagEnd::Paragraph) => {
                if let Some(p) = paragraph.take().map(|p| collapse(&p)) {
                    if !p.is_empty() {
                        return bound(&p);
                    }
                }
            }
            Event::Start(Tag::Heading { .. }) => heading = Some(String::new()),
            Event::End(TagEnd::Heading(_)) => {
                if let Some(h) = heading.take().map(|h| collapse(&h)) {
                    if first_heading.is_none() && !h.is_empty() {
                        first_heading = Some(h);
                    }
                }
            }
            Event::Start(Tag::CodeBlock(_)) => code += 1,
            Event::End(TagEnd::CodeBlock) => code = code.saturating_sub(1),
            Event::Text(t) | Event::Code(t) if code == 0 => {
                for buf in [&mut paragraph, &mut heading].into_iter().flatten() {
                    buf.push_str(&t);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                for buf in [&mut paragraph, &mut heading].into_iter().flatten() {
                    buf.push(' ');
                }
            }
            _ => {}
        }
    }
    first_heading.map(|h| bound(&h)).unwrap_or_default()
}

fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn bound(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= MAX_SNIPPET_CHARS {
        return s.to_string();
    }
    let last = MAX_SNIPPET_CHARS - 1;
    let end = (0..=last).rev().find(|i| chars[*i] == ' ').unwrap_or(last);
    let mut out: String = chars[..end].iter().collect();
    out.push('…');
    out
}

/// The heading titles of a text in outline order, so heading `i` is section `s{i+1}`
/// of the outline; at most 4096. Empty for a text over the bound.
pub(crate) fn heading_titles(text: &str) -> Vec<String> {
    if text.len() > MAX_MARKDOWN_BYTES {
        return vec![];
    }
    let (_, body_start) = front_matter(text);
    scan_headings(&text[body_start..], MAX_HEADINGS)
        .into_iter()
        .take(MAX_HEADINGS)
        .map(|(_, _, title)| title)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bound_cuts_at_a_space_and_counts_the_ellipsis() {
        let s = "abcd ".repeat(200);
        let b = bound(s.trim());
        assert_eq!(b.chars().count(), 240);
        assert!(b.ends_with("abcd…"));
        let one_word = "x".repeat(300);
        assert_eq!(bound(&one_word).chars().count(), 240);
        assert_eq!(bound("short"), "short");
    }

    fn title_of(path: &str, text: Option<&str>) -> String {
        text.and_then(content_title)
            .unwrap_or_else(|| basename_title(path))
    }

    #[test]
    fn titles_trim_and_skip_empty_headings() {
        assert_eq!(title_of("a/b.md", Some("#\n\n# Real ")), "Real");
        assert_eq!(title_of("a/b.md", Some("---\ntitle: \"  \"\n---\n")), "b");
        assert_eq!(title_of(".env", None), ".env");
        assert_eq!(title_of("a/archive.tar.gz", None), "archive.tar");
        assert_eq!(title_of("x.md", Some("```\n# not\n```\n")), "x");
    }

    #[test]
    fn heading_titles_follow_the_outline() {
        assert_eq!(
            heading_titles("---\nk: v\n---\n# A\ntext\n## B\n```\n# C\n```\nD\n-\n"),
            vec!["A", "B", "D"]
        );
    }
}
