//! User exclusions are case-insensitive literal prefixes or slash-wrapped
//! regular expressions, rather than glob rules. Matching does not mutate paths.
use crate::vault_search::{js_whitespace, non_unicode_pattern};
use regress::Regex;

#[derive(Debug, Clone, Default)]
pub struct IgnoreFilters {
    patterns: Vec<Regex>,
    rejected: Vec<(String, String)>,
}

impl IgnoreFilters {
    pub fn from_filters<'a>(filters: impl IntoIterator<Item = &'a str>) -> Self {
        let mut patterns = Vec::new();
        let mut rejected = Vec::new();
        for filter in filters {
            let text = filter.trim_matches(js_whitespace);
            if text.is_empty() {
                continue;
            }
            let source = if text.encode_utf16().count() > 2
                && text.starts_with('/')
                && text.ends_with('/')
            {
                non_unicode_pattern(&text[1..text.len() - 1])
            } else {
                let mut escaped = String::from("^");
                for ch in text.chars() {
                    if "\\^$.*+?()[]{}|".contains(ch) {
                        escaped.push('\\');
                    }
                    escaped.push(ch);
                }
                escaped
            };
            // The reference logs a bad entry and continues with other entries.
            match Regex::with_flags(&source, "i") {
                Ok(pattern) => patterns.push(pattern),
                Err(error) => rejected.push((text.to_string(), error.to_string())),
            }
        }
        Self { patterns, rejected }
    }

    /// Diagnostic inputs dropped during compilation. Parser message wording is
    /// Rust-library wording; exact reference console diagnostics remain pending.
    pub fn rejected_filters(&self) -> &[(String, String)] {
        &self.rejected
    }

    pub fn is_ignored(&self, path: &str) -> bool {
        let path: Vec<u16> = path.encode_utf16().collect();
        self.patterns
            .iter()
            .any(|pattern| pattern.find_from_utf16(&path, 0).next().is_some())
    }
}
