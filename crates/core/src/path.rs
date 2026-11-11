//! Path primitives matching the desktop app's internal helpers.

use unicode_normalization::UnicodeNormalization;

/// `normalizePath`: collapse slash runs, trim leading/trailing slashes, map
/// non-breaking spaces to ASCII space, then NFC-normalize.
///
/// Does **not** resolve `.`/`..` and does **not** strip a leading `./`.
pub fn normalize_path(s: &str) -> String {
    let mut t = collapse_slashes(s);
    t = t.replace(['\u{00A0}', '\u{202F}'], " ");
    t.nfc().collect()
}

/// The last path segment (`basename`).
pub fn basename(s: &str) -> &str {
    match s.rfind('/') {
        Some(i) => &s[i + 1..],
        None => s,
    }
}

/// Everything before the last `/` (the parent folder), or `""`.
pub fn parent(s: &str) -> &str {
    match s.rfind('/') {
        Some(i) => &s[..i],
        None => "",
    }
}

/// Lowercased extension after the last `.`, or `""` if there is none, the dot
/// is the first character, or the dot is the last character.
pub fn extension(s: &str) -> String {
    match s.rfind('.') {
        Some(i) if i != 0 && i != s.len() - 1 => s[i + 1..].to_lowercase(),
        _ => String::new(),
    }
}

/// The path with the final extension removed (or unchanged per the same guard
/// rules as [`extension`]).
pub fn strip_extension(s: &str) -> String {
    match s.rfind('.') {
        Some(i) if i != 0 && i != s.len() - 1 => s[..i].to_string(),
        _ => s.to_string(),
    }
}

/// Whether any path segment starts with `.` (a hidden file or folder).
pub fn has_hidden_ancestor(s: &str) -> bool {
    let mut cur = s;
    loop {
        if basename(cur).starts_with('.') {
            return true;
        }
        let p = parent(cur);
        if p.is_empty() {
            return false;
        }
        cur = p;
    }
}

/// The basename with a trailing `.md` removed when the extension is `md`
/// (as the desktop app computes it).
pub fn basename_no_md(s: &str) -> String {
    let t = basename(s);
    if extension(t) == "md" {
        strip_extension(t)
    } else {
        t.to_string()
    }
}

/// The full path with a trailing `.md` removed when the basename's extension is
/// `md`.
pub fn strip_md_extension(s: &str) -> String {
    if extension(basename(s)) == "md" {
        strip_extension(s)
    } else {
        s.to_string()
    }
}

fn collapse_slashes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_slash = false;
    for c in s.chars() {
        if c == '/' || c == '\\' {
            if !prev_slash {
                out.push('/');
                prev_slash = true;
            }
        } else {
            out.push(c);
            prev_slash = false;
        }
    }
    let trimmed = out.trim_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_collapses_and_trims() {
        assert_eq!(normalize_path("a//b\\\\c///"), "a/b/c");
        assert_eq!(normalize_path("/foo/"), "foo");
        assert_eq!(normalize_path(""), "/");
        assert_eq!(normalize_path("////"), "/");
    }

    #[test]
    fn normalize_maps_nbsp() {
        assert_eq!(normalize_path("a\u{00A0}b"), "a b");
        assert_eq!(normalize_path("a\u{202F}b"), "a b");
    }

    #[test]
    fn basename_and_parent() {
        assert_eq!(basename("a/b/c.md"), "c.md");
        assert_eq!(basename("c.md"), "c.md");
        assert_eq!(parent("a/b/c.md"), "a/b");
        assert_eq!(parent("c.md"), "");
    }

    #[test]
    fn extension_and_strip() {
        assert_eq!(extension("note.md"), "md");
        assert_eq!(extension("NOTE.MD"), "md");
        assert_eq!(extension(".hidden"), "");
        assert_eq!(extension("noext"), "");
        assert_eq!(extension("trailing."), "");
        assert_eq!(strip_extension("note.md"), "note");
        assert_eq!(strip_extension("noext"), "noext");
        assert_eq!(strip_extension("a.b.md"), "a.b");
    }

    #[test]
    fn hidden_ancestor() {
        assert!(has_hidden_ancestor(&format!("{}/x.md", crate::CONFIG_DIR)));
        assert!(has_hidden_ancestor("a/.hidden/b.md"));
        assert!(!has_hidden_ancestor("a/b.md"));
    }
}
