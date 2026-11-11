//! Link extraction and resolution for managed markdown content.

use crate::{Entry, EntryKind};
use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

/// Version of what `extract_links` returns for a text. Raise it whenever extraction
/// changes, so links cached by an earlier grammar are parsed again.
pub const GRAMMAR_VERSION: u32 = 2;

const MAX_TEXT_BYTES: usize = 2 * 1024 * 1024;
const MAX_LINKS: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkKind {
    Markdown,
    Image,
    Wikilink,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    pub target: String,
    pub kind: LinkKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// A wikilink's `#heading` as written (trimmed); `None` when it names none. A
    /// Markdown `#fragment` is a slug, not a title, and is dropped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heading: Option<String>,
}

/// Links in document order, duplicates kept. The first 4096 links of a text of
/// at most 2 MiB; an empty list for a larger text. Never errors.
pub fn extract_links(text: &str) -> Vec<Link> {
    let mut out = Vec::new();
    if text.len() > MAX_TEXT_BYTES {
        return out;
    }
    let mut buf = String::new();
    let mut in_code_block = false;
    for event in Parser::new(text) {
        if out.len() >= MAX_LINKS {
            break;
        }
        match event {
            Event::Start(Tag::CodeBlock(_)) => {
                flush_wikilinks(&mut buf, &mut out);
                in_code_block = true;
            }
            Event::End(TagEnd::CodeBlock) => in_code_block = false,
            Event::Text(_) if in_code_block => {}
            Event::Text(t) => buf.push_str(&t),
            Event::Start(Tag::Link { dest_url, .. }) => {
                flush_wikilinks(&mut buf, &mut out);
                push_dest(&dest_url, LinkKind::Markdown, &mut out);
            }
            Event::Start(Tag::Image { dest_url, .. }) => {
                flush_wikilinks(&mut buf, &mut out);
                push_dest(&dest_url, LinkKind::Image, &mut out);
            }
            Event::End(TagEnd::Link) | Event::End(TagEnd::Image) => {}
            _ => flush_wikilinks(&mut buf, &mut out),
        }
    }
    flush_wikilinks(&mut buf, &mut out);
    out.truncate(MAX_LINKS);
    out
}

fn push_dest(dest: &str, kind: LinkKind, out: &mut Vec<Link>) {
    if let Some(target) = clean_destination(dest) {
        out.push(Link {
            target,
            kind,
            alias: None,
            heading: None,
        });
    }
}

fn clean_destination(dest: &str) -> Option<String> {
    let dest = dest.trim();
    if dest.is_empty() || dest.starts_with('/') || dest.starts_with('#') || has_scheme(dest) {
        return None;
    }
    let end = dest.find(['#', '?']).unwrap_or(dest.len());
    let decoded = percent_decode(&dest[..end]);
    if decoded.is_empty() {
        None
    } else {
        Some(decoded)
    }
}

fn has_scheme(dest: &str) -> bool {
    let Some(colon) = dest.find(':') else {
        return false;
    };
    let head = &dest[..colon];
    if dest[..colon].contains(['/', '?', '#']) {
        return false;
    }
    let mut chars = head.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> Option<u8> {
    (b as char).to_digit(16).map(|d| d as u8)
}

fn flush_wikilinks(buf: &mut String, out: &mut Vec<Link>) {
    let mut rest = buf.as_str();
    while out.len() < MAX_LINKS {
        let Some(open) = rest.find("[[") else { break };
        let after = &rest[open + 2..];
        let Some(close) = after.find("]]") else { break };
        let inner = &after[..close];
        rest = &after[close + 2..];
        if inner.contains(['\n', '[']) {
            continue;
        }
        let (target_part, alias) = match inner.split_once('|') {
            Some((t, a)) => (t, Some(a.trim()).filter(|a| !a.is_empty())),
            None => (inner, None),
        };
        let (target, heading) = match target_part.split_once('#') {
            Some((t, h)) => (t.trim(), Some(h.trim()).filter(|h| !h.is_empty())),
            None => (target_part.trim(), None),
        };
        if target.is_empty() {
            continue;
        }
        out.push(Link {
            target: target.to_string(),
            kind: LinkKind::Wikilink,
            alias: alias.map(str::to_string),
            heading: heading.map(str::to_string),
        });
    }
    buf.clear();
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Resolution {
    Resolved {
        path: String,
    },
    Unresolved {
        target: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        candidates: Vec<String>,
    },
}

/// Wikilink basenames (`.md` stripped) of the non-folder entries of a manifest, each
/// mapped to its paths in sorted order. Build once per manifest for many resolutions.
pub struct BasenameIndex<'a>(HashMap<&'a str, Vec<&'a str>>);

impl<'a> BasenameIndex<'a> {
    pub fn new(files: &'a BTreeMap<String, Entry>) -> Self {
        let mut map: HashMap<&str, Vec<&str>> = HashMap::new();
        // BTreeMap order keeps every path list sorted.
        for (path, entry) in files {
            if entry.kind != EntryKind::Folder {
                let name = path.rsplit('/').next().unwrap_or(path);
                map.entry(strip_md(name)).or_default().push(path);
            }
        }
        Self(map)
    }
}

/// Resolve `link`, found in the file at `from`, against the head manifest.
pub fn resolve(from: &str, link: &Link, files: &BTreeMap<String, Entry>) -> Resolution {
    resolve_with(from, link, files, &BasenameIndex::new(files))
}

/// As `resolve`, with a prebuilt basename index of the same `files`.
pub fn resolve_with(
    from: &str,
    link: &Link,
    files: &BTreeMap<String, Entry>,
    by_basename: &BasenameIndex,
) -> Resolution {
    let unresolved = |candidates: Vec<String>| Resolution::Unresolved {
        target: link.target.clone(),
        candidates,
    };
    let is_file = |p: &str| files.get(p).is_some_and(|e| e.kind != EntryKind::Folder);
    match link.kind {
        LinkKind::Wikilink => match by_basename.0.get(strip_md(&link.target)).map(Vec::as_slice) {
            None | Some([]) => unresolved(vec![]),
            Some([one]) => Resolution::Resolved {
                path: one.to_string(),
            },
            Some(many) => unresolved(many.iter().map(|p| p.to_string()).collect()),
        },
        LinkKind::Markdown | LinkKind::Image => {
            let Some(joined) = join(from, &link.target) else {
                return unresolved(vec![]);
            };
            if is_file(&joined) {
                return Resolution::Resolved { path: joined };
            }
            let with_md = format!("{joined}.md");
            if is_file(&with_md) {
                return Resolution::Resolved { path: with_md };
            }
            unresolved(vec![])
        }
    }
}

fn strip_md(name: &str) -> &str {
    name.strip_suffix(".md").unwrap_or(name)
}

/// Join `target` onto the folder of `from`, normalising `.` and `..`.
/// `None` when the result is empty or climbs above the tenant root.
fn join(from: &str, target: &str) -> Option<String> {
    let mut parts: Vec<&str> = from.split('/').collect();
    parts.pop();
    for seg in target.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            s => parts.push(s),
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("/"))
    }
}
