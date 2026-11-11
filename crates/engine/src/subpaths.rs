//! Markdown ephemeral subpath projection: shipped $m + L8.setEphemeralState.
//! Resolves saved metadata, moves the editor cursor to the highlighted range's
//! start, and leaves the cursor alone for absent subpaths. Rendering/highlight
//! decoration and PDF/media fragment handlers are separate view backends.
use crate::{EditorPosition, EditorSelection, Engine, EngineError};
use mnemosyne_core::cache::{CacheLoc, CachedMetadata};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubpathKind {
    Heading,
    Block,
    Footnote,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubpathLocation {
    pub kind: SubpathKind,
    pub start: EditorPosition,
    pub end: Option<EditorPosition>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubpathApplyResult {
    Applied(SubpathLocation),
    Reading(SubpathLocation),
    NotFound,
    MetadataUnavailable,
    NoEditor,
}
fn position(loc: CacheLoc) -> EditorPosition {
    EditorPosition {
        line: loc.line as i64,
        ch: loc.col as i64,
    }
}
fn location(kind: SubpathKind, start: CacheLoc, end: Option<CacheLoc>) -> SubpathLocation {
    SubpathLocation {
        kind,
        start: position(start),
        end: end.map(position),
    }
}

pub fn resolve(cache: &CachedMetadata, subpath: &str) -> Option<SubpathLocation> {
    let parts = subpath
        .split('#')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if parts.is_empty() {
        return None;
    }
    if parts.len() == 1 {
        if let Some(id) = parts[0].strip_prefix('^') {
            if let Some(block) = cache.blocks.get(&id.to_lowercase()) {
                return Some(location(
                    SubpathKind::Block,
                    block.position.start,
                    Some(block.position.end),
                ));
            }
        } else if let Some(id) = parts[0].strip_prefix("[^") {
            // Native slice(2,-1), including malformed non-closing delimiters.
            let id = id
                .char_indices()
                .next_back()
                .map_or("", |(end, _)| &id[..end]);
            if let Some(footnote) = cache.footnotes.iter().find(|footnote| footnote.id == id) {
                return Some(location(
                    SubpathKind::Footnote,
                    footnote.position.start,
                    Some(footnote.position.end),
                ));
            }
        }
    }
    let mut matched = 0;
    let mut level = 0;
    let mut found = None;
    for heading in &cache.headings {
        if found.is_some() && heading.level <= level {
            let start = found?;
            return Some(location(
                SubpathKind::Heading,
                start,
                Some(heading.position.start),
            ));
        }
        if found.is_none()
            && heading.level > level
            && mnemosyne_core::strip_heading(&heading.heading).to_lowercase()
                == mnemosyne_core::strip_heading(parts[matched]).to_lowercase()
        {
            matched += 1;
            level = heading.level;
            if matched == parts.len() {
                found = Some(heading.position.start);
            }
        }
    }
    found.map(|start| location(SubpathKind::Heading, start, None))
}
impl Engine {
    pub fn apply_subpath(&mut self, subpath: &str) -> Result<SubpathApplyResult, EngineError> {
        let path = self
            .active_file()
            .ok_or(EngineError::NoActiveFile)?
            .to_owned();
        if self.editor().is_none() {
            return Ok(SubpathApplyResult::NoEditor);
        }
        let Some(cache) = self.file_cache(&path) else {
            return Ok(SubpathApplyResult::MetadataUnavailable);
        };
        let Some(location) = resolve(cache, subpath) else {
            return Ok(SubpathApplyResult::NotFound);
        };
        if self
            .markdown_state()
            .is_some_and(|state| state.mode == crate::markdown::MarkdownMode::Preview)
        {
            let id = self.session.active_leaf.as_deref().unwrap();
            self.session
                .leaves
                .iter_mut()
                .find(|leaf| leaf.id == id)
                .unwrap()
                .markdown
                .as_mut()
                .unwrap()
                .requested_reading_range = Some(location.clone());
            return Ok(SubpathApplyResult::Reading(location));
        }
        self.editor_mut().unwrap().set_selection(EditorSelection {
            anchor: location.start,
            head: location.start,
        })?;
        Ok(SubpathApplyResult::Applied(location))
    }
}
