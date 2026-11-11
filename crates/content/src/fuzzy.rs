//! One typo forgiven: the edit check and the trigram index that
//! bounds `find`'s candidates. A query word of at least `MIN_TYPO_CHARS` characters
//! also matches a word one of whose prefixes (n−1, n or n+1 characters) is within one
//! edit of it; candidates come from shared start-padded trigrams, never a scan.
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};

pub(crate) use mnemosyne_index::fuzzy::osa_within_one;
pub(crate) use mnemosyne_index::MIN_TYPO_CHARS;

/// The most candidates `find` verifies per query word.
pub(crate) const MAX_CANDIDATES: usize = 4096;

/// Whether some prefix of `word` of length n−1, n or n+1 (n = `query.len()`) is
/// within one edit of `query`.
pub(crate) fn prefix_within_one(query: &[char], word: &[char]) -> bool {
    let n = query.len();
    (n.saturating_sub(1).max(1)..=n + 1)
        .filter(|k| *k <= word.len())
        .any(|k| osa_within_one(query, &word[..k]))
}

/// Start-padded trigrams of a word, distinct: `"$$w"`, `"$wo"`, `"wor"`, …
fn trigrams(word: &str) -> BTreeSet<String> {
    let padded: Vec<char> = ['$', '$'].into_iter().chain(word.chars()).collect();
    padded.windows(3).map(|w| w.iter().collect()).collect()
}

/// Distinct words and, per trigram, the ids of the words holding it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct TrigramIndex {
    pub words: Vec<String>,
    pub postings: BTreeMap<String, Vec<u32>>,
}

impl TrigramIndex {
    pub fn build(words: impl IntoIterator<Item = String>) -> Self {
        let words: Vec<String> = words
            .into_iter()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut postings: BTreeMap<String, Vec<u32>> = BTreeMap::new();
        for (id, word) in words.iter().enumerate() {
            for trigram in trigrams(word) {
                postings.entry(trigram).or_default().push(id as u32);
            }
        }
        Self { words, postings }
    }

    /// Ids of the words sharing at least max(1, t−4) of the query's t distinct
    /// trigrams, most shared first, then by id; at most `cap`.
    pub fn candidates(&self, query: &str, cap: usize) -> Vec<u32> {
        let grams = trigrams(query);
        let floor = grams.len().saturating_sub(4).max(1) as u32;
        let mut shared: HashMap<u32, u32> = HashMap::new();
        for gram in &grams {
            for id in self.postings.get(gram).into_iter().flatten() {
                *shared.entry(*id).or_default() += 1;
            }
        }
        let mut found: Vec<(u32, u32)> = shared.into_iter().filter(|(_, n)| *n >= floor).collect();
        found.sort_by(|(a, na), (b, nb)| nb.cmp(na).then(a.cmp(b)));
        found.truncate(cap);
        found.into_iter().map(|(id, _)| id).collect()
    }

    /// The words that `query` reaches only with an edit: candidates within one edit of
    /// a prefix, and not already prefixed by `query` exactly.
    pub fn near(&self, query: &str) -> BTreeSet<&str> {
        let q: Vec<char> = query.chars().collect();
        self.candidates(query, MAX_CANDIDATES)
            .into_iter()
            .filter_map(|id| self.words.get(id as usize))
            .filter(|w| !w.starts_with(query))
            .filter(|w| prefix_within_one(&q, &w.chars().collect::<Vec<_>>()))
            .map(String::as_str)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    #[test]
    fn osa_distance_one() {
        let within = |a: &str, b: &str| osa_within_one(&c(a), &c(b));
        // The brief's `warantty` is two edits from `warranty` (a lost `r`, an extra
        // `t`); `warrnaty` is the one-transposition typo it stands for.
        assert!(!within("warranty", "warantty"));
        assert!(within("warranty", "warrnaty"));
        assert!(within("warranty", "wrranty"));
        assert!(within("prise", "price"));
        assert!(!within("abcde", "edcba"));
        assert!(within("гарантия", "гарнатия"));
        assert!(within("same", "same"));
        assert!(within("warranty", "warranty!"));
        assert!(within("ab", "ba"));
        assert!(!within("abc", "ca"));
        assert!(!within("warranty", "warrantyxx"));
    }

    #[test]
    fn prefixes_within_one() {
        let p = |q: &str, w: &str| prefix_within_one(&c(q), &c(w));
        assert!(p("warrnaty", "warranty-claims"));
        assert!(p("prise", "prices"));
        assert!(p("arranty", "warranty"));
        assert!(p("warrant", "waranty"));
        assert!(!p("prise", "pr"));
        assert!(!p("abcde", "xbcdx"));
    }

    #[test]
    fn trigram_candidates_bounded() {
        let mut words: Vec<String> = (0..9_999).map(|i| format!("w{i:05}word")).collect();
        words.push("warranty".into());
        let index = TrigramIndex::build(words);
        assert_eq!(index.words.len(), 10_000);
        let warranty = index.words.iter().position(|w| w == "warranty").unwrap() as u32;
        let candidates = index.candidates("warantty", MAX_CANDIDATES);
        assert!(candidates.len() <= 4096);
        assert_eq!(candidates[0], warranty);
        // A short query shares `$$w` with every word, and is cut at the cap.
        assert_eq!(index.candidates("wxyzq", MAX_CANDIDATES).len(), 4096);
        assert_eq!(index.near("warrnaty"), BTreeSet::from(["warranty"]));
        assert!(index.near("warranty").is_empty());
    }
}
