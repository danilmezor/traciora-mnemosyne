//! One edit of Damerau–Levenshtein distance (optimal string alignment): the check
//! shared by the content engine's `find` and the keyword snippets of fuzzy hits.

/// Whether `a` and `b` are within optimal-string-alignment distance 1: equal, or one
/// substitution, insertion, deletion or transposition of adjacent characters apart.
/// Linear in the shorter length.
pub fn osa_within_one(a: &[char], b: &[char]) -> bool {
    if a.len().abs_diff(b.len()) > 1 {
        return false;
    }
    let i = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    if i == a.len() && i == b.len() {
        return true;
    }
    match a.len().cmp(&b.len()) {
        std::cmp::Ordering::Equal => {
            a[i + 1..] == b[i + 1..]
                || (i + 1 < a.len()
                    && a[i] == b[i + 1]
                    && a[i + 1] == b[i]
                    && a[i + 2..] == b[i + 2..])
        }
        std::cmp::Ordering::Greater => a[i + 1..] == b[i..],
        std::cmp::Ordering::Less => a[i..] == b[i + 1..],
    }
}
