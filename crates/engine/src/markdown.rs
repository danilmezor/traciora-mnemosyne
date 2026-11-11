//! Retained MarkdownView mode/data state, independent of UI rendering.
//! L8 owns both edit and reading backends. Its editor getter remains available
//! in preview; current getViewData/dirty/save semantics are mode-sensitive.
use crate::{editor_merge, normalize_editor_newlines, Engine, EngineError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MarkdownMode {
    #[default]
    Source,
    Preview,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkdownViewState {
    pub mode: MarkdownMode,
    /// `source=true` disables live preview inside editing mode. It is retained
    /// even while the separate reading mode is selected.
    pub source: bool,
    pub backlinks: bool,
    #[serde(default)]
    pub requested_reading_range: Option<crate::subpaths::SubpathLocation>,
    pub(crate) data: String,
    pub(crate) last_saved_data: String,
    /// Properties synchronize on the internal-data callback; an explicit
    /// source save can advance data without advancing this retained owner.
    #[serde(default)]
    pub(crate) template_properties_data: Option<String>,
    pub(crate) clean_editor_revision: u64,
    /// Renderer timers are process state, not workspace/private restart state.
    #[serde(skip)]
    pub(crate) autosave_due_ms: Option<u64>,
    #[serde(skip)]
    pub(crate) autosave_realtime_due: Option<Instant>,
    #[serde(skip)]
    pub(crate) internal_data_due_ms: Option<u64>,
    #[serde(skip)]
    pub(crate) internal_data_realtime_due: Option<Instant>,
    #[serde(skip)]
    pub(crate) scheduled_editor_revision: u64,
}
#[derive(Debug)]
pub(crate) struct PendingTemplateSave {
    id: String,
    path: String,
    data: String,
    previous_saved: String,
    save_again: bool,
}
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64() != Some(0.0),
        Value::String(value) => !value.is_empty(),
        _ => true,
    }
}
fn property_name(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        Value::Array(values) => values
            .iter()
            .map(|value| {
                if value.is_null() {
                    String::new()
                } else {
                    property_name(value)
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Null => "null".into(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::Object(_) => "[object Object]".into(),
    }
}
fn mode_value(value: &Value) -> Option<MarkdownMode> {
    if !truthy(value) {
        return None;
    }
    match property_name(value).as_str() {
        "source" => Some(MarkdownMode::Source),
        "preview" => Some(MarkdownMode::Preview),
        _ => None,
    }
}
impl MarkdownViewState {
    pub(crate) fn new(mode: MarkdownMode, source: bool, saved: &str) -> Self {
        let data = saved.strip_prefix('\u{feff}').unwrap_or(saved).to_owned();
        Self {
            mode,
            source,
            backlinks: false,
            requested_reading_range: None,
            last_saved_data: data.clone(),
            template_properties_data: Some(data.clone()),
            data,
            clean_editor_revision: 0,
            autosave_due_ms: None,
            autosave_realtime_due: None,
            internal_data_due_ms: None,
            internal_data_realtime_due: None,
            scheduled_editor_revision: 0,
        }
    }
    pub(crate) fn load(&mut self, saved: &str) {
        self.data = saved.strip_prefix('\u{feff}').unwrap_or(saved).into();
        self.last_saved_data = self.data.clone();
        self.template_properties_data = Some(self.data.clone());
        self.clean_editor_revision = 0;
        self.scheduled_editor_revision = 0;
        self.requested_reading_range = None;
    }
    pub fn last_saved_data(&self) -> &str {
        &self.last_saved_data
    }
    pub fn reading_data(&self) -> &str {
        &self.data
    }
    pub fn clean_revision(&self) -> u64 {
        self.clean_editor_revision
    }
}
impl Engine {
    pub fn markdown_state(&self) -> Option<&MarkdownViewState> {
        let id = self.session.active_leaf.as_deref()?;
        self.session
            .leaves
            .iter()
            .find(|leaf| leaf.id == id)?
            .markdown
            .as_ref()
    }
    pub fn markdown_leaf_state(&self, id: &str) -> Option<&MarkdownViewState> {
        self.session
            .leaves
            .iter()
            .find(|leaf| leaf.id == id)?
            .markdown
            .as_ref()
    }
    pub fn markdown_view_data(&self) -> Option<&str> {
        let state = self.markdown_state()?;
        match state.mode {
            MarkdownMode::Source => Some(&self.editor()?.text),
            MarkdownMode::Preview => Some(&state.data),
        }
    }
    pub fn markdown_is_dirty(&self) -> Option<bool> {
        let state = self.markdown_state()?;
        Some(self.editor()?.edit_revision != state.clean_editor_revision)
    }
    pub(crate) fn new_markdown_state(&self, saved: &str) -> MarkdownViewState {
        let mode = mode_value(&self.get_config("defaultViewMode")).unwrap_or(MarkdownMode::Source);
        MarkdownViewState::new(mode, !truthy(&self.get_config("livePreview")), saved)
    }
    /// Apply the observed nonvisual L8 mode/source/backlinks state subset.
    /// Missing/invalid mode values and non-boolean source flags retain state.
    /// File selection and the outer leaf's active/pinned/group fields are owned
    /// by their separate workspace operations.
    pub fn set_markdown_state(&mut self, value: &Value) -> Result<bool, EngineError> {
        let Some(old) = self.markdown_state() else {
            return Ok(false);
        };
        let old_mode = old.mode;
        let mode = value.get("mode").and_then(mode_value).unwrap_or(old_mode);
        let mut changed = mode != old_mode;
        if changed {
            if old_mode == MarkdownMode::Source {
                self.save_active()?;
            }
            let data = self.markdown_state().unwrap().data.clone();
            if mode == MarkdownMode::Source {
                let buffer = self.editor_mut().ok_or(EngineError::NoEditor)?;
                editor_merge::load_contents(buffer, &normalize_editor_newlines(&data))?;
            }
            let id = self.session.active_leaf.clone().unwrap();
            let state = self
                .session
                .leaves
                .iter_mut()
                .find(|leaf| leaf.id == id)
                .unwrap()
                .markdown
                .as_mut()
                .unwrap();
            state.mode = mode;
        }
        let id = self.session.active_leaf.clone().unwrap();
        let state = self
            .session
            .leaves
            .iter_mut()
            .find(|leaf| leaf.id == id)
            .unwrap()
            .markdown
            .as_mut()
            .unwrap();
        if let Some(source) = value.get("source").and_then(Value::as_bool) {
            changed |= state.source != source;
            state.source = source;
        }
        if let Some(backlinks) = value.get("backlinks").and_then(Value::as_bool) {
            changed |= state.backlinks != backlinks;
            state.backlinks = backlinks;
        }
        Ok(changed)
    }
    /// Register an editor doc-change event after mutating EditorBuffer. The
    /// caller supplies a monotonic millisecond clock; source Gl debounces save
    /// by 2000 ms from the first pending event. Gl's default third argument is
    /// false: later edits update the data but keep the scheduled deadline.
    /// Explicit saves/mode switches do not cancel that pending call.
    pub fn mark_editor_changed_at(&mut self, now_ms: u64) -> Result<(), EngineError> {
        let revision = self.editor().ok_or(EngineError::NoEditor)?.edit_revision;
        let id = self
            .session
            .active_leaf
            .as_deref()
            .ok_or(EngineError::NoActiveFile)?;
        let state = self
            .session
            .leaves
            .iter_mut()
            .find(|leaf| leaf.id == id)
            .and_then(|leaf| leaf.markdown.as_mut())
            .ok_or(EngineError::NoEditor)?;
        state
            .autosave_due_ms
            .get_or_insert(now_ms.saturating_add(2000));
        state.autosave_realtime_due = None;
        state
            .internal_data_due_ms
            .get_or_insert(now_ms.saturating_add(10));
        state.internal_data_realtime_due = None;
        state.scheduled_editor_revision = revision;
        Ok(())
    }
    pub fn set_editor_value_at(&mut self, text: String, now_ms: u64) -> Result<(), EngineError> {
        self.editor_mut()
            .ok_or(EngineError::NoEditor)?
            .set_value(text);
        self.mark_editor_changed_at(now_ms)
    }
    /// Consume callbacks scheduled using an explicitly supplied logical clock.
    /// The persistent server separately drives default editor events using
    /// poll_editor_autosave and the real monotonic process clock.
    pub fn process_markdown_timers(&mut self, now_ms: u64) -> Result<Vec<String>, EngineError> {
        self.drain_template_saves()?;
        let internal = self
            .session
            .leaves
            .iter_mut()
            .filter_map(|leaf| {
                let state = leaf.markdown.as_mut()?;
                state
                    .internal_data_due_ms
                    .filter(|deadline| *deadline <= now_ms)?;
                state.internal_data_due_ms = None;
                Some(leaf.id.clone())
            })
            .collect::<Vec<_>>();
        for id in internal {
            self.process_internal_markdown_data(&id)?;
        }
        let due = self
            .session
            .leaves
            .iter_mut()
            .filter_map(|leaf| {
                let state = leaf.markdown.as_mut()?;
                state
                    .autosave_due_ms
                    .filter(|deadline| *deadline <= now_ms)?;
                Some(leaf.id.clone())
            })
            .collect::<Vec<_>>();
        for id in &due {
            self.session
                .leaves
                .iter_mut()
                .find(|leaf| leaf.id == *id)
                .unwrap()
                .markdown
                .as_mut()
                .unwrap()
                .autosave_due_ms = None;
            self.save_markdown_leaf(id)?;
        }
        Ok(due)
    }
    /// Drive default editor events on the real monotonic process clock. This
    /// is used by the persistent server, including periods with no clients.
    /// Logical *_at timers remain in their explicitly supplied clock domain.
    pub fn poll_editor_autosave(&mut self) -> Vec<(String, Result<(), EngineError>)> {
        self.poll_editor_autosave_at(Instant::now())
    }
    pub(crate) fn poll_editor_autosave_at(
        &mut self,
        now: Instant,
    ) -> Vec<(String, Result<(), EngineError>)> {
        if let Err(error) = self.drain_template_saves() {
            self.config_diagnostics
                .push(format!("Markdown template save: {error}"));
        }
        for leaf in &mut self.session.leaves {
            let Some(state) = &mut leaf.markdown else {
                continue;
            };
            let Some(buffer) = self.session.leaf_buffers.get(&leaf.id) else {
                continue;
            };
            if buffer.edit_revision != state.scheduled_editor_revision {
                state.scheduled_editor_revision = buffer.edit_revision;
                if let Some(at) = buffer.last_edit_at {
                    state.autosave_due_ms = None;
                    if state.autosave_realtime_due.is_none() {
                        state.autosave_realtime_due = at.checked_add(Duration::from_secs(2));
                    }
                    state.internal_data_due_ms = None;
                    if state.internal_data_realtime_due.is_none() {
                        state.internal_data_realtime_due =
                            at.checked_add(Duration::from_millis(10));
                    }
                }
            }
        }
        let internal = self
            .session
            .leaves
            .iter_mut()
            .filter_map(|leaf| {
                let state = leaf.markdown.as_mut()?;
                state
                    .internal_data_realtime_due
                    .filter(|deadline| *deadline <= now)?;
                state.internal_data_realtime_due = None;
                Some(leaf.id.clone())
            })
            .collect::<Vec<_>>();
        for id in internal {
            if let Err(error) = self.process_internal_markdown_data(&id) {
                self.config_diagnostics
                    .push(format!("Markdown quick preview: {error}"));
            }
        }
        let due = self
            .session
            .leaves
            .iter_mut()
            .filter_map(|leaf| {
                let state = leaf.markdown.as_mut()?;
                state
                    .autosave_realtime_due
                    .filter(|deadline| *deadline <= now)?;
                state.autosave_realtime_due = None;
                Some(leaf.id.clone())
            })
            .collect::<Vec<_>>();
        due.into_iter()
            .map(|id| {
                let outcome = self.save_markdown_leaf(&id);
                if let Err(error) = &outcome {
                    self.config_diagnostics
                        .push(format!("Markdown autosave: {error}"));
                }
                (id, outcome)
            })
            .collect()
    }
    /// P8's 10ms debounced internal-data callback publishes unsaved data to
    /// same-file views. This is a workspace quick-preview notification, not a
    /// saved-file modification. Programmatic peer `set` transactions preserve
    /// each selection and do not schedule another user edit/save callback.
    pub(crate) fn publish_template_markdown_data(&mut self, id: &str) -> Result<(), EngineError> {
        // L8.setViewData loads frontmatter independently of the ordinary
        // internal-data callback's equality guard.
        self.process_internal_markdown_data_inner(id, true)
    }
    fn process_internal_markdown_data(&mut self, id: &str) -> Result<(), EngineError> {
        self.process_internal_markdown_data_inner(id, false)
    }
    fn process_internal_markdown_data_inner(
        &mut self,
        id: &str,
        load_frontmatter: bool,
    ) -> Result<(), EngineError> {
        let Some(leaf) = self.session.leaves.iter().find(|leaf| leaf.id == id) else {
            return Ok(());
        };
        let Some(path) = leaf.file.clone() else {
            return Ok(());
        };
        let Some(state) = &leaf.markdown else {
            return Ok(());
        };
        let data = if state.mode == MarkdownMode::Source {
            let Some(buffer) = self.editor_for_leaf(id) else {
                return Ok(());
            };
            buffer.text.clone()
        } else {
            state.data.clone()
        };
        if !load_frontmatter && data == state.data {
            return Ok(());
        }
        for leaf in &mut self.session.leaves {
            if leaf.file.as_ref() != Some(&path) {
                continue;
            }
            let Some(state) = &mut leaf.markdown else {
                continue;
            };
            state.template_properties_data = Some(data.clone());
            if state.data == data {
                continue;
            }
            state.data = data.clone();
            if leaf.id != id && state.mode == MarkdownMode::Source {
                if let Some(buffer) = self.session.leaf_buffers.get_mut(&leaf.id) {
                    editor_merge::load_contents(buffer, &normalize_editor_newlines(&data))?;
                }
            }
        }
        Ok(())
    }
    pub(crate) fn save_markdown_leaf(&mut self, id: &str) -> Result<(), EngineError> {
        self.drain_template_saves()?;
        if let Some(save) = self.prepare_markdown_save(id)? {
            self.complete_markdown_save(save)?;
        }
        Ok(())
    }
    pub(crate) fn queue_template_save(&mut self, id: &str) -> Result<(), EngineError> {
        let path = self
            .session
            .leaves
            .iter()
            .find(|leaf| leaf.id == id)
            .and_then(|leaf| leaf.file.as_deref());
        if let Some(save) = self
            .pending_template_saves
            .iter_mut()
            .find(|save| save.id == id && Some(save.path.as_str()) == path)
        {
            save.save_again = true;
            if let Some(state) = self
                .session
                .leaves
                .iter_mut()
                .find(|leaf| leaf.id == id)
                .and_then(|leaf| leaf.markdown.as_mut())
            {
                state.clean_editor_revision = self
                    .session
                    .leaf_buffers
                    .get(id)
                    .map_or(0, |buffer| buffer.edit_revision);
            }
            return Ok(());
        }
        if let Some(save) = self.prepare_markdown_save(id)? {
            self.pending_template_saves.push_back(save);
        }
        Ok(())
    }
    pub(crate) fn drain_template_saves(&mut self) -> Result<(), EngineError> {
        while let Some(save) = self.pending_template_saves.pop_front() {
            let again = save.save_again;
            let id = save.id.clone();
            let path = save.path.clone();
            let completion = self.complete_markdown_save(save);
            if again
                && self
                    .session
                    .leaves
                    .iter()
                    .any(|leaf| leaf.id == id && leaf.file.as_deref() == Some(path.as_str()))
            {
                if let Some(save) = self.prepare_markdown_save(&id)? {
                    self.pending_template_saves.push_back(save);
                }
            }
            completion?;
        }
        Ok(())
    }
    fn prepare_markdown_save(
        &mut self,
        id: &str,
    ) -> Result<Option<PendingTemplateSave>, EngineError> {
        let leaf = self
            .session
            .leaves
            .iter()
            .find(|leaf| leaf.id == id)
            .ok_or(EngineError::NoActiveFile)?;
        let path = leaf
            .file
            .as_deref()
            .ok_or(EngineError::NoActiveFile)?
            .to_owned();
        let state = leaf.markdown.as_ref().ok_or(EngineError::NoEditor)?;
        let buffer = self
            .session
            .leaf_buffers
            .get(id)
            .ok_or(EngineError::NoEditor)?;
        let data = if state.mode == MarkdownMode::Preview {
            state.data.clone()
        } else {
            buffer.text.clone()
        };
        let unchanged = state.last_saved_data == data;
        let previous_saved = state.last_saved_data.clone();
        let revision = buffer.edit_revision;
        // Q4.save resets dirty before attempting the adapter write, including
        // failures. The old lastSavedData survives a failed save separately.
        self.session
            .leaves
            .iter_mut()
            .find(|leaf| leaf.id == id)
            .unwrap()
            .markdown
            .as_mut()
            .unwrap()
            .clean_editor_revision = revision;
        if unchanged {
            return Ok(None);
        }
        // Q4 commits data/lastSavedData in the synchronous save prefix. On IO
        // failure it restores only lastSavedData, keeping the new view data.
        let state = self
            .session
            .leaves
            .iter_mut()
            .find(|leaf| leaf.id == id)
            .unwrap()
            .markdown
            .as_mut()
            .unwrap();
        state.data = data.clone();
        state.last_saved_data = data.clone();
        Ok(Some(PendingTemplateSave {
            id: id.to_owned(),
            path,
            data,
            previous_saved,
            save_again: false,
        }))
    }
    pub(crate) fn template_saving_leaves(&self, path: &str) -> Vec<String> {
        let mut leaves = self
            .pending_template_saves
            .iter()
            .filter(|save| save.path == path)
            .map(|save| save.id.clone())
            .collect::<Vec<_>>();
        if let Some((id, owner_path)) = &self.template_save_inflight {
            if owner_path == path {
                leaves.push(id.clone());
            }
        }
        leaves
    }
    fn complete_markdown_save(&mut self, save: PendingTemplateSave) -> Result<(), EngineError> {
        let PendingTemplateSave {
            id,
            path,
            data,
            previous_saved,
            ..
        } = save;
        self.template_save_inflight = Some((id.clone(), path.clone()));
        let write = if crate::external::real_path(&path).is_some() {
            self.modify_external_saved(&path, &data)
        } else {
            self.vault.modify(&path, &data).map_err(EngineError::from)
        };
        if let Err(error) = write {
            self.session
                .leaves
                .iter_mut()
                .find(|leaf| leaf.id == id && leaf.file.as_deref() == Some(path.as_str()))
                .and_then(|leaf| leaf.markdown.as_mut())
                .filter(|state| state.last_saved_data == data)
                .map(|state| state.last_saved_data = previous_saved);
            self.template_save_inflight = None;
            return Err(error);
        }
        if self
            .session
            .leaves
            .iter()
            .any(|leaf| leaf.id == id && leaf.file.as_deref() == Some(path.as_str()))
        {
            if let Some(buffer) = self.session.leaf_buffers.get_mut(&id) {
                buffer.saved_text = normalize_editor_newlines(&data);
            }
        }
        if crate::external::real_path(&path).is_none() {
            self.file_modified(&path);
        }
        self.template_save_inflight = None;
        Ok(())
    }
    /// Save nonvisual Markdown state before a normal application shutdown.
    /// Dirty Source editors follow the native focus-loss saveImmediately hook,
    /// including quit before the 10ms internal-data callback. The subsequent
    /// quit collector compares retained view data with lastSavedData. A hidden
    /// Preview editor's dirty text alone never becomes saved reading data.
    pub fn save_changed_markdown_views(&mut self) -> Result<Vec<String>, EngineError> {
        self.drain_template_saves()?;
        let ids = self
            .session
            .leaves
            .iter()
            .filter(|leaf| leaf.markdown.is_some())
            .map(|leaf| leaf.id.clone())
            .collect::<Vec<_>>();
        let mut saved = Vec::new();
        for id in ids {
            let Some(state) = self.markdown_leaf_state(&id) else {
                continue;
            };
            let dirty_source = state.mode == MarkdownMode::Source
                && self
                    .editor_for_leaf(&id)
                    .is_some_and(|buffer| buffer.edit_revision != state.clean_editor_revision);
            // Use Q4.data here, not raw Source editor text: a clean CRLF note
            // has a normalized LF editor and must not be rewritten on shutdown.
            if dirty_source || state.data != state.last_saved_data {
                self.save_markdown_leaf(&id)?;
                saved.push(id);
            }
        }
        Ok(saved)
    }
    pub(crate) fn mark_markdown_saved(&mut self, path: &str, source: &str) {
        for leaf in &mut self.session.leaves {
            if leaf.file.as_deref() == Some(path) {
                if let Some(state) = &mut leaf.markdown {
                    state.last_saved_data = source.into();
                    state.data = source.into();
                    state.clean_editor_revision = self
                        .session
                        .leaf_buffers
                        .get(&leaf.id)
                        .map_or(0, |buffer| buffer.edit_revision);
                }
            }
        }
    }
    pub(crate) fn markdown_preview_saved_modified(&mut self, path: &str, source: &str) -> bool {
        let source = source.strip_prefix('\u{feff}').unwrap_or(source);
        let mut has_preview = false;
        let mut has_source = false;
        let saving = self.template_saving_leaves(path);
        for leaf in &mut self.session.leaves {
            if leaf.file.as_deref() == Some(path) && !saving.contains(&leaf.id) {
                if let Some(state) = &mut leaf.markdown {
                    state.last_saved_data = source.into();
                    if state.mode == MarkdownMode::Preview {
                        state.data = source.into();
                        state.template_properties_data = Some(source.into());
                        if let Some(buffer) = self.session.leaf_buffers.get_mut(&leaf.id) {
                            buffer.saved_text = normalize_editor_newlines(source);
                        }
                        has_preview = true;
                    } else {
                        has_source = true;
                    }
                }
            }
        }
        has_preview && !has_source
    }
}

#[cfg(test)]
mod autosave_tests {
    use super::*;
    use crate::{EditorPosition, EditorSelection, FileTarget, OpenOptions};
    use std::{
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "mnemo-debounce-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&root).unwrap();
            for name in ["A.md", "B.md"] {
                std::fs::write(root.join(name), "saved\n").unwrap();
            }
            Self(root)
        }
        fn engine(&self) -> Engine {
            let mut engine = Engine::open(&self.0).unwrap();
            engine
                .open_file(FileTarget::path("A.md"), OpenOptions::default())
                .unwrap();
            engine
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn changed_at(engine: &mut Engine, text: &str, at: Instant) {
        let editor = engine.editor_mut().unwrap();
        editor.set_value(text.into());
        editor.last_edit_at = Some(at);
    }
    #[test]
    fn document_events_keep_first_deadline_and_selection_does_not_schedule_another_save() {
        let fixture = Fixture::new();
        let mut engine = fixture.engine();
        let start = Instant::now();
        changed_at(&mut engine, "first\n", start);
        assert!(engine.poll_editor_autosave_at(start).is_empty());
        // Authored Gl(default n=false) burst: latest text, first deadline.
        let at = start + Duration::from_secs(1);
        changed_at(&mut engine, "second\n", at);
        engine.poll_editor_autosave_at(at);
        let position = EditorPosition { line: 0, ch: 2 };
        engine
            .editor_mut()
            .unwrap()
            .set_selection(EditorSelection {
                anchor: position,
                head: position,
            })
            .unwrap();
        assert!(engine
            .poll_editor_autosave_at(start + Duration::from_millis(1999))
            .is_empty());
        let outcomes = engine.poll_editor_autosave_at(start + Duration::from_secs(2));
        assert_eq!(outcomes.len(), 1);
        assert!(outcomes[0].1.is_ok());
        assert_eq!(
            std::fs::read_to_string(fixture.0.join("A.md")).unwrap(),
            "second\n"
        );
        assert!(!engine.markdown_is_dirty().unwrap());
        assert!(engine
            .poll_editor_autosave_at(start + Duration::from_secs(10))
            .is_empty());
    }
    #[test]
    fn replacement_api_schedules_and_same_value_set_still_marks_dirty() {
        let fixture = Fixture::new();
        let mut engine = fixture.engine();
        let start = Instant::now();
        engine
            .editor_mut()
            .unwrap()
            .replace_selection("prefix ")
            .unwrap();
        engine.editor_mut().unwrap().last_edit_at = Some(start);
        assert_eq!(
            engine
                .poll_editor_autosave_at(start + Duration::from_secs(2))
                .len(),
            1
        );
        assert_eq!(
            std::fs::read_to_string(fixture.0.join("A.md")).unwrap(),
            "prefix saved\n"
        );
        changed_at(
            &mut engine,
            "prefix saved\n",
            start + Duration::from_secs(3),
        );
        assert!(engine.markdown_is_dirty().unwrap());
        engine.poll_editor_autosave_at(start + Duration::from_secs(5));
        assert!(!engine.markdown_is_dirty().unwrap());
    }
    #[test]
    fn explicit_logical_events_do_not_get_rescheduled_on_realtime_clock() {
        let fixture = Fixture::new();
        let mut engine = fixture.engine();
        engine.set_editor_value_at("logical\n".into(), 10).unwrap();
        assert!(engine
            .poll_editor_autosave_at(Instant::now() + Duration::from_secs(10))
            .is_empty());
        assert_eq!(
            std::fs::read_to_string(fixture.0.join("A.md")).unwrap(),
            "saved\n"
        );
        assert_eq!(engine.process_markdown_timers(2010).unwrap().len(), 1);
    }
    #[test]
    fn one_save_failure_does_not_discard_another_views_callback() {
        let fixture = Fixture::new();
        let mut engine = fixture.engine();
        let start = Instant::now();
        changed_at(&mut engine, "fail\n", start);
        engine
            .open_file(FileTarget::path("B.md"), OpenOptions { new_tab: true })
            .unwrap();
        changed_at(&mut engine, "success\n", start);
        std::fs::remove_file(fixture.0.join("A.md")).unwrap();
        std::fs::create_dir(fixture.0.join("A.md")).unwrap();
        let outcomes = engine.poll_editor_autosave_at(start + Duration::from_secs(2));
        assert_eq!(outcomes.len(), 2);
        assert_eq!(outcomes.iter().filter(|(_, r)| r.is_err()).count(), 1);
        assert_eq!(
            std::fs::read_to_string(fixture.0.join("B.md")).unwrap(),
            "success\n"
        );
        assert!(engine
            .config_diagnostics
            .iter()
            .any(|s| s.starts_with("Markdown autosave:")));
        assert!(engine
            .poll_editor_autosave_at(start + Duration::from_secs(3))
            .is_empty());
    }
    #[test]
    fn restart_does_not_restore_a_process_timer_from_private_state() {
        let fixture = Fixture::new();
        let mut engine = fixture.engine();
        changed_at(&mut engine, "pending\n", Instant::now());
        engine.poll_editor_autosave();
        engine.persist_session().unwrap();
        let mut reopened = Engine::open(&fixture.0).unwrap();
        assert!(reopened
            .poll_editor_autosave_at(Instant::now() + Duration::from_secs(10))
            .is_empty());
        assert_eq!(
            std::fs::read_to_string(fixture.0.join("A.md")).unwrap(),
            "saved\n"
        );
    }
}
