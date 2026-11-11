//! Saved-data integration recovered from MarkdownView.loadFileInternal/e2.
//! The Rust DMP port operates on explicit UTF-16 units with JS character classes.
//! Rare fuzzy-match numeric threshold/very-large line-index differences remain
//! bounded by the differential evidence, rather than claimed fully certified.
use crate::{EditorBuffer, EngineError};
use diff_match_patch_rs::{traits::DType, DiffMatchPatch, PatchInput};

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
struct Unit(u16);
impl DType for Unit {
    fn from_char(c: char) -> Self {
        Self(c as u16)
    }
    fn as_char(&self) -> Option<char> {
        char::from_u32(u32::from(self.0))
    }
    fn from_str(text: &str) -> Vec<Self> {
        text.encode_utf16().map(Self).collect()
    }
    fn to_string(units: &[Self]) -> Result<String, diff_match_patch_rs::Error> {
        String::from_utf16(&units.iter().map(|u| u.0).collect::<Vec<_>>())
            .map_err(|_| diff_match_patch_rs::Error::Utf8Error)
    }
    fn is_whitespace(self) -> bool {
        self.as_char()
            .is_some_and(mnemosyne_core::vault_search::js_whitespace)
    }
    fn is_newline(self) -> bool {
        self.0 == 10
    }
    fn is_carriage(self) -> bool {
        self.0 == 13
    }
    fn is_alphanum(self) -> bool {
        matches!(self.0,48..=57|65..=90|97..=122)
    }
    fn is_linebreak_end(units: &[Self]) -> bool {
        units.ends_with(&[Self(10), Self(10)]) || units.ends_with(&[Self(10), Self(13), Self(10)])
    }
    fn is_linebreak_start(units: &[Self]) -> bool {
        units.starts_with(&[Self(10), Self(10)])
            || units.starts_with(&[Self(13), Self(10), Self(10)])
            || units.starts_with(&[Self(10), Self(13), Self(10)])
            || units.starts_with(&[Self(13), Self(10), Self(13), Self(10)])
    }
    // This private adapter only uses in-memory patches. These serialization
    // helpers satisfy the port's trait; no engine protocol exposes them.
    fn percent_encode(units: &[Self]) -> Vec<Self> {
        let text = String::from_utf16_lossy(&units.iter().map(|u| u.0).collect::<Vec<_>>());
        let chars = <char as DType>::percent_encode(&text.chars().collect::<Vec<_>>());
        Self::from_str(&chars.into_iter().collect::<String>())
    }
    fn percent_decode(units: &[Self]) -> Vec<Self> {
        let chars = units.iter().filter_map(Self::as_char).collect::<Vec<_>>();
        Self::from_str(
            &<char as DType>::percent_decode(&chars)
                .into_iter()
                .collect::<String>(),
        )
    }
}

pub(crate) fn merge(
    old_saved: &str,
    dirty_editor: &str,
    new_saved: &str,
) -> Result<String, EngineError> {
    let mut dmp = DiffMatchPatch::new();
    // The shipped e2 passes the explicit deadline 0 to initial diff_main, while
    // retaining default diffTimeout (so half-match remains enabled). This port's
    // zero-millisecond deadline has the same expired-deadline branch.
    dmp.set_timeout(Some(0));
    let patches = dmp
        .patch_make::<Unit>(PatchInput::new_text_text(old_saved, dirty_editor))
        .map_err(|e| EngineError::PendingEditorMerge(format!("{e:?}")))?;
    // patch_apply's internal diffs receive the normal one-second deadline.
    dmp.set_timeout(Some(1000));
    let (merged, _applied) = dmp
        .patch_apply(&patches, new_saved)
        .map_err(|e| EngineError::PendingEditorMerge(format!("{e:?}")))?;
    Ok(merged)
}

/// CodeMirror editor-mode `set` computes one changed block of whole lines,
/// trimming characters only when that block is one line in both documents.
/// Cursor mapping is ChangeDesc.mapPos's default negative association.
pub(crate) fn load_contents(buffer: &mut EditorBuffer, new_text: &str) -> Result<(), EngineError> {
    if buffer.text == new_text {
        return Ok(());
    }
    let old_lines = buffer.text.split('\n').collect::<Vec<_>>();
    let new_lines = new_text.split('\n').collect::<Vec<_>>();
    let old_count = old_lines.len();
    let new_count = new_lines.len();
    let mut first = 0;
    while first < old_count && first < new_count && old_lines[first] == new_lines[first] {
        first += 1;
    }
    let mut old_last = old_count - 1;
    let mut new_last = new_count - 1;
    while old_last > first && new_last > first && old_lines[old_last] == new_lines[new_last] {
        old_last -= 1;
        new_last -= 1;
    }
    let start_line = if first == old_count.min(new_count) {
        first - 1
    } else {
        first
    };
    let mut start = old_lines
        .iter()
        .take(start_line)
        .map(|s| s.len() + 1)
        .sum::<usize>();
    let mut end = old_lines
        .iter()
        .take(old_last)
        .map(|s| s.len() + 1)
        .sum::<usize>()
        + old_lines[old_last].len();
    if first == old_count.min(new_count) {
        start += old_lines[start_line].len();
    } else if first == old_last && first == new_last && first < old_count && first < new_count {
        let a = old_lines[first];
        let b = new_lines[first];
        let prefix = a
            .chars()
            .zip(b.chars())
            .take_while(|(a, b)| a == b)
            .map(|(c, _)| c.len_utf8())
            .sum::<usize>();
        let suffix = a[prefix..]
            .chars()
            .rev()
            .zip(b[prefix..].chars().rev())
            .take_while(|(a, b)| a == b)
            .map(|(c, _)| c.len_utf8())
            .sum::<usize>();
        start += prefix;
        end -= suffix;
    }
    let anchor = buffer.offset(buffer.selection.anchor)?;
    let head = buffer.offset(buffer.selection.head)?;
    // The whole-line algorithm's replacement has the same unchanged outer bytes.
    let inserted = new_text.len() - (buffer.text.len() - (end - start));
    let map = |pos: usize| {
        if pos < start {
            pos
        } else if pos < end || (pos == end && start == end) {
            start
        } else {
            pos - (end - start) + inserted
        }
    };
    let anchor = map(anchor);
    let head = map(head);
    buffer.text = new_text.into();
    buffer.selection.anchor = buffer.position(anchor);
    buffer.selection.head = buffer.position(head);
    Ok(())
}
