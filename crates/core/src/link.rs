//! Scanning for vault-specific inline syntax: `[[wikilinks]]`,
//! `![[embeds]]`, and `#tags`, plus link-text and heading helpers.
//!
//! Rules match the desktop app's parser. Spans are byte
//! offsets into the string passed to [`extract`].

/// An `[[internal link]]` (or `![[embed]]`) reference.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Wikilink {
    /// The raw target, e.g. `Note` or `Note#Heading`.
    pub target: String,
    /// The display alias after a `|`, e.g. `Note#Heading|My Title`.
    pub alias: Option<String>,
    /// `true` for `![[embed]]` syntax.
    pub embed: bool,
    /// Byte offset of the leading `[` (or `!` for embeds).
    pub start: usize,
    /// Byte offset one past the trailing `]]`.
    pub end: usize,
}

impl Wikilink {
    /// The part of the target before any `#`.
    pub fn path_part(&self) -> &str {
        match self.target.find('#') {
            Some(i) => &self.target[..i],
            None => &self.target,
        }
    }

    /// The part of the target after the first `#`, if any.
    pub fn heading_part(&self) -> Option<&str> {
        self.target.find('#').map(|i| &self.target[i + 1..])
    }

    /// The display text: the alias if present, otherwise the target.
    pub fn display(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.target)
    }
}

/// A `#tag` reference. `name` includes the leading `#`, matching the desktop app's
/// metadata cache (e.g. `#tag`, `#nested/tag`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Tag {
    /// The full tag including `#` (may include `/` for nesting).
    pub name: String,
    /// Byte offset of the leading `#`.
    pub start: usize,
    /// Byte offset one past the last tag character.
    pub end: usize,
}

/// The result of scanning a string for inline vault syntax.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Extracted {
    pub wikilinks: Vec<Wikilink>,
    pub tags: Vec<Tag>,
}

/// A link split into its path and subpath parts (`parseLinktext`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Linktext {
    pub path: String,
    /// Includes the leading `#`, or empty when absent.
    pub subpath: String,
}

/// Splits a link on the first `#` into path and subpath.
pub fn parse_linktext(link: &str) -> Linktext {
    match link.find('#') {
        Some(i) => Linktext {
            path: link[..i].to_string(),
            subpath: link[i..].to_string(),
        },
        None => Linktext {
            path: link.to_string(),
            subpath: String::new(),
        },
    }
}

/// Extracts wikilinks, embeds, and tags, skipping code and comments.
pub fn extract(source: &str) -> Extracted {
    let bytes = source.as_bytes();
    let n = bytes.len();
    let mut out = Extracted::default();

    let mut i = 0usize;
    let mut line_start = true;
    let mut fence: Option<(u8, usize)> = None;

    while i < n {
        if let Some((fch, flen)) = fence {
            if bytes[i] == b'\n' {
                line_start = true;
                i += 1;
                continue;
            }
            if line_start && (bytes[i] == b' ' || bytes[i] == b'\t') {
                i += 1;
                continue;
            }
            if line_start && bytes[i] == fch {
                let run = run_len(bytes, i, fch);
                if run >= flen && is_fence_close(bytes, i + run) {
                    fence = None;
                    i += run;
                    continue;
                }
            }
            line_start = false;
            i += 1;
            continue;
        }

        let b = bytes[i];
        match b {
            b'\n' => {
                line_start = true;
                i += 1;
            }
            b'\r' => i += 1,
            b' ' | b'\t' if line_start => match skip_indented_code(bytes, i) {
                Some(end) => i = end,
                None => i += 1,
            },
            b'`' => {
                line_start = false;
                let run = run_len(bytes, i, b'`');
                if run >= 3 {
                    fence = Some((b'`', run));
                    i += run;
                } else {
                    // Unbalanced backticks are literal (CommonMark).
                    i = find_inline_code_end(bytes, i, run).unwrap_or(i + run);
                }
            }
            b'~' => {
                let run = run_len(bytes, i, b'~');
                if line_start && run >= 3 {
                    line_start = false;
                    fence = Some((b'~', run));
                    i += run;
                } else {
                    line_start = false;
                    i += run;
                }
            }
            b'<' => {
                line_start = false;
                if bytes[i..].starts_with(b"<!--") {
                    i = find_subslice(bytes, i + 4, b"-->")
                        .map(|p| p + 3)
                        .unwrap_or(n);
                } else {
                    i += 1;
                }
            }
            b'!' => {
                line_start = false;
                if bytes[i..].starts_with(b"![[") {
                    if let Some((target, alias, end)) = read_wikilink(bytes, i + 1) {
                        out.wikilinks.push(Wikilink {
                            target,
                            alias,
                            embed: true,
                            start: i,
                            end,
                        });
                        i = end;
                        continue;
                    }
                }
                i += 1;
            }
            b'[' => {
                let was_line_start = line_start;
                line_start = false;
                if bytes[i..].starts_with(b"[[") {
                    if let Some((target, alias, end)) = read_wikilink(bytes, i) {
                        out.wikilinks.push(Wikilink {
                            target,
                            alias,
                            embed: false,
                            start: i,
                            end,
                        });
                        i = end;
                        continue;
                    }
                } else if was_line_start {
                    if let Some(end) = reference_def_end(bytes, i) {
                        i = end;
                        continue;
                    }
                }
                i += 1;
            }
            b']' => {
                line_start = false;
                if i + 1 < n && bytes[i + 1] == b'(' {
                    i = skip_link_destination(bytes, i + 1);
                } else {
                    i += 1;
                }
            }
            b'#' => {
                line_start = false;
                if let Some((name, end)) = read_tag(source, i) {
                    out.tags.push(Tag {
                        name,
                        start: i,
                        end,
                    });
                    i = end;
                    continue;
                }
                i += 1;
            }
            _ => {
                line_start = false;
                i += 1;
            }
        }
    }

    out
}

fn run_len(bytes: &[u8], start: usize, ch: u8) -> usize {
    let mut i = start;
    while i < bytes.len() && bytes[i] == ch {
        i += 1;
    }
    i - start
}

fn is_fence_close(bytes: &[u8], at: usize) -> bool {
    at >= bytes.len() || matches!(bytes[at], b'\n' | b'\r' | b' ' | b'\t')
}

/// If the line starting at `start` is an indented code block (4+ spaces and no
/// list marker), returns the index of its newline; otherwise `None`.
fn skip_indented_code(bytes: &[u8], start: usize) -> Option<usize> {
    let mut i = start;
    let mut indent = 0;
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
        indent += if bytes[i] == b'\t' { 4 } else { 1 };
        i += 1;
    }
    if indent < 4 {
        return None;
    }
    if i >= bytes.len() {
        return Some(i);
    }
    let b = bytes[i];
    if b == b'-' || b == b'*' || b == b'+' {
        return None;
    }
    if b.is_ascii_digit() {
        let mut j = i;
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            j += 1;
        }
        if j < bytes.len() && (bytes[j] == b'.' || bytes[j] == b')') {
            return None;
        }
    }
    let mut e = i;
    while e < bytes.len() && bytes[e] != b'\n' {
        e += 1;
    }
    Some(e)
}

fn find_subslice(hay: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    hay[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| from + p)
}

/// Skips past a `](...)` link destination, returning the index after the
/// matching `)` (handling nested parens and `<...>`).
fn skip_link_destination(bytes: &[u8], open: usize) -> usize {
    let mut i = open + 1;
    let mut depth = 1;
    let mut in_angle = false;
    while i < bytes.len() {
        match bytes[i] {
            b'<' if !in_angle => {
                in_angle = true;
                i += 1;
            }
            b'>' if in_angle => {
                in_angle = false;
                i += 1;
            }
            b'(' if !in_angle => {
                depth += 1;
                i += 1;
            }
            b')' if !in_angle => {
                depth -= 1;
                i += 1;
                if depth == 0 {
                    return i;
                }
            }
            _ => i += 1,
        }
    }
    i
}

/// If `bytes[start]` begins a `[label]:` reference definition, returns the
/// index of the newline ending it.
fn reference_def_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut j = start + 1;
    while j < bytes.len() && bytes[j] != b'\n' {
        if bytes[j] == b']' && j + 1 < bytes.len() && bytes[j + 1] == b':' {
            let mut e = j + 2;
            while e < bytes.len() && bytes[e] != b'\n' {
                e += 1;
            }
            return Some(e);
        }
        j += 1;
    }
    None
}

fn find_inline_code_end(bytes: &[u8], start: usize, run: usize) -> Option<usize> {
    let mut i = start + run;
    while i < bytes.len() {
        match bytes[i] {
            b'`' => {
                let r = run_len(bytes, i, b'`');
                if r == run {
                    return Some(i + r);
                }
                i += r;
            }
            // Inline code spans do not cross newlines.
            b'\n' => return None,
            _ => i += 1,
        }
    }
    None
}

fn read_wikilink(bytes: &[u8], open: usize) -> Option<(String, Option<String>, usize)> {
    let inner_start = open + 2;
    let close = find_subslice(bytes, inner_start, b"]]")?;
    let inner = std::str::from_utf8(&bytes[inner_start..close]).ok()?;
    let (target, alias) = match inner.find('|') {
        Some(p) => {
            let (t, a) = inner.split_at(p);
            (t.trim().to_string(), Some(a[1..].trim().to_string()))
        }
        None => (inner.trim().to_string(), None),
    };
    if target.is_empty() {
        return None;
    }
    Some((target, alias, close + 2))
}

/// `Yk = /^#[^\u2000-\u206F\u2E00-\u2E7F'!"#$%&()*+,.:;<=>?@^`{|}~\[\]\\\s]+/`.
fn is_tag_char(c: char) -> bool {
    if crate::vault_search::js_whitespace(c) {
        return false;
    }
    let cp = c as u32;
    if (0x2000..=0x206F).contains(&cp) || (0x2E00..=0x2E7F).contains(&cp) {
        return false;
    }
    !matches!(
        c,
        '\'' | '!'
            | '"'
            | '#'
            | '$'
            | '%'
            | '&'
            | '('
            | ')'
            | '*'
            | '+'
            | ','
            | '.'
            | ':'
            | ';'
            | '<'
            | '='
            | '>'
            | '?'
            | '@'
            | '^'
            | '`'
            | '{'
            | '|'
            | '}'
            | '~'
            | '['
            | ']'
            | '\\'
    )
}

/// Whether a tag (with its leading `#`) is valid: it fully matches the tag
/// character set and is not all digits (`iR`, `tR`/`nR`).
pub fn is_valid_tag(tag: &str) -> bool {
    let Some(rest) = tag.strip_prefix('#') else {
        return false;
    };
    if rest.is_empty() {
        return false;
    }
    if rest.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    rest.chars().all(is_tag_char)
}

/// Whether a link target is internal (a vault reference) as
/// opposed to an external URL.
pub fn is_internal_link(target: &str) -> bool {
    (target.starts_with("./") || target.starts_with("../")) || !target.contains(':')
}

fn read_tag(source: &str, hash: usize) -> Option<(String, usize)> {
    if hash > 0 {
        let prev = source[..hash].chars().next_back()?;
        // A tag must be preceded by whitespace or a link-text opening `[`
        // (the start of an inline context), not by a word character.
        if !crate::vault_search::js_whitespace(prev) && prev != '[' {
            return None;
        }
    }

    let mut name = String::new();
    let mut end = hash + 1;
    for c in source[hash + 1..].chars() {
        if is_tag_char(c) {
            name.push(c);
            end += c.len_utf8();
        } else {
            break;
        }
    }
    if name.is_empty() {
        return None;
    }
    // `/^#\d+$/` — tags may not consist solely of ASCII digits.
    if name.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some((format!("#{name}"), end))
}

const HEADING_PUNCT: [char; 26] = [
    '!', '"', '#', '$', '%', '&', '(', ')', '*', '+', ',', '.', ':', ';', '<', '=', '>', '?', '@',
    '^', '`', '{', '|', '}', '~', '/',
];

/// `stripHeading`: replace the `mS` punctuation set with spaces, collapse
/// whitespace, trim.
pub fn strip_heading(s: &str) -> String {
    let mapped: String = s
        .chars()
        .map(|c| {
            if HEADING_PUNCT.contains(&c) || c == '[' || c == ']' || c == '\\' {
                ' '
            } else {
                c
            }
        })
        .collect();
    collapse_whitespace(&mapped).trim().to_string()
}

/// `stripHeadingForLink`: replace `: # | ^ \` and `%%`, `[[`, `]]` with spaces,
/// collapse whitespace, trim.
pub fn strip_heading_for_link(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let two = |a, b| chars[i] == a && i + 1 < chars.len() && chars[i + 1] == b;
        if two('%', '%') || two('[', '[') || two(']', ']') {
            out.push(' ');
            i += 2;
        } else if matches!(c, ':' | '#' | '|' | '^' | '\\' | '\r' | '\n') {
            out.push(' ');
            i += 1;
        } else {
            out.push(c);
            i += 1;
        }
    }
    collapse_whitespace(&out).trim().to_string()
}

fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_ws = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !last_ws {
                out.push(' ');
            }
            last_ws = true;
        } else {
            out.push(c);
            last_ws = false;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_wikilink() {
        let e = extract("see [[Foo]] here");
        assert_eq!(e.wikilinks.len(), 1);
        assert_eq!(e.wikilinks[0].target, "Foo");
        assert!(!e.wikilinks[0].embed);
    }

    #[test]
    fn alias_and_heading() {
        let e = extract("[[Note#Section|My Title]]");
        let w = &e.wikilinks[0];
        assert_eq!(w.target, "Note#Section");
        assert_eq!(w.alias.as_deref(), Some("My Title"));
        assert_eq!(w.path_part(), "Note");
        assert_eq!(w.heading_part(), Some("Section"));
    }

    #[test]
    fn embed() {
        let e = extract("![[Image.png]]");
        assert!(e.wikilinks[0].embed);
        assert_eq!(e.wikilinks[0].target, "Image.png");
    }

    #[test]
    fn skips_fenced_code() {
        let e = extract("```\n[[NotALink]]\n```\n[[Real]]");
        assert_eq!(e.wikilinks.len(), 1);
        assert_eq!(e.wikilinks[0].target, "Real");
    }

    #[test]
    fn skips_inline_code() {
        let e = extract("`[[x]]` and [[y]]");
        assert_eq!(e.wikilinks.len(), 1);
        assert_eq!(e.wikilinks[0].target, "y");
    }

    #[test]
    fn tags_basic_and_nested() {
        let e = extract("#tag and #nested/deep");
        let names: Vec<_> = e.tags.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["#tag", "#nested/deep"]);
    }

    #[test]
    fn tag_needs_preceding_whitespace() {
        // foo#bar is NOT a tag (locator requires whitespace before '#').
        let e = extract("foo#bar and # real");
        assert!(e.tags.is_empty());
    }

    #[test]
    fn tag_after_paren_is_not_a_tag() {
        let e = extract("(#tag)");
        assert!(e.tags.is_empty());
    }

    #[test]
    fn numeric_tag_rejected() {
        let e = extract("#123 and #abc");
        let names: Vec<_> = e.tags.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["#abc"]);
    }

    #[test]
    fn unicode_tag() {
        let e = extract("#тег и #nested/tag");
        assert_eq!(e.tags[0].name, "#тег");
    }

    #[test]
    fn tag_skipped_in_code() {
        let e = extract("```\n#hidden\n```\n#visible");
        assert_eq!(e.tags.len(), 1);
        assert_eq!(e.tags[0].name, "#visible");
    }

    #[test]
    fn tag_in_link_destination_skipped() {
        let e = extract("[see](#anchor) and #real");
        assert_eq!(e.tags.len(), 1);
        assert_eq!(e.tags[0].name, "#real");
    }

    #[test]
    fn tag_in_reference_definition_skipped() {
        let e = extract("[ref]: #anchor\n\n#real");
        assert_eq!(e.tags.len(), 1);
        assert_eq!(e.tags[0].name, "#real");
    }

    #[test]
    fn wikilink_in_link_text_kept() {
        let e = extract("[[Note]] (#anchor)");
        assert_eq!(e.wikilinks.len(), 1);
        assert_eq!(e.tags.len(), 0);
    }

    #[test]
    fn tag_after_link_open() {
        let e = extract("[#inlink](http://x)");
        assert_eq!(e.tags.len(), 1);
        assert_eq!(e.tags[0].name, "#inlink");
    }

    #[test]
    fn fence_handling_basic() {
        let e = extract("```console\nnpm i foo\n```\n\n## Usage\n\nafter #tag\n");
        let names: Vec<_> = e.tags.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"#tag"), "tags: {names:?}");
    }

    #[test]
    fn indented_fence_closes() {
        let src = "```javascript\ncode\n```\n- item\n      ```javascript\n      code\n      ```\nafter #tag\n";
        let e = extract(src);
        let names: Vec<_> = e.tags.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"#tag"), "tags: {names:?}");
    }

    #[test]
    fn unbalanced_backtick_is_literal() {
        let e = extract("* a lone `backtick here\n\n* later #tag\n");
        let names: Vec<_> = e.tags.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"#tag"), "tags: {names:?}");
    }

    #[test]
    fn parse_linktext_splits() {
        let l = parse_linktext("Note#Heading");
        assert_eq!(l.path, "Note");
        assert_eq!(l.subpath, "#Heading");
        let l2 = parse_linktext("Note");
        assert_eq!(l2.path, "Note");
        assert_eq!(l2.subpath, "");
    }

    #[test]
    fn strip_heading_filters_punct() {
        assert_eq!(strip_heading("Hello, World!"), "Hello World");
        assert_eq!(strip_heading("a/b:c"), "a b c");
    }

    #[test]
    fn strip_heading_for_link_filters() {
        assert_eq!(strip_heading_for_link("Note#Heading"), "Note Heading");
        assert_eq!(strip_heading_for_link("[[x]]"), "x");
        assert_eq!(strip_heading_for_link("a%%b"), "a b");
    }
}
