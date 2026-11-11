//! The quick-switcher search of the plugin API: `prepareFuzzySearch`, `prepareSimpleSearch`,
//! and `sortSearchResults` Matching and scoring operate on UTF-16
//! code units to mirror the original JavaScript string semantics.

/// A search hit with a relevance score and match ranges.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchMatch {
    pub score: f64,
    /// Match ranges `[start, end)` as UTF-16 code-unit offsets into the text.
    pub matches: Vec<(usize, usize)>,
}

// ---------------------------------------------------------------------------
// Character classes
// ---------------------------------------------------------------------------

fn is_ws_u16(u: u16) -> bool {
    matches!(
        u,
        0x09 | 0x0A
            | 0x0B
            | 0x0C
            | 0x0D
            | 0x20
            | 0xA0
            | 0x1680
            | 0x2028
            | 0x2029
            | 0x202F
            | 0x205F
            | 0x3000
            | 0xFEFF
    ) || (0x2000..=0x200A).contains(&u)
}

fn is_punct_u16(u: u16) -> bool {
    if (0x2000..=0x206F).contains(&u) || (0x2E00..=0x2E7F).contains(&u) {
        return true;
    }
    match char::from_u32(u as u32) {
        Some(c) => matches!(
            c,
            '\\' | '\''
                | '!'
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
                | '-'
                | '.'
                | '/'
                | ':'
                | ';'
                | '<'
                | '='
                | '>'
                | '?'
                | '@'
                | '['
                | ']'
                | '^'
                | '_'
                | '`'
                | '{'
                | '|'
                | '}'
                | '~'
        ),
        None => false,
    }
}

fn is_cjk_u16(u: u16) -> bool {
    (0x0F00..=0x0FFF).contains(&u)
        || (0x3040..=0x30FF).contains(&u)
        || (0x3400..=0x4DBF).contains(&u)
        || (0x4E00..=0x9FFF).contains(&u)
        || (0xF900..=0xFAFF).contains(&u)
        || (0xFF66..=0xFF9F).contains(&u)
}

fn lower_differs(u: u16) -> bool {
    match char::from_u32(u as u32) {
        Some(c) => c.to_lowercase().to_string() != c.to_string(),
        None => false,
    }
}

fn upper_differs(u: u16) -> bool {
    match char::from_u32(u as u32) {
        Some(c) => c.to_uppercase().to_string() != c.to_string(),
        None => false,
    }
}

fn utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

// ---------------------------------------------------------------------------
// Fuzzy search (prepareFuzzySearch)
// ---------------------------------------------------------------------------

struct PreparedQuery {
    query_len: usize,
    tokens: Vec<Vec<u16>>,
    fuzzy: Vec<Vec<u16>>,
}

/// A compiled fuzzy-search query over a prepared query string.
pub struct FuzzySearch {
    prepared: PreparedQuery,
}

impl FuzzySearch {
    pub fn new(query: &str) -> Self {
        let lower = query.to_lowercase();
        let lower_units: Vec<u16> = lower.encode_utf16().collect();

        let mut tokens: Vec<Vec<u16>> = Vec::new();
        let mut cur: Vec<u16> = Vec::new();
        for &c in &lower_units {
            if is_ws_u16(c) {
                if !cur.is_empty() {
                    tokens.push(std::mem::take(&mut cur));
                }
            } else if is_punct_u16(c) || is_cjk_u16(c) {
                if !cur.is_empty() {
                    tokens.push(std::mem::take(&mut cur));
                }
                tokens.push(vec![c]);
            } else {
                cur.push(c);
            }
        }
        if !cur.is_empty() {
            tokens.push(cur);
        }

        FuzzySearch {
            prepared: PreparedQuery {
                query_len: query.encode_utf16().count(),
                tokens,
                fuzzy: lower_units.iter().map(|&u| vec![u]).collect(),
            },
        }
    }

    pub fn find(&self, text: &str) -> Option<SearchMatch> {
        if self.prepared.query_len == 0 {
            return Some(SearchMatch {
                score: 0.0,
                matches: Vec::new(),
            });
        }
        fuzzy_tokens(&self.prepared.tokens, self.prepared.query_len, text, false)
            .or_else(|| fuzzy_tokens(&self.prepared.fuzzy, self.prepared.query_len, text, true))
    }
}

fn fuzzy_tokens(
    tokens: &[Vec<u16>],
    query_len: usize,
    text: &str,
    fuzzy: bool,
) -> Option<SearchMatch> {
    if tokens.is_empty() {
        return None;
    }
    let lower: Vec<u16> = text.to_lowercase().encode_utf16().collect();
    let original: Vec<u16> = text.encode_utf16().collect();

    let mut boundary = 0i32;
    let mut search_start = 0usize;
    let mut ranges: Vec<(usize, usize)> = Vec::new();

    let mut l = 0usize;
    while l < tokens.len() {
        let token = &tokens[l];
        let u = index_of_u16(&lower, token, search_start)?;

        let h = original[u];
        if u > 0 && !is_punct_u16(h) && !is_cjk_u16(h) {
            let p = original[u - 1];
            let both_upper = lower_differs(h) && lower_differs(p);
            let lower_word =
                upper_differs(h) && !is_punct_u16(p) && !is_ws_u16(p) && !is_cjk_u16(p);
            if both_upper || lower_word {
                if fuzzy {
                    if u != search_start {
                        search_start += token.len();
                        continue;
                    }
                } else {
                    boundary += 1;
                }
            }
        }

        if ranges.is_empty() {
            ranges.push((u, u + token.len()));
        } else {
            let last = ranges.last_mut().unwrap();
            if last.1 < u {
                ranges.push((u, u + token.len()));
            } else if last.1 < u + token.len() {
                last.1 = u + token.len();
            }
        }
        search_start = u + token.len();
        l += 1;
    }

    Some(SearchMatch {
        score: score(&ranges, query_len, lower.len(), boundary),
        matches: ranges,
    })
}

fn index_of_u16(hay: &[u16], needle: &[u16], from: usize) -> Option<usize> {
    if needle.is_empty() {
        return Some(from.min(hay.len()));
    }
    hay.get(from..)?
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

// ---------------------------------------------------------------------------
// Simple search (prepareSimpleSearch)
// ---------------------------------------------------------------------------

/// A compiled simple (space-delimited) search query.
pub struct SimpleSearch {
    query_len: usize,
    tokens: Vec<Vec<u16>>,
}

impl SimpleSearch {
    pub fn new(query: &str) -> Self {
        let tokens = query
            .to_lowercase()
            .split(' ')
            .filter(|s| !s.is_empty())
            .map(utf16)
            .collect();
        SimpleSearch {
            query_len: query.encode_utf16().count(),
            tokens,
        }
    }

    pub fn find(&self, text: &str) -> Option<SearchMatch> {
        let lower: Vec<u16> = text.to_lowercase().encode_utf16().collect();
        let mut ranges: Vec<(usize, usize)> = Vec::new();

        for token in &self.tokens {
            let mut found = false;
            let mut pos = 0usize;
            while let Some(u) = index_of_u16(&lower, token, pos) {
                found = true;
                ranges.push((u, u + token.len()));
                pos = u + token.len() + 1;
            }
            if !found {
                return None;
            }
        }

        let merged = merge_ranges(ranges);
        Some(SearchMatch {
            score: score(&merged, self.query_len, text.encode_utf16().count(), 0),
            matches: merged,
        })
    }
}

/// Sorts a mutable list of results by score descending (higher first).
pub fn sort_search_results<T>(results: &mut [T], score: impl Fn(&T) -> f64) {
    results.sort_by(|a, b| {
        score(b)
            .partial_cmp(&score(a))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

// ---------------------------------------------------------------------------
// Scoring and range merging
// ---------------------------------------------------------------------------

fn score(matches: &[(usize, usize)], query_len: usize, text_len: usize, boundary: i32) -> f64 {
    if matches.is_empty() {
        return 0.0;
    }
    let mut r = 0.0;
    r -= f64::from((matches.len() as i32 - 1).max(0));
    r -= f64::from(boundary) / 10.0;
    let start = matches[0].0 as f64;
    let end = matches[matches.len() - 1].1 as f64;
    r -= (end - start + 1.0 - query_len as f64) / 100.0;
    r -= start / 1000.0;
    r -= text_len as f64 / 10000.0;
    r
}

fn merge_ranges(mut ranges: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    if ranges.is_empty() {
        return ranges;
    }
    ranges.sort_by_key(|r| r.0);
    let mut out: Vec<(usize, usize)> = vec![ranges[0]];
    for &r in &ranges[1..] {
        let last = out.last_mut().unwrap();
        if last.1 < r.0 {
            out.push(r);
        } else if last.1 < r.1 {
            last.1 = r.1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_basic() {
        let f = FuzzySearch::new("mnsy");
        let m = f.find("Mnemosyne").unwrap();
        assert!(m.score < 0.0);
        assert!(!m.matches.is_empty());
    }

    #[test]
    fn fuzzy_no_match() {
        let f = FuzzySearch::new("zzzz");
        assert!(f.find("Mnemosyne").is_none());
    }

    #[test]
    fn fuzzy_empty_query() {
        let f = FuzzySearch::new("");
        assert_eq!(f.find("anything").unwrap().score, 0.0);
    }

    #[test]
    fn simple_search() {
        let s = SimpleSearch::new("hello world");
        assert!(s.find("hello cruel world").is_some());
        assert!(s.find("hello").is_none());
    }

    #[test]
    fn simple_search_case_insensitive() {
        let s = SimpleSearch::new("HELLO");
        assert!(s.find("hello").is_some());
    }

    #[test]
    fn scoring_prefers_earlier_and_tighter() {
        let f = FuzzySearch::new("note");
        let a = f.find("my note here").unwrap();
        let b = f.find("n o t e scattered").unwrap();
        // Earlier contiguous match scores better.
        assert!(a.score > b.score);
    }
}
