//! Link rewriting for note renames/moves, following the desktop app's `Wx`,
//! which reconstructs a `[[wikilink]]` or `[text](url)` reference with a new
//! link text while preserving custom aliases and titles.

use crate::link::parse_linktext;
use crate::path::{basename, basename_no_md, strip_extension, strip_md_extension};

/// A source mutation with UTF-8 byte offsets in the complete original file.
/// Editors convert these coordinates to UTF-16 after BOM/newline handling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextEdit {
    pub start: usize,
    pub end: usize,
    pub text: String,
}
impl TextEdit {
    /// Trim only within an already identified reference or frontmatter span.
    /// This does not derive a global diff or merge concurrent editor content.
    pub fn within_span(start: usize, old: &str, new: &str) -> Option<Self> {
        if old == new {
            return None;
        }
        let prefix: usize = old
            .chars()
            .zip(new.chars())
            .take_while(|(a, b)| a == b)
            .map(|(c, _)| c.len_utf8())
            .sum();
        let suffix: usize = old[prefix..]
            .chars()
            .rev()
            .zip(new[prefix..].chars().rev())
            .take_while(|(a, b)| a == b)
            .map(|(c, _)| c.len_utf8())
            .sum();
        Some(Self {
            start: start + prefix,
            end: start + old.len() - suffix,
            text: new[prefix..new.len() - suffix].to_owned(),
        })
    }
}

/// Rewrites a single link reference to point at `new_linktext` (the new
/// `path#subpath`). `original` is the raw link text and `link` the parsed
/// target (path plus optional `#subpath`, or the Markdown destination).
pub fn rewrite_link(original: &str, link: &str, new_linktext: &str) -> String {
    if let Some(r) = rewrite_wikilink(original, new_linktext) {
        return r;
    }
    rewrite_markdown_link(original, link, new_linktext)
}

fn rewrite_wikilink(original: &str, new_linktext: &str) -> Option<String> {
    let (open, rest) = if let Some(r) = original.strip_prefix("![[") {
        ("![[", r)
    } else if let Some(r) = original.strip_prefix("[[") {
        ("[[", r)
    } else {
        return None;
    };
    let inner = rest.strip_suffix("]]")?;

    let (target, alias) = match inner.find('|') {
        Some(p) => (&inner[..p], Some(&inner[p + 1..])),
        None => (inner, None),
    };

    match alias {
        Some(alias) => {
            let sep = if original.contains("\\|") { "\\|" } else { "|" };
            // Aliases are preserved on bare filenames. Only path-based
            // aliases equal to the old stem are updated, with custom alias
            // whitespace otherwise left intact.
            let h = if target.contains('/') && strip_extension(basename(target)) == alias.trim() {
                strip_extension(basename(&parse_linktext(new_linktext).path))
            } else {
                alias.to_string()
            };
            Some(format!("{open}{new_linktext}{sep}{h}]]"))
        }
        None => Some(format!("{open}{new_linktext}]]")),
    }
}

fn rewrite_markdown_link(original: &str, link: &str, new_linktext: &str) -> String {
    let (open, rest) = if let Some(r) = original.strip_prefix("!") {
        ("!", r)
    } else {
        ("", original)
    };

    // rest must start with "["
    let Some(body) = rest.strip_prefix('[') else {
        return original.to_string();
    };
    let Some(close) = body.find("](") else {
        return original.to_string();
    };
    let text = &body[..close];
    let after = &body[close + 2..]; // after "]("

    // Parse destination + optional title + closing ")".
    let (_dest, dest_was_angle, title_and_close) =
        if let Some(after_angle) = after.strip_prefix('<') {
            match after_angle.find('>') {
                Some(gt) => (&after_angle[..gt], true, &after_angle[gt + 1..]),
                None => return original.to_string(),
            }
        } else {
            let end = after
                .find(|c: char| c == ')' || c.is_whitespace())
                .unwrap_or(after.len());
            (&after[..end], false, &after[end..])
        };

    // title_and_close: optional whitespace + title + optional whitespace + ")"
    let (title, _close) = split_title(title_and_close);

    // New destination.
    let v = if dest_was_angle {
        format!("<{new_linktext}>")
    } else {
        encode_url(new_linktext)
    };

    // New link text: reset only if it was the default (old basename/path).
    let old_basename = basename_no_md(&parse_linktext(link).path);
    let text_trim = text
        .replace(['\u{00A0}', '\u{202F}'], " ")
        .trim()
        .to_string();
    let g = if text_trim == old_basename {
        basename_no_md(&parse_linktext(new_linktext).path)
    } else if text_trim.contains('/') && text_trim == strip_md_extension(&parse_linktext(link).path)
    {
        strip_md_extension(&parse_linktext(new_linktext).path)
    } else {
        text.to_string()
    };

    format!("{open}[{g}]({v}{title})")
}

fn split_title(after_dest: &str) -> (String, String) {
    // If there's a title, it's separated from dest by whitespace and quoted.
    let s = after_dest.trim_start();
    if s.is_empty() {
        return (String::new(), ")".to_string());
    }
    // s ends with ")"
    let s = s.strip_suffix(')').unwrap_or(s);
    let s = s.trim_end();
    if s.is_empty() {
        return (String::new(), ")".to_string());
    }
    // Preserve the leading whitespace + title.
    let leading_ws_len = after_dest.len() - after_dest.trim_start().len();
    let ws = &after_dest[..leading_ws_len];
    (format!("{ws}{s}"), ")".to_string())
}

fn encode_url(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\\' | ' ' => format!("%{:02X}", c as u32),
            c if (c as u32) < 0x20 => format!("%{:02X}", c as u32),
            c => c.to_string(),
        })
        .collect()
}

/// JavaScript `decodeURI`, preserving escapes for URI reserved characters.
/// Invalid percent sequences or UTF-8 reject the entire destination.
pub(crate) fn decode_uri(s: &str) -> Option<String> {
    let input = s.as_bytes();
    let mut output = Vec::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        if input[index] == b'%' {
            let high = (*input.get(index + 1)? as char).to_digit(16)?;
            let low = (*input.get(index + 2)? as char).to_digit(16)?;
            let byte = ((high << 4) | low) as u8;
            if b";/?:@&=+$,#".contains(&byte) {
                output.extend_from_slice(&input[index..index + 3]);
            } else {
                output.push(byte);
            }
            index += 3;
        } else {
            output.push(input[index]);
            index += 1;
        }
    }
    String::from_utf8(output).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_wikilink() {
        assert_eq!(rewrite_link("[[Old]]", "Old", "New"), "[[New]]");
    }

    #[test]
    fn embed() {
        assert_eq!(rewrite_link("![[Old]]", "Old", "New"), "![[New]]");
    }

    #[test]
    fn custom_alias_preserved() {
        assert_eq!(
            rewrite_link("[[Old|My alias]]", "Old", "New"),
            "[[New|My alias]]"
        );
    }

    #[test]
    fn bare_default_alias_preserved() {
        assert_eq!(rewrite_link("[[Old|Old]]", "Old", "New"), "[[New|Old]]");
    }

    #[test]
    fn path_default_alias_reset() {
        assert_eq!(
            rewrite_link("[[folder/Old|Old]]", "folder/Old", "New"),
            "[[New|New]]"
        );
    }

    #[test]
    fn heading_subpath_preserved() {
        assert_eq!(
            rewrite_link("[[Old#Section]]", "Old#Section", "New#Section"),
            "[[New#Section]]"
        );
    }

    #[test]
    fn markdown_link_default_text() {
        assert_eq!(
            rewrite_link("[Old](Old.md)", "Old.md", "New.md"),
            "[New](New.md)"
        );
    }

    #[test]
    fn markdown_link_custom_text() {
        assert_eq!(
            rewrite_link("[see here](Old.md)", "Old.md", "New.md"),
            "[see here](New.md)"
        );
    }

    #[test]
    fn uri_decoding_preserves_reserved_characters_and_rejects_invalid_input() {
        assert_eq!(
            decode_uri("caf%C3%A9%20note.md%23Heading").as_deref(),
            Some("café note.md%23Heading")
        );
        assert_eq!(decode_uri("a%2Fb%3Fc%25").as_deref(), Some("a%2Fb%3Fc%"));
        for invalid in ["%", "%zz", "%C3", "%FF", "%ED%A0%80"] {
            assert!(decode_uri(invalid).is_none(), "{invalid}");
        }
    }
}
