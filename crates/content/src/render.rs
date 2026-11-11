//! Presentation over stored text: line counts at publish, and `render`, which turns a
//! change into reading — paragraphs aligned, words diffed inside changed paragraphs,
//! rewrites flagged, long changes folded into pages. The text algorithms are plain
//! functions; `Service::render` at the end picks what the caller may see.
use super::*;
use std::ops::Range;

/// Lines of `text` for counting: CRLF is LF, and one trailing newline is not a line.
fn lines(text: &str) -> Vec<String> {
    let text = text.replace("\r\n", "\n");
    let text = text.strip_suffix('\n').unwrap_or(&text);
    if text.is_empty() {
        vec![]
    } else {
        text.split('\n').map(String::from).collect()
    }
}

/// `(added, removed)` lines turning `before` into `after`; `None` is a file that did not
/// exist. Line endings (`\r\n` against `\n`) and a trailing-newline-only difference do
/// not count. A line-level longest-common-subsequence diff, not a character diff.
/// Bounded: when the lines left between the common head and tail multiply to more than
/// `MAX_CELLS`, the diff is approximated by counting every one of them as added/removed.
const MAX_CELLS: usize = 10_000_000;

pub(crate) fn line_counts(before: Option<&str>, after: &str) -> (u32, u32) {
    let old = before.map(lines).unwrap_or_default();
    let new = lines(after);
    let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let old = &old[prefix..old.len() - suffix];
    let new = &new[prefix..new.len() - suffix];
    if old.len().saturating_mul(new.len()) > MAX_CELLS {
        return (new.len() as u32, old.len() as u32);
    }
    // Two-row LCS length over what is left between the common head and tail.
    let mut row = vec![0u32; new.len() + 1];
    for a in old {
        let mut diagonal = 0;
        for (j, b) in new.iter().enumerate() {
            let above = row[j + 1];
            row[j + 1] = if a == b {
                diagonal + 1
            } else {
                row[j + 1].max(row[j])
            };
            diagonal = above;
        }
    }
    let common = row[new.len()];
    (new.len() as u32 - common, old.len() as u32 - common)
}

/// Changed paragraphs on one page of a file.
const PAGE: usize = 12;
/// Without a `path`, files past this many render as summaries only (`next_offset: 0`).
const PAGED_FILES: usize = 24;
/// A file is rewritten when added + removed lines reach this share of its longer side.
const REWRITE: f64 = 0.6;
/// Cells of one alignment table (paragraphs or word tokens); larger inputs are
/// approximated as everything between the common head and tail removed and added.
const MAX_ALIGN_CELLS: usize = 2_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpanKind {
    Same,
    Added,
    Removed,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub kind: SpanKind,
    pub text: String,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParagraphKind {
    Same,
    Changed,
    Added,
    Removed,
}
/// One paragraph of the aligned before/after sequence; `index` is its position in that
/// sequence. A run of unchanged paragraphs is one `same` entry with its `lines`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Paragraph {
    pub index: u32,
    pub kind: ParagraphKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spans: Vec<Span>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fits: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflict: Option<bool>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    pub added_lines: u32,
    pub removed_lines: u32,
    pub paragraphs_changed: u32,
    pub rewritten: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileRender {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub moved_from: Option<String>,
    /// `text`, `new_version`, `meta`, `deleted` or `created`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<(Option<ranks::PathMeta>, Option<ranks::PathMeta>)>,
    pub summary: Summary,
    pub paragraphs: Vec<Paragraph>,
    pub next_offset: Option<usize>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RenderResult {
    /// The revision rendered, or the proposal's base revision.
    pub revision_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposal_id: Option<String>,
    pub stale: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_revision: Option<String>,
    pub files: Vec<FileRender>,
    /// Changes (inside `path`, when given) the caller may not see.
    pub hidden_paths: u32,
}

/// One step of an alignment of `a` against `b`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Equal(usize, usize),
    Delete(usize),
    Insert(usize),
}

/// A longest-common-subsequence alignment; deletions come before insertions where both
/// are possible. Bounded like `line_counts`: past `MAX_ALIGN_CELLS` the part between the
/// common head and tail is all deleted, then all inserted.
fn align<T: PartialEq>(a: &[T], b: &[T]) -> Vec<Op> {
    let prefix = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let suffix = a[prefix..]
        .iter()
        .rev()
        .zip(b[prefix..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let (m, n) = (a.len() - prefix - suffix, b.len() - prefix - suffix);
    let mut ops: Vec<Op> = (0..prefix).map(|i| Op::Equal(i, i)).collect();
    if m.saturating_add(1).saturating_mul(n.saturating_add(1)) > MAX_ALIGN_CELLS {
        ops.extend((prefix..prefix + m).map(Op::Delete));
        ops.extend((prefix..prefix + n).map(Op::Insert));
    } else {
        let (x, y) = (&a[prefix..prefix + m], &b[prefix..prefix + n]);
        // table[i][j] = LCS length of x[i..] and y[j..].
        let w = n + 1;
        let mut table = vec![0u32; (m + 1) * w];
        for i in (0..m).rev() {
            for j in (0..n).rev() {
                table[i * w + j] = if x[i] == y[j] {
                    table[(i + 1) * w + j + 1] + 1
                } else {
                    table[(i + 1) * w + j].max(table[i * w + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < m || j < n {
            if i < m && j < n && x[i] == y[j] {
                ops.push(Op::Equal(prefix + i, prefix + j));
                i += 1;
                j += 1;
            } else if i < m && (j == n || table[(i + 1) * w + j] >= table[i * w + j + 1]) {
                ops.push(Op::Delete(prefix + i));
                i += 1;
            } else {
                ops.push(Op::Insert(prefix + j));
                j += 1;
            }
        }
    }
    let (sa, sb) = (a.len() - suffix, b.len() - suffix);
    ops.extend((0..suffix).map(|k| Op::Equal(sa + k, sb + k)));
    ops
}

/// A paragraph: its text (lines joined by `\n`) and how many lines it has.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Para {
    text: String,
    lines: u32,
}

/// Paragraphs of `text`: runs of lines separated by one or more blank lines, a fenced
/// code block (``` … ```) kept whole. CRLF is LF; trailing newlines do not matter.
fn paragraphs(text: &str) -> Vec<Para> {
    paragraph_spans(text)
        .into_iter()
        .map(|span| {
            let text = text[span].replace("\r\n", "\n");
            Para {
                lines: text.split('\n').count() as u32,
                text,
            }
        })
        .collect()
}

/// Where each of `paragraphs(text)` lies in `text`: from its first line's start to its
/// last line's end, without that line's `\r\n` or `\n`.
fn paragraph_spans(text: &str) -> Vec<Range<usize>> {
    let mut out = vec![];
    let mut current: Option<Range<usize>> = None;
    let mut fenced = false;
    let mut start = 0;
    for raw in text.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        let here = start..start + line.len();
        start += raw.len() + 1;
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
        } else if !fenced && line.trim().is_empty() {
            out.extend(current.take());
            continue;
        }
        current = Some(current.map_or(here.clone(), |c| c.start..here.end));
    }
    out.extend(current);
    out
}

/// A paragraph-level step from `before` to `after`. `Added` carries the base paragraphs
/// its hunk replaced (empty: inserted before base paragraph `range.start`).
#[derive(Clone, Debug, PartialEq, Eq)]
enum Step {
    Same(usize, usize),
    Changed(usize, usize),
    Removed(usize),
    Added(usize, Range<usize>),
}

/// Paragraph alignment; inside a hunk of removed and added paragraphs the first ones pair
/// up as changed paragraphs, the rest stay removed or added.
fn steps(before: &[Para], after: &[Para]) -> Vec<Step> {
    let a: Vec<&str> = before.iter().map(|p| p.text.as_str()).collect();
    let b: Vec<&str> = after.iter().map(|p| p.text.as_str()).collect();
    let ops = align(&a, &b);
    let mut out = vec![];
    let mut k = 0;
    while k < ops.len() {
        if let Op::Equal(i, j) = ops[k] {
            out.push(Step::Same(i, j));
            k += 1;
            continue;
        }
        let (mut removed, mut added) = (vec![], vec![]);
        while let Some(op) = ops.get(k).filter(|op| !matches!(op, Op::Equal(..))) {
            match *op {
                Op::Delete(i) => removed.push(i),
                Op::Insert(j) => added.push(j),
                Op::Equal(..) => unreachable!(),
            }
            k += 1;
        }
        let start = match ops.get(k) {
            _ if !removed.is_empty() => removed[0],
            Some(Op::Equal(i, _)) => *i,
            _ => before.len(),
        };
        let hunk = start..start + removed.len();
        let pairs = removed.len().min(added.len());
        for p in 0..pairs {
            out.push(Step::Changed(removed[p], added[p]));
        }
        out.extend(removed[pairs..].iter().map(|&i| Step::Removed(i)));
        out.extend(added[pairs..].iter().map(|&j| Step::Added(j, hunk.clone())));
    }
    out
}

/// Word tokens: runs of letters and digits, runs of whitespace, and single other
/// characters, so that their concatenation is the text.
fn tokens(text: &str) -> Vec<&str> {
    #[derive(PartialEq)]
    enum Class {
        Word,
        Space,
        Other,
    }
    let class = |c: char| {
        if c.is_alphanumeric() {
            Class::Word
        } else if c.is_whitespace() {
            Class::Space
        } else {
            Class::Other
        }
    };
    let mut out = vec![];
    let mut start = 0;
    let mut last: Option<Class> = None;
    for (i, c) in text.char_indices() {
        let k = class(c);
        if i > start && (last.as_ref() != Some(&k) || k == Class::Other) {
            out.push(&text[start..i]);
            start = i;
        }
        last = Some(k);
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

/// How good a place the boundary between two tokens is for an edit to start or end:
/// the text's edges, then line breaks, sentence ends, whitespace, punctuation.
fn boundary(left: Option<&str>, right: Option<&str>) -> u8 {
    let space = |t: &str| t.chars().all(char::is_whitespace);
    let word = |t: &str| t.chars().all(char::is_alphanumeric);
    match (left, right) {
        (None, _) | (_, None) => 6,
        (Some(l), Some(r)) if l.ends_with('\n') || r.starts_with('\n') => 5,
        (Some(l), Some(r)) if matches!(l, "." | "!" | "?" | ";" | ":") && space(r) => 4,
        (Some(l), Some(r)) if space(l) || space(r) => 3,
        (Some(l), Some(r)) if !word(l) || !word(r) => 2,
        _ => 1,
    }
}

#[derive(Debug)]
enum Seg<'a> {
    Same(Vec<&'a str>),
    Change(Vec<&'a str>, Vec<&'a str>),
}

/// Word spans turning `before` into `after`, with a semantic cleanup: a pure insertion or
/// deletion slides to the best word boundary, and whitespace or punctuation alone between
/// two edits joins them.
fn spans(before: &str, after: &str) -> Vec<Span> {
    let (a, b) = (tokens(before), tokens(after));
    // Equal runs, possibly empty, on both sides of every change.
    let mut segs: Vec<Seg> = vec![Seg::Same(vec![])];
    for op in align(&a, &b) {
        match (op, segs.last_mut()) {
            (Op::Equal(i, _), Some(Seg::Same(t))) => t.push(a[i]),
            (Op::Equal(i, _), _) => segs.push(Seg::Same(vec![a[i]])),
            (Op::Delete(i), Some(Seg::Change(d, _))) => d.push(a[i]),
            (Op::Insert(j), Some(Seg::Change(_, n))) => n.push(b[j]),
            (Op::Delete(i), _) => segs.push(Seg::Change(vec![a[i]], vec![])),
            (Op::Insert(j), _) => segs.push(Seg::Change(vec![], vec![b[j]])),
        }
    }
    segs.push(Seg::Same(vec![]));
    slide(&mut segs);
    let mut out: Vec<Seg> = vec![];
    for seg in segs {
        match seg {
            Seg::Same(t) if t.is_empty() => {}
            Seg::Same(t) => match out.last_mut() {
                Some(Seg::Same(p)) => p.extend(t),
                _ => out.push(Seg::Same(t)),
            },
            Seg::Change(d, n) => {
                let glue = match (out.len().checked_sub(2).map(|k| &out[k]), out.last()) {
                    (Some(Seg::Change(..)), Some(Seg::Same(t)))
                        if !t.iter().any(|x| x.chars().any(char::is_alphanumeric)) =>
                    {
                        match out.pop() {
                            Some(Seg::Same(t)) => t,
                            _ => unreachable!(),
                        }
                    }
                    _ => vec![],
                };
                match out.last_mut() {
                    Some(Seg::Change(pd, pn)) => {
                        pd.extend(glue.iter().copied().chain(d));
                        pn.extend(glue.into_iter().chain(n));
                    }
                    _ => out.push(Seg::Change(d, n)),
                }
            }
        }
    }
    let mut result = vec![];
    for seg in out {
        let mut push = |kind, t: Vec<&str>| {
            if !t.is_empty() {
                result.push(Span {
                    kind,
                    text: t.concat(),
                })
            }
        };
        match seg {
            Seg::Same(t) => push(SpanKind::Same, t),
            Seg::Change(d, n) => {
                push(SpanKind::Removed, d);
                push(SpanKind::Added, n);
            }
        }
    }
    result
}

/// How far, in tokens, a pure edit may slide either way to find a better boundary.
const MAX_SLIDE: usize = 64;

/// Moves each pure insertion or deletion between its neighbouring equal runs to the
/// position whose two boundaries score best (leftmost on ties); the text is unchanged.
/// Neighbours of a change are always equal runs: consecutive changes are one segment.
/// Bounded: the edit is a window over its neighbours joined, each step is O(1), and it
/// moves at most `MAX_SLIDE` tokens left of where it was and `MAX_SLIDE` right.
fn slide(segs: &mut [Seg]) {
    fn same<'a>(seg: Option<&mut Seg<'a>>) -> Vec<&'a str> {
        match seg {
            Some(Seg::Same(t)) => std::mem::take(t),
            _ => vec![],
        }
    }
    for k in 0..segs.len() {
        let deletion = match &segs[k] {
            Seg::Change(d, n) if d.is_empty() != n.is_empty() => n.is_empty(),
            _ => continue,
        };
        let mut joined = same(k.checked_sub(1).and_then(|i| segs.get_mut(i)));
        let start = joined.len();
        let edit = match &mut segs[k] {
            Seg::Change(d, _) if deletion => std::mem::take(d),
            Seg::Change(_, n) => std::mem::take(n),
            Seg::Same(_) => unreachable!(),
        };
        let e = edit.len();
        joined.extend(edit);
        joined.extend(same(segs.get_mut(k + 1)));
        // The edit is joined[p..p + e]; sliding keeps the text when the token leaving one
        // end equals the token entering at the other.
        let c = &joined;
        let mut p = start;
        while p > 0 && start - p < MAX_SLIDE && c[p - 1] == c[p + e - 1] {
            p -= 1;
        }
        let score = |p: usize| {
            boundary(p.checked_sub(1).map(|i| c[i]), Some(c[p]))
                + boundary(Some(c[p + e - 1]), c.get(p + e).copied())
        };
        let (mut best, mut at) = (score(p), p);
        let mut q = p;
        while q + e < c.len() && q < start + MAX_SLIDE && c[q] == c[q + e] {
            q += 1;
            if score(q) > best {
                (best, at) = (score(q), q);
            }
        }
        let right = joined.split_off(at + e);
        let edit = joined.split_off(at);
        if let Some(Seg::Same(t)) = k.checked_sub(1).and_then(|i| segs.get_mut(i)) {
            *t = joined;
        }
        if let Some(Seg::Same(t)) = segs.get_mut(k + 1) {
            *t = right;
        }
        match &mut segs[k] {
            Seg::Change(d, _) if deletion => *d = edit,
            Seg::Change(_, n) => *n = edit,
            Seg::Same(_) => unreachable!(),
        }
    }
}

/// Which changed paragraphs a page holds: the range of steps it covers, from just after
/// the previous page's last changed paragraph, and the next page's offset.
fn page(steps: &[Step], offset: usize) -> (Range<usize>, Option<usize>) {
    let changed: Vec<usize> = steps
        .iter()
        .enumerate()
        .filter(|(_, s)| !matches!(s, Step::Same(..)))
        .map(|(k, _)| k)
        .collect();
    if offset > 0 && offset >= changed.len() {
        return (0..0, None);
    }
    let end = offset.saturating_add(PAGE).min(changed.len());
    let more = end < changed.len();
    let start = if offset == 0 {
        0
    } else {
        changed[offset - 1] + 1
    };
    let stop = if more {
        changed[end - 1] + 1
    } else {
        steps.len()
    };
    (start..stop, more.then_some(end))
}

/// Today's text at a stale proposal's file: `None` when the file is gone at head.
type Today = Option<String>;

/// The base paragraphs head changed or removed, and the gaps (before base paragraph `k`,
/// or at the end) where head inserted paragraphs.
fn touched(base: &[Para], head: &[Para]) -> (Vec<bool>, Vec<bool>) {
    let mut changed = vec![false; base.len()];
    let mut inserted = vec![false; base.len() + 1];
    for step in steps(base, head) {
        match step {
            Step::Changed(i, _) | Step::Removed(i) => changed[i] = true,
            Step::Added(_, hunk) if hunk.is_empty() => inserted[hunk.start] = true,
            // Inserted inside a hunk whose base paragraphs are already changed.
            Step::Added(..) | Step::Same(..) => {}
        }
    }
    (changed, inserted)
}

/// `head` with every paragraph change from `base` to `proposal` that fits applied (the
/// same three-way as a stale proposal's `fits`/`conflict`), conflicting ones dropped;
/// `None` when nothing fits. Paragraphs are aligned as `render` compares them (CRLF as
/// LF), but the answer is head's own bytes with only the fitting paragraphs spliced in:
/// head's separators, blank-line runs, leading and trailing bytes are kept, and a spliced
/// paragraph takes head's line ending. A file neither base nor head has text in is the
/// proposal as written.
pub(crate) fn merge_fitting(base: &str, head: &str, proposal: &str) -> Option<String> {
    let (old, now, new) = (paragraphs(base), paragraphs(head), paragraphs(proposal));
    if old.is_empty() && now.is_empty() {
        return (!new.is_empty()).then(|| proposal.to_owned());
    }
    let (changed, inserted) = touched(&old, &now);
    // Per base paragraph: replaced by a proposal paragraph (`Some(Some(j))`) or removed
    // (`Some(None)`); per gap, proposal paragraphs inserted before base paragraph `k`;
    // per base paragraph, the extra proposal paragraphs of the hunk that ends there.
    let mut replace: Vec<Option<Option<usize>>> = vec![None; old.len()];
    let mut before: Vec<Vec<usize>> = vec![vec![]; old.len() + 1];
    let mut after: Vec<Vec<usize>> = vec![vec![]; old.len()];
    let mut fits = false;
    for step in steps(&old, &new) {
        match step {
            Step::Same(..) => continue,
            Step::Changed(i, j) if !changed[i] => replace[i] = Some(Some(j)),
            Step::Removed(i) if !changed[i] => replace[i] = Some(None),
            Step::Added(j, hunk) if hunk.is_empty() && !inserted[hunk.start] => {
                before[hunk.start].push(j)
            }
            Step::Added(j, hunk) if !hunk.is_empty() && !changed[hunk.clone()].contains(&true) => {
                after[hunk.end - 1].push(j)
            }
            // Conflicts with what head changed since the base: dropped.
            _ => continue,
        }
        fits = true;
    }
    if !fits {
        return None;
    }
    /// One paragraph of the answer: head's paragraph `h` (as it is, or replaced) or a
    /// paragraph only the proposal has.
    enum Item {
        Head(usize, Option<usize>),
        New(usize),
    }
    let mut items = vec![];
    // Head's own order; a base paragraph head kept is where the proposal's change lands.
    for step in steps(&old, &now) {
        if let Step::Same(i, _) | Step::Changed(i, _) | Step::Removed(i) = step {
            items.extend(before[i].iter().map(|&j| Item::New(j)));
        }
        match step {
            Step::Same(i, h) => {
                match replace[i] {
                    None => items.push(Item::Head(h, None)),
                    Some(Some(j)) => items.push(Item::Head(h, Some(j))),
                    Some(None) => {}
                }
                items.extend(after[i].iter().map(|&j| Item::New(j)));
            }
            Step::Changed(_, h) | Step::Added(h, _) => items.push(Item::Head(h, None)),
            Step::Removed(_) => {}
        }
    }
    items.extend(before[old.len()].iter().map(|&j| Item::New(j)));
    let spans = paragraph_spans(head);
    let eol = if head.contains("\r\n") { "\r\n" } else { "\n" };
    let blank = eol.repeat(2);
    let ours = |j: usize| new[j].text.replace('\n', eol);
    // Bytes before the first paragraph and after the last are head's.
    let (lead, tail) = match (spans.first(), spans.last()) {
        (Some(first), Some(last)) => (&head[..first.start], &head[last.end..]),
        _ => (head, ""),
    };
    let mut out = String::from(lead);
    let mut previous: Option<&Item> = None;
    for item in &items {
        // After head's paragraph `h`, the separator head had there; otherwise one blank line.
        match previous {
            None => {}
            Some(Item::Head(h, _)) if h + 1 < spans.len() => {
                out.push_str(&head[spans[*h].end..spans[h + 1].start])
            }
            Some(_) => out.push_str(&blank),
        }
        match item {
            Item::Head(h, None) => out.push_str(&head[spans[*h].clone()]),
            Item::Head(_, Some(j)) | Item::New(j) => out.push_str(&ours(*j)),
        }
        previous = Some(item);
    }
    out.push_str(tail);
    Some(out)
}

/// Summary and one page of paragraphs turning `before` into `after`; `None` is a side
/// where the file does not exist. Without `paged`, the summary alone (no word spans). With `today`, every paragraph the change touches is
/// marked as fitting today's text or conflicting with what head changed since the base.
fn render_text(
    before: Option<&str>,
    after: Option<&str>,
    offset: usize,
    today: Option<&Today>,
    paged: bool,
) -> (Summary, Vec<Paragraph>, Option<usize>) {
    let (old, new) = (
        paragraphs(before.unwrap_or("")),
        paragraphs(after.unwrap_or("")),
    );
    let (added_lines, removed_lines) = line_counts(before, after.unwrap_or(""));
    let steps = steps(&old, &new);
    let longer = lines(before.unwrap_or(""))
        .len()
        .max(lines(after.unwrap_or("")).len())
        .max(1);
    let summary = Summary {
        added_lines,
        removed_lines,
        paragraphs_changed: steps
            .iter()
            .filter(|s| !matches!(s, Step::Same(..)))
            .count() as u32,
        rewritten: before.is_some()
            && after.is_some()
            && f64::from(added_lines + removed_lines) >= REWRITE * longer as f64,
    };
    if summary.rewritten {
        return (summary, vec![], None);
    }
    if !paged {
        // Summary only: the first page is read with the file's path, at offset 0.
        return (summary, vec![], (!steps.is_empty()).then_some(0));
    }
    let (range, next) = page(&steps, offset);
    let head = today.map(|t| t.as_deref().map(|text| touched(&old, &paragraphs(text))));
    let conflict = |step: &Step| match (&head, step) {
        (None, _) | (_, Step::Same(..)) => None,
        (Some(None), _) => Some(true),
        (Some(Some((changed, _))), Step::Changed(i, _) | Step::Removed(i)) => Some(changed[*i]),
        (Some(Some((_, inserted))), Step::Added(_, hunk)) if hunk.is_empty() => {
            Some(inserted[hunk.start])
        }
        (Some(Some((changed, _))), Step::Added(_, hunk)) => {
            Some(changed[hunk.clone()].iter().any(|c| *c))
        }
    };
    let mut out: Vec<Paragraph> = vec![];
    for k in range {
        let flag = conflict(&steps[k]);
        let paragraph = |kind, spans| Paragraph {
            index: k as u32,
            kind,
            lines: None,
            spans,
            fits: flag.map(|c| !c),
            conflict: flag,
        };
        let one = |kind, text: &str| {
            vec![Span {
                kind,
                text: text.into(),
            }]
        };
        let item = match &steps[k] {
            Step::Same(_, j) => {
                if let Some(p) = out.last_mut().filter(|p| p.kind == ParagraphKind::Same) {
                    p.lines = Some(p.lines.unwrap_or(0) + new[*j].lines);
                    continue;
                }
                Paragraph {
                    lines: Some(new[*j].lines),
                    ..paragraph(ParagraphKind::Same, vec![])
                }
            }
            Step::Changed(i, j) => {
                paragraph(ParagraphKind::Changed, spans(&old[*i].text, &new[*j].text))
            }
            Step::Removed(i) => paragraph(
                ParagraphKind::Removed,
                one(SpanKind::Removed, &old[*i].text),
            ),
            Step::Added(j, _) => {
                paragraph(ParagraphKind::Added, one(SpanKind::Added, &new[*j].text))
            }
        };
        out.push(item);
    }
    (summary, out, next)
}

/// Ok when `text` is the stored text of `blob`: records whose texts were not kept (or a
/// legacy binary change, whose kind was not recorded) cannot be rendered.
fn available(text: Option<&str>, blob: Option<&str>) -> Result<()> {
    match blob {
        Some(b) if text.is_none_or(|t| format!("{:x}", Sha256::digest(t.as_bytes())) != b) => {
            Err(err("render_unavailable"))
        }
        _ => Ok(()),
    }
}

/// One file of a revision or proposal, page `offset` of its paragraphs (only its summary
/// without `paged`); `today` for a stale proposal.
fn file(d: &FileDiff, offset: usize, today: Option<&Today>, paged: bool) -> Result<FileRender> {
    let mut out = FileRender {
        path: d.path.clone(),
        moved_from: d.previous_path.clone(),
        kind: "text".into(),
        meta: None,
        summary: Summary::default(),
        paragraphs: vec![],
        next_offset: None,
    };
    let text = matches!(d.kind, Some(EntryKind::Text) | None);
    let existed = d.before.is_some()
        || d.before_blob.is_some()
        || d.previous_path.is_some()
        || d.meta_before.is_some();
    let (before, after) = if d.meta_before != d.meta_after && d.before_blob == d.after_blob {
        out.kind = "meta".into();
        out.meta = Some((d.meta_before.clone(), d.meta_after.clone()));
        return Ok(out);
    } else if d.deleted {
        out.kind = "deleted".into();
        if !text {
            return Ok(out);
        }
        available(d.before.as_deref(), d.before_blob.as_deref())?;
        (d.before.as_deref(), None)
    } else if !existed {
        out.kind = "created".into();
        if !text {
            return Ok(out);
        }
        available(Some(&d.after), d.after_blob.as_deref())?;
        (None, Some(d.after.as_str()))
    } else if d.kind == Some(EntryKind::Binary)
        || (d.kind == Some(EntryKind::Text) && d.before.is_none() && d.before_blob.is_some())
    {
        out.kind = "new_version".into();
        return Ok(out);
    } else if !text {
        // A folder moved: nothing to read inside it.
        return Ok(out);
    } else {
        available(d.before.as_deref(), d.before_blob.as_deref())?;
        available(Some(&d.after), d.after_blob.as_deref())?;
        (d.before.as_deref(), Some(d.after.as_str()))
    };
    (out.summary, out.paragraphs, out.next_offset) =
        render_text(before, after, offset, today, paged);
    Ok(out)
}

/// A text change with paragraphs: not binary, folder or meta-only.
fn compares_text(d: &FileDiff) -> bool {
    matches!(d.kind, Some(EntryKind::Text) | None)
        && !(d.meta_before != d.meta_after && d.before_blob == d.after_blob)
}

/// The change touches `path` (by its name now or before a move); any change without one.
fn touches(c: &FileDiff, path: Option<&str>) -> bool {
    path.is_none_or(|p| c.path == p || c.previous_path.as_deref() == Some(p))
}

impl Service {
    /// Today's text of the file a proposal changes: the head entry with its identity,
    /// wherever it has moved; for a file the proposal creates, whatever head has at its
    /// path under any identity (head created it too). Gone when the base had it and head
    /// does not (or it is no longer text); empty when neither had it. Called for text
    /// changes only, so no binary or meta change reads a head blob.
    fn today(&self, files: &BTreeMap<String, Entry>, d: &FileDiff) -> Result<Today> {
        Ok(self.today_at(files, d)?.map(|(_, text)| text))
    }

    /// `today`, with the path the file has at head (its own when the proposal creates it).
    fn today_at(
        &self,
        files: &BTreeMap<String, Entry>,
        d: &FileDiff,
    ) -> Result<Option<(String, String)>> {
        let id = d.file_id.as_ref();
        let created = d.before.is_none() && d.before_blob.is_none();
        let entry = files
            .get_key_value(&d.path)
            .filter(|(_, e)| created || Some(&e.file_id) == id)
            .or_else(|| files.iter().find(|(_, e)| Some(&e.file_id) == id));
        match entry {
            Some((path, e)) if e.kind == EntryKind::Text => Ok(Some((
                path.clone(),
                String::from_utf8(storage::read_blob(
                    &self.root,
                    e.blob.as_deref().ok_or_else(|| err("corrupt_blob"))?,
                )?)
                .map_err(io)?,
            ))),
            Some(_) => Ok(None),
            None if !created => Ok(None),
            None => Ok(Some((d.path.clone(), String::new()))),
        }
    }

    /// A stale proposal's text change `d` (proposing `proposed`) carried onto head: the
    /// path the file has at head and its text with what fits applied; `None` when the
    /// file is gone at head or nothing in it fits.
    pub(crate) fn rebase_file(
        &self,
        files: &BTreeMap<String, Entry>,
        d: &FileDiff,
        proposed: &str,
    ) -> Result<Option<(String, String)>> {
        available(d.before.as_deref(), d.before_blob.as_deref())?;
        let Some((path, head)) = self.today_at(files, d)? else {
            return Ok(None);
        };
        Ok(
            merge_fitting(d.before.as_deref().unwrap_or(""), &head, proposed)
                .map(|text| (path, text)),
        )
    }

    /// A revision's or a proposal's change as reading. Exactly one id. Without `path`, the
    /// first page of the first 24 files the caller may see and the summaries of the rest
    /// (`next_offset: 0`); with it, page `offset` of that file.
    pub fn render(
        &self,
        who: &TrustedIdentity,
        revision_id: Option<&str>,
        proposal_id: Option<&str>,
        path: Option<&str>,
        offset: usize,
    ) -> Result<RenderResult> {
        if let Some(path) = path {
            valid_path(path)?;
        } else if offset > 0 {
            return Err(err("invalid_request"));
        }
        match (revision_id, proposal_id) {
            (Some(id), None) => self.render_revision(who, id, path, offset),
            (None, Some(id)) => self.render_proposal(who, id, path, offset),
            _ => Err(err("invalid_request")),
        }
    }

    /// As `log`: the files readable now and then; the rest are counted, not shown.
    fn render_revision(
        &self,
        who: &TrustedIdentity,
        id: &str,
        path: Option<&str>,
        offset: usize,
    ) -> Result<RenderResult> {
        // The parent manifest decides what was readable before; revisions never change parents.
        let parent =
            self.transaction_for(who, false, vec![id.to_owned()], false, None, None, |s| {
                Ok(s.tenants
                    .get(&who.tenant_id)
                    .and_then(|t| t.revisions.get(id))
                    .and_then(|snap| snap.revision.parent.clone()))
            })?;
        let revisions = std::iter::once(id.to_owned()).chain(parent).collect();
        self.transaction_for(who, false, revisions, false, None, None, |s| {
            read_tenant(s, who)?;
            let t = s
                .tenants
                .get(&who.tenant_id)
                .ok_or_else(|| err("not_found"))?;
            let r = &t
                .revisions
                .get(id)
                .ok_or_else(|| t.missing_revision(id))?
                .revision;
            let scoped: Vec<&FileDiff> = r.changes.iter().filter(|c| touches(c, path)).collect();
            let visible: Vec<&FileDiff> = scoped
                .iter()
                .copied()
                .filter(|c| {
                    authorize_diff(s, who, c).is_ok()
                        && readable_then(s, who, t, r, std::slice::from_ref(*c)).is_ok()
                })
                .collect();
            if visible.is_empty() {
                return Err(err("not_found"));
            }
            Ok(RenderResult {
                revision_id: id.into(),
                proposal_id: None,
                stale: false,
                head_revision: None,
                files: visible
                    .iter()
                    .enumerate()
                    .map(|(k, c)| file(c, offset, None, k < PAGED_FILES))
                    .collect::<Result<_>>()?,
                hidden_paths: (scoped.len() - visible.len()) as u32,
            })
        })
    }

    /// A proposal the caller may review, rendered from its base to what it proposes. A
    /// proposal not yet applied a path of which changed after its base (by identity or
    /// name, renames included) is stale, and is also compared with today's text,
    /// paragraph by paragraph. A head that moved on other files only is not stale.
    fn render_proposal(
        &self,
        who: &TrustedIdentity,
        id: &str,
        path: Option<&str>,
        offset: usize,
    ) -> Result<RenderResult> {
        self.with_chain(who, |chain| {
            self.transaction_chain(who, false, vec![], Some(id), None, chain, |s| {
                let p = reviewable(s, who, id)?;
                let scoped: Vec<&FileDiff> = p.diff.iter().filter(|c| touches(c, path)).collect();
                if scoped.is_empty() {
                    return Err(err("not_found"));
                }
                let head = s
                    .tenants
                    .get(&who.tenant_id)
                    .filter(|t| !t.head.is_empty())
                    .map(|t| {
                        let snap = t.revisions.get(&t.head);
                        (t.head.clone(), snap.map(|snap| &snap.files))
                    });
                let head_id = head.as_ref().map_or("r0", |(id, _)| id.as_str());
                let stale = matches!(p.state, ProposalState::Pending | ProposalState::Approved)
                    && touched_since(
                        s,
                        &who.tenant_id,
                        &p.base_revision,
                        &Touched::of_proposal(&p),
                        chain,
                    )?;
                let empty = BTreeMap::new();
                let files = head
                    .as_ref()
                    .and_then(|(_, files)| *files)
                    .unwrap_or(&empty);
                let mut rendered = vec![];
                for (k, c) in scoped.into_iter().enumerate() {
                    let paged = k < PAGED_FILES;
                    // Only text read page by page is compared with today's text.
                    let today = (stale && paged && compares_text(c))
                        .then(|| self.today(files, c))
                        .transpose()?;
                    rendered.push(file(c, offset, today.as_ref(), paged)?);
                }
                Ok(RenderResult {
                    revision_id: p.base_revision.clone(),
                    proposal_id: Some(p.id.clone()),
                    stale,
                    head_revision: stale.then(|| head_id.to_owned()),
                    files: rendered,
                    hidden_paths: 0,
                })
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(spans: &[Span]) -> Vec<(SpanKind, &str)> {
        spans.iter().map(|s| (s.kind, s.text.as_str())).collect()
    }

    #[test]
    fn paragraphs_split_on_blank_lines_and_keep_fences_whole() {
        let p = paragraphs("a\r\nb\r\n\r\n\r\n  \nc\n```\nx\n\ny\n```\n\nd\n\n");
        let t: Vec<(&str, u32)> = p.iter().map(|p| (p.text.as_str(), p.lines)).collect();
        assert_eq!(t, vec![("a\nb", 2), ("c\n```\nx\n\ny\n```", 6), ("d", 1)]);
        assert!(paragraphs("").is_empty() && paragraphs("\n\n").is_empty());
        // An unclosed fence runs to the end.
        assert_eq!(paragraphs("```\na\n\nb").len(), 1);
    }

    #[test]
    fn words_diff_with_semantic_cleanup() {
        use SpanKind::*;
        // Whitespace alone between two edits joins them.
        assert_eq!(
            texts(&spans("lasts two years.", "lasts three months.")),
            vec![
                (Same, "lasts "),
                (Removed, "two years"),
                (Added, "three months"),
                (Same, ".")
            ]
        );
        // An insertion stays where it reads best, not where it slides furthest.
        assert_eq!(
            texts(&spans("the dog", "the cat the dog")),
            vec![(Added, "the cat "), (Same, "the dog")]
        );
        assert_eq!(texts(&spans("a b", "a b")), vec![(Same, "a b")]);
        assert_eq!(texts(&spans("", "new")), vec![(Added, "new")]);
        assert_eq!(
            texts(&spans("Ünïcode wörds", "Ünïcode wårds")),
            vec![(Same, "Ünïcode "), (Removed, "wörds"), (Added, "wårds")]
        );
    }

    #[test]
    fn paragraph_steps_pair_inside_a_hunk() {
        let p = |t: &str| paragraphs(t);
        assert_eq!(
            steps(&p("a\n\nb\n\nc"), &p("a\n\nB\n\nX\n\nc")),
            vec![
                Step::Same(0, 0),
                Step::Changed(1, 1),
                Step::Added(2, 1..2),
                Step::Same(2, 3)
            ]
        );
        assert_eq!(
            steps(&p("a\n\nc"), &p("a\n\nb\n\nc")),
            vec![Step::Same(0, 0), Step::Added(1, 1..1), Step::Same(1, 2)]
        );
        assert_eq!(
            steps(&p("a\n\nb"), &p("a")),
            vec![Step::Same(0, 0), Step::Removed(1)]
        );
    }

    #[test]
    fn alignment_is_bounded() {
        let a: Vec<String> = (0..3000).map(|i| format!("a{i}")).collect();
        let b: Vec<String> = (0..3000).map(|i| format!("b{i}")).collect();
        let start = std::time::Instant::now();
        let ops = align(&a, &b);
        assert_eq!(ops.len(), 6000);
        assert!(matches!(ops[0], Op::Delete(0)) && matches!(ops[3000], Op::Insert(0)));
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
    }

    /// `(index, conflict)` of the paragraphs a change touches, against `today`.
    fn against(base: Option<&str>, proposed: Option<&str>, today: Today) -> Vec<(u32, bool)> {
        render_text(base, proposed, 0, Some(&today), true)
            .1
            .iter()
            .filter(|p| p.kind != ParagraphKind::Same)
            .map(|p| {
                assert_eq!(p.fits, p.conflict.map(|c| !c));
                (p.index, p.conflict.unwrap())
            })
            .collect()
    }

    #[test]
    fn merge_fitting_applies_what_fits_in_heads_order() {
        let base = "a\n\nb\n\nc\n\nd\n";
        // Head changed b and inserted H before d.
        let head = "a\n\nB\n\nc\n\nH\n\nd\n";
        let m = |p: &str| merge_fitting(base, head, p);
        // Replacing a fits; replacing b conflicts and is dropped.
        assert_eq!(
            m("A\n\nb2\n\nc\n\nd\n").as_deref(),
            Some("A\n\nB\n\nc\n\nH\n\nd\n")
        );
        // Removing c fits, appending fits; inserting before d conflicts with H.
        assert_eq!(
            m("a\n\nb\n\nd\n\nZ").as_deref(),
            Some("a\n\nB\n\nH\n\nd\n\nZ\n")
        );
        assert_eq!(m("a\n\nb\n\nc\n\nY\n\nd\n"), None);
        // c replaced by two paragraphs: both land where c was.
        assert_eq!(
            m("a\n\nb\n\nc1\n\nc2\n\nd\n").as_deref(),
            Some("a\n\nB\n\nc1\n\nc2\n\nH\n\nd\n")
        );
        // Inserting before a fits (head inserted nothing there).
        assert_eq!(
            m("N\n\na\n\nb\n\nc\n\nd\n").as_deref(),
            Some("N\n\na\n\nB\n\nc\n\nH\n\nd\n")
        );
        // Only conflicts, or no change at all: nothing fits.
        assert_eq!(m("a\n\nb2\n\nc\n\nd\n"), None);
        assert_eq!(m(base), None);
        // A file the proposal creates fits only into an empty head.
        assert_eq!(merge_fitting("", "", "n\n").as_deref(), Some("n\n"));
        assert_eq!(merge_fitting("", "h", "n"), None);
    }

    #[test]
    fn three_way_conflicts_only_where_head_touched_the_same_base() {
        let base = "a\n\nb\n\nc\n\nd\n\ne\n\nf";
        let today = |t: &str| Some(t.to_owned());
        // Head inserted between b and c; the proposal inserts there too, and elsewhere.
        let proposed = "a\n\nb\n\nX\n\nc\n\nd\n\ne\n\nf\n\nY";
        assert_eq!(
            against(
                Some(base),
                Some(proposed),
                today("a\n\nb\n\nH\n\nc\n\nd\n\ne\n\nf")
            ),
            vec![(2, true), (7, false)]
        );
        // Head removed d; the proposal replaced d with two paragraphs.
        let proposed = "a\n\nb\n\nc\n\nD1\n\nD2\n\ne\n\nf";
        assert_eq!(
            against(Some(base), Some(proposed), today("a\n\nb\n\nc\n\ne\n\nf")),
            vec![(3, true), (4, true)]
        );
        assert_eq!(
            against(
                Some(base),
                Some(proposed),
                today("A\n\nb\n\nc\n\nd\n\ne\n\nF")
            ),
            vec![(3, false), (4, false)]
        );
        // Gone at head: everything the proposal touched conflicts.
        assert_eq!(
            against(Some(base), Some(proposed), None),
            vec![(3, true), (4, true)]
        );
        // Created by the proposal and, meanwhile, at head.
        assert_eq!(against(None, Some("n"), today("h")), vec![(0, true)]);
        assert_eq!(against(None, Some("n"), today("")), vec![(0, false)]);
    }

    #[test]
    fn span_cleanup_is_bounded_on_repetitive_text() {
        // 1,000,000 tokens ("a" and " " alternating), one more unit appended: the
        // insertion could slide across the whole run.
        let before = "a ".repeat(500_000);
        let after = format!("{before}a ");
        let start = std::time::Instant::now();
        let s = spans(&before, &after);
        let elapsed = start.elapsed();
        assert_eq!(s.iter().map(|s| s.text.len()).sum::<usize>(), after.len());
        assert_eq!(s.iter().filter(|s| s.kind == SpanKind::Added).count(), 1);
        assert!(elapsed < std::time::Duration::from_secs(2), "{elapsed:?}");
        eprintln!("span cleanup over 1,000,000 tokens: {elapsed:?}");
    }

    fn text_diff(before: Option<&str>, after: &str) -> FileDiff {
        let blob = |t: &str| format!("{:x}", Sha256::digest(t.as_bytes()));
        FileDiff {
            path: "a.md".into(),
            before: before.map(String::from),
            after: after.into(),
            file_id: Some("f1".into()),
            previous_path: None,
            deleted: false,
            before_blob: before.map(blob),
            after_blob: Some(blob(after)),
            kind: Some(EntryKind::Text),
            meta_before: None,
            meta_after: None,
            added_lines: None,
            removed_lines: None,
        }
    }

    #[test]
    fn texts_that_were_not_kept_are_unavailable() {
        let d = text_diff(Some("a\n"), "b\n");
        assert_eq!(
            file(&d, 0, None, true).unwrap().summary.paragraphs_changed,
            1
        );
        // The stored text does not match its blob: it was not kept.
        let mut lost = d.clone();
        lost.after = String::new();
        assert_eq!(
            file(&lost, 0, None, true).unwrap_err().code,
            "render_unavailable"
        );
        let mut lost = d.clone();
        lost.before = None;
        lost.kind = None;
        assert_eq!(
            file(&lost, 0, None, true).unwrap_err().code,
            "render_unavailable"
        );
        // A legacy binary change: no kind, no text for its blob.
        let mut legacy = text_diff(None, "");
        legacy.kind = None;
        legacy.after_blob = Some(format!("{:x}", Sha256::digest([1u8, 2])));
        assert_eq!(
            file(&legacy, 0, None, true).unwrap_err().code,
            "render_unavailable"
        );
        // A legacy text change without blobs renders from its texts.
        let mut plain = text_diff(Some("a"), "b");
        (plain.kind, plain.before_blob, plain.after_blob) = (None, None, None);
        assert_eq!(file(&plain, 0, None, true).unwrap().kind, "text");
    }

    #[test]
    fn counts_lines_not_characters() {
        assert_eq!(line_counts(Some("a\nb\n"), "a\nc\nd\n"), (2, 1));
        assert_eq!(line_counts(None, "a\nb\n"), (2, 0));
        assert_eq!(line_counts(Some("a\nb\nc"), ""), (0, 3));
        assert_eq!(line_counts(None, ""), (0, 0));
        assert_eq!(line_counts(Some("a\r\nb\r\n"), "a\nb"), (0, 0));
        assert_eq!(line_counts(Some("a\n\n"), "a\n"), (0, 1));
        assert_eq!(line_counts(Some("a\nb\nc\nd"), "b\nd\ne"), (1, 2));
    }

    #[test]
    fn huge_all_different_inputs_are_bounded() {
        let text = |p: &str| (0..4000).map(|i| format!("{p}{i}\n")).collect::<String>();
        let start = std::time::Instant::now();
        assert_eq!(line_counts(Some(&text("a")), &text("b")), (4000, 4000));
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
    }
}
