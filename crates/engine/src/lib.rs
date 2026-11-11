//! Headless application state shared by CLI and future UI clients.
//!
//! The private `.mnemosyne/session.json` is an implementation persistence format,
//! not the desktop workspace serialization format. Desktop workspace
//! import retains observed main/floating pane topology, file leaves, active
//! identity and recents. Per-leaf editor selection is ephemeral in native
//! workspace data; the private session format also retains it. Full plugin
//! workspace serialization and native shutdown timing need separate evidence.
use mnemosyne_core::CONFIG_DIR;
pub mod bookmarks;
mod chronology;
pub mod command_args;
pub mod command_registry;
pub mod core_plugins;
pub mod daily;
mod editor_merge;
pub mod events;
pub mod external;
pub mod external_watch;
pub mod file_operations;
pub mod host;
pub mod interactive;
pub mod json_units;
pub mod language;
pub mod live_metadata_types;
pub mod markdown;
pub mod metadata_types;
mod metadata_store;
mod metadata_lifecycle;
pub mod moment;
pub mod observation;
pub mod panes;
pub mod raw_links;
#[cfg(unix)]
pub mod script_application;
#[cfg(unix)]
pub mod script_widget_realm;
#[cfg(unix)]
pub mod script_metadata_types;
pub mod script_file_classes;
pub mod script_files;
pub mod script_methods;
#[cfg(unix)]
pub mod server;
pub mod settings;
pub mod subpaths;
pub mod templates;
pub mod vault_registry;

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

static NEXT_SAVE: AtomicU64 = AtomicU64::new(0);

use mnemosyne_core::{cache::CachedMetadata, ignore::IgnoreFilters, Note, Vault, VaultError};
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("{0}")]
    Vault(#[from] VaultError),
    #[error("{0}")]
    CorePlugin(#[from] core_plugins::CorePluginConfigError),
    #[error("{0}")]
    Collation(#[from] mnemosyne_core::collation::CollationError),
    #[error("{0}")]
    UnicodeCase(#[from] mnemosyne_core::collation::UnicodeCaseError),
    #[error("{0}")]
    MetadataType(#[from] metadata_types::MetadataTypeError),
    #[error("{0}")]
    Daily(#[from] daily::DailyError),
    #[error("{0}")]
    Template(#[from] templates::TemplateError),
    #[error("File \"{0}\" not found.")]
    FileNotFound(String),
    #[error("\"{0}\" is a folder, not a file.")]
    Folder(String),
    #[error("No active file. Use file=<name> or path=<path> to specify a file.")]
    NoActiveFile,
    #[error("Missing required parameter: file or path")]
    MissingTarget,
    #[error("No active Markdown editor.")]
    NoEditor,
    #[error("Invalid editor position: line {0}, UTF-16 column {1}")]
    InvalidPosition(i64, i64),
    #[error("Task editor status must be one UTF-16 code unit.")]
    InvalidTaskEditorStatus,
    #[error("Task editor marker at line {0}, column {1} is unavailable.")]
    MissingTaskEditorMarker(i64, i64),
    #[error("Editor merge compatibility remains pending: {0}")]
    PendingEditorMerge(String),
    #[error("Invalid saved source edit: {0}")]
    InvalidSourceEdit(String),
    #[error("Invalid session state: {0}")]
    InvalidState(String),
    #[error("{0}: {1}")]
    Io(String, #[source] std::io::Error),
    #[error("{0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Copy, Default)]
pub struct FileTarget<'a> {
    pub path: Option<&'a str>,
    pub file: Option<&'a str>,
}

impl<'a> FileTarget<'a> {
    pub fn path(path: &'a str) -> Self {
        Self {
            path: Some(path),
            file: None,
        }
    }
    pub fn file(file: &'a str) -> Self {
        Self {
            path: None,
            file: Some(file),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct OpenOptions {
    pub new_tab: bool,
}

/// CodeMirror editor positions count UTF-16 code units, not UTF-8 bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditorPosition {
    pub line: i64,
    pub ch: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditorSelection {
    pub anchor: EditorPosition,
    pub head: EditorPosition,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditorBuffer {
    pub text: String,
    pub saved_text: String,
    pub selection: EditorSelection,
    #[serde(default)]
    pub edit_revision: u64,
    /// Process-local user-edit timestamp. File loads/merges do not create an
    /// editor doc-change event, and persistence never restores a pending timer.
    #[serde(skip)]
    pub(crate) last_edit_at: Option<std::time::Instant>,
}

impl EditorBuffer {
    pub fn new(text: String) -> Self {
        let text = editor_saved_text(&text);
        Self {
            saved_text: text.clone(),
            text,
            selection: EditorSelection::default(),
            edit_revision: 0,
            last_edit_at: None,
        }
    }
    pub fn is_dirty(&self) -> bool {
        self.text != self.saved_text
    }
    pub fn set_value(&mut self, text: String) {
        self.edit_revision = self.edit_revision.wrapping_add(1);
        self.text = normalize_editor_newlines(&text);
        self.selection = EditorSelection::default();
        self.last_edit_at = Some(std::time::Instant::now());
    }
    pub fn set_selection(&mut self, selection: EditorSelection) -> Result<(), EngineError> {
        let anchor = self.position(self.offset(selection.anchor)?);
        let head = self.position(self.offset(selection.head)?);
        self.selection = EditorSelection { anchor, head };
        Ok(())
    }
    pub fn selected_text(&self) -> Result<&str, EngineError> {
        let a = self.offset(self.selection.anchor)?;
        let b = self.offset(self.selection.head)?;
        Ok(&self.text[a.min(b)..a.max(b)])
    }
    /// Replace the current selection and collapse the cursor at insertion end.
    pub fn replace_selection(&mut self, replacement: &str) -> Result<(), EngineError> {
        let replacement = normalize_editor_newlines(replacement);
        let a = self.offset(self.selection.anchor)?;
        let b = self.offset(self.selection.head)?;
        let start = a.min(b);
        self.text.replace_range(start..a.max(b), &replacement);
        self.edit_revision = self.edit_revision.wrapping_add(1);
        self.last_edit_at = Some(std::time::Instant::now());
        let prefix = &self.text[..start + replacement.len()];
        let line = prefix.bytes().filter(|b| *b == b'\n').count();
        let ch = prefix
            .rsplit('\n')
            .next()
            .unwrap_or("")
            .encode_utf16()
            .count();
        let position = EditorPosition {
            line: line as i64,
            ch: ch as i64,
        };
        self.selection = EditorSelection {
            anchor: position,
            head: position,
        };
        Ok(())
    }
    fn position(&self, offset: usize) -> EditorPosition {
        let prefix = &self.text[..offset];
        EditorPosition {
            line: prefix.bytes().filter(|b| *b == b'\n').count() as i64,
            ch: prefix
                .rsplit('\n')
                .next()
                .unwrap_or("")
                .encode_utf16()
                .count() as i64,
        }
    }
    fn offset(&self, position: EditorPosition) -> Result<usize, EngineError> {
        if position.line < 0 {
            return Ok(0);
        }
        let line_index = position.line as usize;
        let Some(line) = self.text.split('\n').nth(line_index) else {
            return Ok(self.text.len());
        };
        let start: usize = self
            .text
            .split('\n')
            .take(line_index)
            .map(|line| line.len() + 1)
            .sum();
        let desired = position.ch.max(0) as usize;
        let mut utf16 = 0;
        for (index, ch) in line.char_indices() {
            if utf16 == desired {
                return Ok(start + index);
            }
            if desired < utf16 + ch.len_utf16() {
                // JavaScript can represent a cursor between a surrogate pair.
                // WTF-16 interoperability belongs to the pending plugin runtime.
                return Err(EngineError::InvalidPosition(position.line, position.ch));
            }
            utf16 += ch.len_utf16();
        }
        Ok(start + line.len())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeafState {
    pub id: String,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub active_order: u64,
    pub file: Option<String>,
    pub view_type: String,
    /// Internal navigation history; reference history serialization is pending.
    #[serde(default)]
    pub back_history: Vec<String>,
    #[serde(default)]
    pub markdown: Option<markdown::MarkdownViewState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionState {
    pub schema_version: u32,
    /// Internal active date locale; importing the desktop global language is not supported.
    #[serde(default = "default_locale")]
    pub date_locale: String,
    #[serde(default = "default_locale")]
    pub ui_language: String,
    #[serde(default)]
    pub language_storage: Option<String>,
    #[serde(default = "default_collation_locale")]
    pub navigator_language: String,
    #[serde(default = "default_collation_locale")]
    pub collation_locale: String,
    /// Internal live cache-source chronology, not reference cache serialization.
    #[serde(default)]
    pub metadata_file_order: Vec<String>,
    pub active_leaf: Option<String>,
    pub leaves: Vec<LeafState>,
    /// Legacy file-keyed projection, used only to migrate earlier private sessions.
    pub buffers: BTreeMap<String, EditorBuffer>,
    /// Each Markdown leaf owns independent editor text and selection.
    #[serde(default)]
    pub leaf_buffers: BTreeMap<String, EditorBuffer>,
    #[serde(default)]
    pub pane_layout: Option<panes::WorkspaceLayout>,
    #[serde(default)]
    pub next_activation: u64,
    /// Full recent tracker storage. `recents()` exposes at most ten entries.
    pub last_open_files: Vec<String>,
    pub next_leaf_id: u64,
    /// Original reference workspace retained for a future complete decoder.
    #[serde(default)]
    pub imported_workspace: Option<serde_json::Value>,
}

fn default_collation_locale() -> String {
    "en-US".into()
}

fn default_locale() -> String {
    "en".into()
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            schema_version: 1,
            date_locale: default_locale(),
            ui_language: default_locale(),
            language_storage: None,
            navigator_language: default_collation_locale(),
            collation_locale: default_collation_locale(),
            metadata_file_order: Vec::new(),
            active_leaf: Some("mnemosyne-1".into()),
            leaves: vec![LeafState {
                id: "mnemosyne-1".into(),
                pinned: false,
                active_order: 0,
                file: None,
                view_type: "empty".into(),
                back_history: Vec::new(),
                markdown: None,
            }],
            buffers: BTreeMap::new(),
            leaf_buffers: BTreeMap::new(),
            pane_layout: None,
            next_activation: 1,
            last_open_files: Vec::new(),
            next_leaf_id: 2,
            imported_workspace: None,
        }
    }
}

#[derive(Debug, Clone)]
struct CachedNote {
    // Exact snapshots detect replacements through the current public Vault API.
    // Sharing source ownership with core is pending; hashes/allocation addresses
    // cannot establish exact unchanged bytes on their own.
    source: Arc<str>,
    computed_data: Option<Arc<str>>,
    metadata: Option<Arc<CachedMetadata>>,
    failure: Option<Arc<mnemosyne_core::cache::MetadataWorkerFailure>>,
}
impl CachedNote {
    fn from_note(note: &Note) -> Self {
        let (metadata, failure) = match CachedMetadata::try_from_note(note) {
            Ok(metadata) => (Some(Arc::new(metadata)), None),
            Err(failure) => (None, Some(Arc::new(failure))),
        };
        Self {
            source: Arc::from(note.source.as_str()),
            computed_data: None,
            metadata,
            failure,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ConfigDiskSnapshot {
    app: Option<(Option<std::time::SystemTime>, Vec<u8>)>,
    appearance: Option<(Option<std::time::SystemTime>, Vec<u8>)>,
}
impl ConfigDiskSnapshot {
    fn read(root: &Path) -> Self {
        let read = |name: &str| {
            let path = root.join(CONFIG_DIR).join(format!("{name}.json"));
            std::fs::read(&path).ok().map(|bytes| {
                let modified = std::fs::metadata(&path)
                    .ok()
                    .and_then(|stat| stat.modified().ok());
                (modified, bytes)
            })
        };
        Self {
            app: read("app"),
            appearance: read("appearance"),
        }
    }
}

#[derive(Debug)]
pub struct Engine {
    pub search_regex_realm: mnemosyne_core::vault_search::RegexCacheRealm,
    pub bookmarks: bookmarks::Bookmarks,
    pub vault: Vault,
    pub commands: command_registry::CommandRegistry<String>,
    session: SessionState,
    chronology: chronology::FolderChronology,
    event_journal: events::EventJournal,
    lifecycle_journal: events::LifecycleJournal,
    lifecycle_journal_precedes_metadata: bool,
    metadata_lifecycle: metadata_lifecycle::MetadataLifecycle,
    startup: metadata_lifecycle::StartupState,
    external_watcher: Option<external_watch::ExternalWatcher>,
    cached_notes: BTreeMap<String, CachedNote>,
    metadata_store: metadata_store::MetadataStore,
    // Native existing indexed non-Markdown files have an empty cache object,
    // independently of whether they participate in Markdown graph resolution.
    non_markdown_caches: BTreeMap<String, CachedMetadata>,
    // MetadataCache.onRename saves the supplied raw path. A watcher may then
    // create a canonical VFS entry without deleting that raw cache key. Keys
    // here are metadata identities; disk operations use the separately retained
    // physical destination from the successful adapter rename.
    raw_file_cache: BTreeMap<String, Option<CachedNote>>,
    raw_file_destinations: BTreeMap<String, String>,
    external_files: BTreeMap<String, external::ExternalFile>,
    app_config: serde_json::Map<String, serde_json::Value>,
    app_config_disk_snapshot: Option<ConfigDiskSnapshot>,
    config_defaults: serde_json::Map<String, serde_json::Value>,
    user_ignore_filters: IgnoreFilters,
    user_ignore_fingerprint: String,
    user_ignore_filters_active: bool,
    user_ignore_cache: RefCell<BTreeMap<String, bool>>,
    config_diagnostics: Vec<String>,
    property_types: metadata_types::MetadataTypeManager,
    property_config_snapshot: Option<String>,
    core_plugin_settings: core_plugins::CorePluginSettings,
    core_plugin_state: core_plugins::CorePluginState,
    file_operations: file_operations::FileOperations,
    pending_template_saves: std::collections::VecDeque<markdown::PendingTemplateSave>,
    template_save_inflight: Option<(String, String)>,
}

impl Engine {
    /// Start/reset opt-in operation collection. No startup/refresh history is
    /// synthesized. The returned internal cursor remains monotonic across resets.
    pub fn start_event_journal(&mut self) -> u64 { self.event_journal.start() }
    pub fn stop_event_journal(&mut self) { self.event_journal.stop(); }
    pub fn event_cursor(&self) -> u64 { self.event_journal.cursor() }
    pub fn event_journal_since(&self, cursor: u64) -> Result<&[events::VaultEvent], events::EventJournalError> {
        self.event_journal.since(cursor)
    }
    fn emit_lifecycle_vault_event(
        &mut self,
        name: events::EventName,
        path: &str,
        kind: events::FileKind,
        old_path: Option<&str>,
    ) {
        if self.lifecycle_journal.enabled() {
            let frame = self.metadata_lifecycle_snapshot();
            if let Err(error) = self
                .lifecycle_journal
                .emit_vault(name, path, kind, old_path, frame)
            {
                self.config_diagnostics.push(error.to_string());
            }
        }
    }
    fn emit_vault_event(
        &mut self,
        name: events::EventName,
        path: &str,
        kind: events::FileKind,
        old_path: Option<&str>,
    ) {
        self.emit_lifecycle_vault_event(name, path, kind, old_path);
        if let Err(error) = self.event_journal.emit(name, path, kind, old_path) {
            self.config_diagnostics.push(error.to_string());
        }
    }
    fn emit_before_metadata_vault_event(
        &mut self,
        name: events::EventName,
        path: &str,
        kind: events::FileKind,
        old_path: Option<&str>,
    ) {
        if self.lifecycle_journal_precedes_metadata {
            self.emit_lifecycle_vault_event(name, path, kind, old_path);
        }
    }
    fn emit_after_metadata_vault_event(
        &mut self,
        name: events::EventName,
        path: &str,
        kind: events::FileKind,
        old_path: Option<&str>,
    ) {
        if !self.lifecycle_journal_precedes_metadata {
            self.emit_lifecycle_vault_event(name, path, kind, old_path);
        }
        if let Err(error) = self.event_journal.emit(name, path, kind, old_path) {
            self.config_diagnostics.push(error.to_string());
        }
    }
    fn event_file_kind(&self, path: &str) -> events::FileKind {
        if self.chronology.folders().contains(&path) { events::FileKind::Folder } else { events::FileKind::File }
    }
    /// Retained startup configuration resolution. This does not claim every
    /// enabled plugin's lifecycle/backend has been implemented or initialized.
    pub fn is_core_plugin_enabled(&self, id: &str) -> bool {
        self.core_plugin_state.enabled(id)
    }
    pub fn core_plugin_settings(&self) -> &core_plugins::CorePluginSettings {
        &self.core_plugin_settings
    }
    pub fn core_plugin_state(&self) -> &core_plugins::CorePluginState {
        &self.core_plugin_state
    }
    /// Records the nonvisual file-creator registration subset. Full plugin
    /// hooks/readiness are separate from the retained state model.
    pub fn can_create_file_with_ext(&self, extension: &str) -> bool {
        self.core_plugin_state.can_create_file_with_ext(extension)
    }
    /// Explicit manager.enable-style config reload. True flags enable retained
    /// wrappers; false flags do not disable previously enabled wrappers. Watching
    /// core-plugins.json does not automatically apply external enable flags.
    pub fn reload_core_plugin_settings(&mut self) -> Result<(), EngineError> {
        let document = self.read_config_json("core-plugins");
        let migration = if matches!(document, Some(serde_json::Value::Array(_))) {
            self.read_config_json("core-plugins-migration")
        } else {
            None
        };
        self.core_plugin_settings =
            core_plugins::CorePluginSettings::resolve(document.as_ref(), migration.as_ref())?;
        self.core_plugin_state
            .apply_enable_configuration(&self.core_plugin_settings);
        Ok(())
    }
    /// Canonical completed assigned overrides, separate from inferred properties.
    /// Live SDK widget roots cannot be flattened through this import boundary.
    pub fn canonical_assignments_units(&self) -> Vec<metadata_types::PropertyInfoUnits> {
        self.property_types.assigned_infos_units()
    }
    pub fn inferred_property_entries(&self) -> Vec<live_metadata_types::InferredProperty> {
        self.cached_file_paths().into_iter().filter(|path| !self.is_user_ignored(path))
            .filter_map(|path| self.file_cache(path))
            .flat_map(metadata_types::MetadataTypeManager::inferred_entries).collect()
    }
    pub fn property_infos(&self) -> Vec<metadata_types::PropertyInfo> {
        self.property_types.infos()
    }
    pub fn property_info(&self, name: &str) -> Result<metadata_types::PropertyInfo, EngineError> {
        Ok(self.property_types.info(name)?)
    }
    pub fn property_occurrences(&self, name: &str) -> Result<usize, EngineError> {
        Ok(self.property_types.occurrences(name)?)
    }
    pub fn set_property_type(&mut self, name: &str, widget: &str) -> Result<(), EngineError> {
        self.set_property_type_units(&json_units::units(name), &json_units::units(widget))
    }
    pub fn property_type_load_error(&self) -> Option<&metadata_types::TypeLoadError> {
        self.property_types.last_load_error()
    }
    pub fn property_infos_units(&self) -> Vec<metadata_types::PropertyInfoUnits> {
        self.property_types.infos_units()
    }
    pub fn property_info_units(
        &self,
        name: &[u16],
    ) -> Result<metadata_types::PropertyInfoUnits, EngineError> {
        Ok(self.property_types.info_units(name)?)
    }
    pub fn assigned_property_widget_units(
        &self,
        name: &[u16],
    ) -> Result<Option<&json_units::JsonUnits>, EngineError> {
        Ok(self.property_types.assigned_widget_units(name)?)
    }
    pub fn property_occurrence_entries_units(&self) -> Vec<(Vec<u16>, usize)> {
        self.property_types.occurrence_entries_units()
    }
    pub fn property_occurrences_units(&self, name: &[u16]) -> Result<usize, EngineError> {
        Ok(self.property_types.occurrences_units(name)?)
    }
    pub fn set_property_type_units(
        &mut self,
        name: &[u16],
        widget: &[u16],
    ) -> Result<(), EngineError> {
        self.set_property_type_value_units(name, json_units::JsonUnits::String(widget.to_vec()))
    }
    pub fn set_property_type_value_units(
        &mut self,
        name: &[u16],
        widget: json_units::JsonUnits,
    ) -> Result<(), EngineError> {
        self.property_types.set_type_value_units(name, widget)?;
        self.write_config_units("types", &self.property_types.config_units())?;
        self.property_config_snapshot =
            read_text(&self.vault.root.join(CONFIG_DIR).join("types.json")).ok();
        self.try_rebuild_property_types()?;
        Ok(())
    }
    pub fn reload_property_types(&mut self) {
        let path = self.vault.root.join(CONFIG_DIR).join("types.json");
        let text = std::fs::read(&path).map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
        self.property_config_snapshot = text.as_ref().ok().cloned();
        let config = match text {
            Ok(text) => match json_units::JsonUnits::parse(&text) {
                Ok(config) => Some(config),
                Err(error) => {
                    let message = error.native_message(&text).unwrap_or_else(|| {
                        mnemosyne_core::yaml_value::quote_units(&error.native_message_units(&text))
                    });
                    self.config_diagnostics.push(format!(
                        "failed to read JSON types.json: SyntaxError: {message}"
                    ));
                    None
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                self.config_diagnostics
                    .push(format!("failed to read JSON types.json: {error}"));
                None
            }
        };
        let mut manager = std::mem::take(&mut self.property_types);
        let was_loaded = manager.is_loaded();
        let outcome = manager.load_data_units(
            config.as_ref(),
            self.cached_file_paths()
                .into_iter()
                .filter(|path| !self.is_user_ignored(path))
                .filter_map(|path| self.file_cache(path)),
        );
        self.property_types = manager;
        match outcome {
            Ok(()) if !was_loaded => self.rebuild_property_types(),
            Err(error) => self.config_diagnostics.push(format!(
                "Metadata type load {}: {}",
                error.name, error.message
            )),
            _ => {}
        }
    }
    pub fn try_rebuild_property_types(&mut self) -> Result<(), EngineError> {
        let mut manager = std::mem::take(&mut self.property_types);
        let outcome = manager.rebuild(
            self.cached_file_paths()
                .into_iter()
                .filter(|path| !self.is_user_ignored(path))
                .filter_map(|path| self.file_cache(path)),
        );
        self.property_types = manager;
        Ok(outcome?)
    }
    pub fn rebuild_property_types(&mut self) {
        if let Err(error) = self.try_rebuild_property_types() {
            self.config_diagnostics
                .push(format!("Metadata type rebuild: {error}"));
        }
    }
    /// Deliver a known saved-metadata changed event to the retained type
    /// manager. Cache imports/rebuilds and warm changed events have different
    /// display-name rules. This does not create a watcher or certify delivery.
    pub fn apply_metadata_changed_event(&mut self, path: &str) -> Result<bool, EngineError> {
        if external::real_path(path).is_some()
            || self.is_user_ignored(path)
            || self.file_cache(path).is_none()
        {
            return Ok(false);
        }
        let mut manager = std::mem::take(&mut self.property_types);
        let outcome = manager
            .cache_changed(self.file_cache(path).unwrap())
            .and_then(|()| {
                manager.recount(
                    self.cached_file_paths()
                        .into_iter()
                        .filter(|path| !self.is_user_ignored(path))
                        .filter_map(|path| self.file_cache(path)),
                )
            });
        self.property_types = manager;
        outcome?;
        Ok(true)
    }
    pub fn ui_language(&self) -> &str {
        &self.session.ui_language
    }
    /// Import renderer preference values explicitly, without reading a personal
    /// Electron profile or fabricating an app.json language setting. Reproduces
    /// a fresh renderer's Moment default before applying the selected UI locale.
    pub fn import_language(&mut self, storage: Option<&str>, navigator: &str) -> &str {
        self.session.language_storage = storage.map(str::to_owned);
        self.session.navigator_language = navigator.into();
        self.session.ui_language = language::select(storage, navigator);
        let moment_id = language::moment_id(&self.session.ui_language);
        self.session.date_locale = default_locale();
        self.set_locale(&moment_id);
        self.ui_language()
    }
    pub fn collation_locale(&self) -> &str {
        &self.session.collation_locale
    }
    /// Explicit renderer Intl resolvedOptions().locale import. This is separate
    /// from localStorage UI language and Moment's active formatting locale.
    pub fn import_collation_locale(&mut self, locale: &str) -> Result<(), EngineError> {
        mnemosyne_core::collation::NaturalCollator::new(locale)?;
        self.session.collation_locale = locale.into();
        Ok(())
    }
    pub fn cached_file_paths(&self) -> Vec<&str> {
        self.session
            .metadata_file_order
            .iter()
            .map(String::as_str)
            .filter(|path| self.vault.has_file(path) || self.raw_file_cache.contains_key(*path) || self.metadata_store.file_info(path).is_some())
            .collect()
    }
    pub fn resolved_link_paths(&self) -> Vec<&str> {
        self.cached_file_paths()
            .into_iter()
            .filter(|path| {
                // Empty attachment caches do not create resolved/unresolved
                // source-map entries. Retained raw Markdown aliases retain the
                // prior source contract independently of canonical membership.
                (self.vault.get(path).is_some() || self.raw_file_cache.contains_key(*path))
                    && self.file_cache(path).is_some()
            })
            .collect()
    }
    /// Import observed cache-source order (not Vault traversal order). Missing
    /// live files append; cold preloaded async insertion order remains pending.
    pub fn import_cache_order(&mut self, paths: &[String]) -> Result<(), EngineError> {
        let mut order = Vec::new();
        for path in paths {
            if !safe_relative(path) || !self.vault.has_file(path) {
                return Err(EngineError::InvalidState(format!(
                    "invalid cache path {path:?}"
                )));
            }
            if !order.contains(path) {
                order.push(path.clone());
            }
        }
        self.session.metadata_file_order = order;
        self.reconcile_cache_order();
        self.rebuild_property_types();
        Ok(())
    }
    fn reconcile_cache_order(&mut self) {
        self.session
            .metadata_file_order
            .retain(|path| self.vault.has_file(path) || self.raw_file_cache.contains_key(path) || self.metadata_store.file_info(path).is_some());
        for path in self.vault.files() {
            if !self.session.metadata_file_order.contains(&path) {
                self.session.metadata_file_order.push(path.clone());
            }
        }
    }
    pub fn locale(&self) -> &str {
        &self.session.date_locale
    }

    /// Like Moment.locale(string), an unknown locale retains the active locale.
    /// This is an internal API, not an app.json language preference.
    pub fn set_locale(&mut self, id: &str) -> &str {
        if let Some(locale) = moment::resolve_locale(id) {
            self.session.date_locale = locale.into();
        }
        self.locale()
    }

    /// Restore private persistent state, or import a desktop workspace
    /// fields if this vault has never been opened by Mnemosyne.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, EngineError> {
        let mut engine = Self::open_retained(root)?;
        engine.drain_metadata_startup()?;
        engine.metadata_lifecycle.retained = false;
        Ok(engine)
    }
    /// Normal retained loading returns at layout readiness, without a cache-clean gate.
    pub fn open_retained(root: impl AsRef<Path>) -> Result<Self, EngineError> {
        let mut engine = Self::prepare_startup(root)?;
        engine.start_startup_loading()?;
        while !engine.poll_startup()? {}
        Ok(engine)
    }
    /// Construction is observable before the real inventory and preload run.
    pub fn prepare_startup(root: impl AsRef<Path>) -> Result<Self, EngineError> {
        mnemosyne_core::collation::lowercase_units(&[])?;
        let vault = Vault::empty(root)?;
        let chronology = chronology::FolderChronology::empty();
        let metadata_store = metadata_store::MetadataStore::default();
        let mut engine = Self {
            search_regex_realm: mnemosyne_core::vault_search::RegexCacheRealm::default(),
            bookmarks: bookmarks::Bookmarks::open(&vault.root),
            vault,
            commands: command_registry::CommandRegistry::default(),
            chronology,
            event_journal: events::EventJournal::default(),
            lifecycle_journal: events::LifecycleJournal::default(),
            lifecycle_journal_precedes_metadata: false,
            metadata_lifecycle: metadata_lifecycle::MetadataLifecycle::default(),
            startup: metadata_lifecycle::StartupState::default(),
            external_watcher: None,
            session: SessionState::default(),
            cached_notes: BTreeMap::new(),
            metadata_store,
            non_markdown_caches: BTreeMap::new(),
            raw_file_cache: BTreeMap::new(),
            raw_file_destinations: BTreeMap::new(),
            external_files: BTreeMap::new(),
            app_config: serde_json::Map::new(),
            app_config_disk_snapshot: None,
            config_defaults: settings::defaults().clone(),
            user_ignore_filters: IgnoreFilters::default(),
            user_ignore_fingerprint: String::new(),
            user_ignore_filters_active: false,
            user_ignore_cache: RefCell::new(BTreeMap::new()),
            config_diagnostics: Vec::new(),
            property_types: metadata_types::MetadataTypeManager::default(),
            property_config_snapshot: None,
            core_plugin_settings: core_plugins::CorePluginSettings::default(),
            core_plugin_state: core_plugins::CorePluginState::default(),
            file_operations: file_operations::FileOperations::default(),
            pending_template_saves: std::collections::VecDeque::new(),
            template_save_inflight: None,
        };
        engine.reload_app_config();
        settings::migrate_initial(&mut engine.app_config);
        // Native setupConfig always schedules a trailing save after initial
        // merge/migration; ordinary reloadConfig does not request another save.
        engine.request_file_operation_config_save();
        engine.update_ignore_filters();
        engine.update_link_format();
        engine.reload_core_plugin_settings()?;
        engine.enable_metadata_lifecycle()?;
        Ok(engine)
    }
    pub(crate) fn finish_startup_layout(&mut self) -> Result<(), EngineError> {
        let cache_order = self.session.metadata_file_order.clone();
        let state_path = self.vault.root.join(".mnemosyne/session.json");
        if state_path.exists() {
            self.restore(serde_json::from_str(&read_text(&state_path)?)?)?;
        } else {
            let workspace = self.vault.root.join(CONFIG_DIR).join("workspace.json");
            if workspace.exists() {
                self.import_workspace(serde_json::from_str(&read_text(&workspace)?)?)?;
            }
        }
        self.session.metadata_file_order = cache_order;
        self.reset_editors_for_startup()?;
        self.ensure_pane_layout();
        Ok(())
    }
    /// Get today's daily note, creating it when absent, without opening an
    /// editor. The native tasks daily branch calls this same plugin operation.
    pub fn daily_note_path(&mut self) -> Result<String, EngineError> {
        let now = daily::now()?;
        self.daily_note_path_at(now)
    }
    pub fn daily_note_path_at(&mut self, now: time::OffsetDateTime) -> Result<String, EngineError> {
        daily::get_daily_note(self, now)
    }
    pub fn render_named_template(
        &mut self,
        name: &str,
        title: &str,
    ) -> Result<String, EngineError> {
        let now = daily::now()?;
        self.render_named_template_at(name, title, now)
    }
    pub fn render_named_template_at(
        &mut self,
        name: &str,
        title: &str,
        now: time::OffsetDateTime,
    ) -> Result<String, EngineError> {
        templates::render_named(self, name, title, now)
    }
    /// Combined app/appearance values with all shipped defaults. Unknown values
    /// have no default; get_config retains its compatibility null sentinel.
    pub fn get_config_optional(&self, key: &str) -> Option<&serde_json::Value> {
        self.app_config
            .get(key)
            .or_else(|| self.config_defaults.get(key))
    }
    pub fn get_config_value(&self, key: &str) -> Option<serde_json::Value> {
        self.get_config_optional(key).cloned()
    }
    pub fn explicit_config(&self) -> &serde_json::Map<String, serde_json::Value> {
        &self.app_config
    }
    pub fn config_defaults(&self) -> &serde_json::Map<String, serde_json::Value> {
        &self.config_defaults
    }
    pub fn get_config(&self, key: &str) -> &serde_json::Value {
        static NULL: serde_json::Value = serde_json::Value::Null;
        self.get_config_optional(key).unwrap_or(&NULL)
    }
    /// Synchronous persistence remains an internal implementation choice until
    /// the source save-debounce/config-changed scheduler is certified.
    pub fn set_config(&mut self, key: &str, value: serde_json::Value) -> Result<(), EngineError> {
        self.app_config.insert(key.into(), value);
        self.config_updated(key);
        self.save_config()
    }
    /// Represent source setConfig(key,undefined) without treating JSON null as
    /// deletion. An absent key simply retains its default/fallback behavior.
    pub fn unset_config(&mut self, key: &str) -> Result<(), EngineError> {
        self.app_config.shift_remove(key);
        self.config_updated(key);
        self.save_config()
    }
    fn config_updated(&mut self, key: &str) {
        self.update_ignore_filters();
        self.update_link_format();
        if key == "userIgnoreFilters" {
            self.rebuild_property_types();
        }
    }
    /// writeJson catches adapter-write failures; callers still advance in-memory
    /// state and attempt later config files. This intentionally retains that
    /// observed behavior. Serialization failure remains a typed Rust error.
    pub fn write_config_json(
        &self,
        name: &str,
        value: &serde_json::Value,
    ) -> Result<(), EngineError> {
        self.write_config_bytes(name, serde_json::to_vec_pretty(value)?)
    }
    pub fn write_config_units(
        &self,
        name: &str,
        value: &json_units::JsonUnits,
    ) -> Result<(), EngineError> {
        self.write_config_bytes(name, value.pretty().into_bytes())
    }
    fn write_config_bytes(&self, name: &str, bytes: Vec<u8>) -> Result<(), EngineError> {
        if !safe_relative(name) {
            return Err(EngineError::InvalidState("unsafe config filename".into()));
        }
        let directory = self.vault.root.join(CONFIG_DIR);
        let destination = directory.join(format!("{name}.json"));
        let temporary = directory.join(format!(
            "{name}-{}-{}.tmp",
            std::process::id(),
            NEXT_SAVE.fetch_add(1, Ordering::Relaxed)
        ));
        let write = || -> std::io::Result<()> {
            std::fs::create_dir_all(&directory)?;
            std::fs::write(&temporary, bytes)?;
            std::fs::rename(&temporary, &destination)
        };
        if write().is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        Ok(())
    }
    pub fn save_config(&self) -> Result<(), EngineError> {
        let directory = self.vault.root.join(CONFIG_DIR);
        // saveConfig's outer mkdir is outside writeJson's catch boundary.
        std::fs::create_dir_all(&directory).map_err(|error| io(&directory, error))?;
        let (app, appearance) = settings::partition(&self.app_config);
        self.write_config_json("app", &serde_json::Value::Object(app))?;
        self.write_config_json("appearance", &serde_json::Value::Object(appearance))?;
        Ok(())
    }
    /// Match the exact supplied path; cached false answers are retained too.
    pub fn is_user_ignored(&self, path: &str) -> bool {
        if !self.user_ignore_filters_active {
            return false;
        }
        if let Some(ignored) = self.user_ignore_cache.borrow().get(path).copied() {
            return ignored;
        }
        let ignored = self.user_ignore_filters.is_ignored(path);
        self.user_ignore_cache
            .borrow_mut()
            .insert(path.into(), ignored);
        ignored
    }
    pub fn config_diagnostics(&self) -> &[String] {
        &self.config_diagnostics
    }
    fn reload_app_config(&mut self) {
        let disk = ConfigDiskSnapshot::read(&self.vault.root);
        if self.file_operations.config_save_due.is_some()
            && self.app_config_disk_snapshot.as_ref() == Some(&disk)
        {
            return;
        }
        let appearance = self.read_config_json("appearance");
        let app = self.read_config_json("app");
        self.app_config = settings::merge(appearance.as_ref(), app.as_ref());
        self.app_config_disk_snapshot = Some(disk);
        self.update_ignore_filters();
        self.update_link_format();
    }
    fn read_config_json(&mut self, name: &str) -> Option<serde_json::Value> {
        let path = self.vault.root.join(CONFIG_DIR).join(format!("{name}.json"));
        match std::fs::read(&path).map(|bytes| String::from_utf8_lossy(&bytes).into_owned()) {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(value) => Some(value),
                Err(error) => {
                    self.config_diagnostics
                        .push(format!("failed to read JSON {name}.json: {error}"));
                    None
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                self.config_diagnostics
                    .push(format!("failed to read JSON {name}.json: {error}"));
                None
            }
        }
    }
    fn update_ignore_filters(&mut self) {
        let value = self.get_config("userIgnoreFilters").clone();
        let fingerprint =
            serde_json::to_string(&value).expect("JSON Value serialization cannot fail");
        if fingerprint == self.user_ignore_fingerprint {
            return;
        }
        self.user_ignore_filters_active = !matches!(
            &value,
            serde_json::Value::Null | serde_json::Value::Bool(false)
        ) && value.as_str() != Some("")
            && value.as_f64() != Some(0.0);
        let mut filters = Vec::new();
        if let Some(entries) = value.as_array() {
            for entry in entries {
                if let Some(text) = entry.as_str() {
                    filters.push(text);
                } else {
                    self.config_diagnostics.push("userIgnoreFilters entry has no string trim method; remaining entries were not compiled. Asynchronous exception propagation remains pending.".into());
                    break;
                }
            }
        } else if self.user_ignore_filters_active {
            self.config_diagnostics.push("Unsupported non-array userIgnoreFilters value; array-like JavaScript behavior remains pending.".into());
        }
        self.user_ignore_filters = IgnoreFilters::from_filters(filters);
        for (input, error) in self.user_ignore_filters.rejected_filters() {
            self.config_diagnostics.push(format!(
                "Bad regex for user ignore filter {input:?}: {error}"
            ));
        }
        self.user_ignore_fingerprint = fingerprint;
        self.user_ignore_cache.get_mut().clear();
    }
    fn update_link_format(&mut self) {
        use mnemosyne_core::vault::LinkFormat;
        self.vault.new_link_format = match self.get_config("newLinkFormat").as_str() {
            Some("relative") => LinkFormat::Relative,
            Some("absolute") => LinkFormat::Absolute,
            _ => LinkFormat::Shortest,
        };
    }
    /// Reverse depth-first traversal of current folder child insertion order.
    /// All indexed nonhidden files, including attachments, in retained vault
    /// child traversal order. Does not perform a second filesystem scan.
    pub fn ordered_file_paths(&self) -> Vec<&str> {
        self.chronology.paths()
    }
    /// Folder traversal includes empty folders and root, whose vault path is "/".
    pub fn ordered_folder_paths(&self) -> Vec<&str> {
        self.chronology.folders()
    }
    pub fn ordered_note_paths(&self) -> Vec<&str> {
        self.chronology
            .paths()
            .into_iter()
            .filter(|path| self.vault.notes.contains_key(*path))
            .collect()
    }
    pub fn ordered_notes(&self) -> impl Iterator<Item = &Note> {
        self.ordered_note_paths()
            .into_iter()
            .filter_map(|path| self.vault.get(path))
    }
    /// Saved Markdown metadata or the native empty cache for an indexed
    /// non-Markdown file. Missing/hidden/folder targets remain absent.
    /// A direct Vault mutation invalidates
    /// stale entries immediately; `file_modified`/`refresh_cache` repopulate.
    pub fn file_cache(&self, path: &str) -> Option<&CachedMetadata> {
        if external::real_path(path).is_some() {
            return self.external_files.get(path)?.metadata.as_ref();
        }
        if let Some(cached) = self.raw_file_cache.get(path) {
            return cached.as_ref()?.metadata.as_deref();
        }
        if self.vault.has_file(path) && self.vault.get(path).is_none() {
            return self.non_markdown_caches.get(path);
        }
        if self.metadata_lifecycle.retained || self.metadata_job_pending(path)
            || (!self.vault.has_file(path) && self.metadata_store.file_info(path).is_some())
        {
            return self.published_metadata_cache(path);
        }
        let note = self.vault.get(path)?;
        let cached = self.cached_notes.get(path)?;
        (cached.source.as_ref() == note.source)
            .then_some(cached.metadata.as_deref())
            .flatten()
    }
    /// A worker failure preserves saved-file discovery but has no metadata or
    /// resolved/unresolved source entry. Exact-source retention avoids turning
    /// the public permissive YAML parse into a successful cache on refresh.
    pub fn metadata_worker_failure(
        &self,
        path: &str,
    ) -> Option<&mnemosyne_core::cache::MetadataWorkerFailure> {
        if external::real_path(path).is_some() {
            return self.external_files.get(path)?.metadata_failure.as_ref();
        }
        if let Some(cached) = self.raw_file_cache.get(path) {
            return cached.as_ref()?.failure.as_deref();
        }
        let cached = self.cached_notes.get(path)?;
        let note = self.vault.get(path)?;
        (cached.source.as_ref() == note.source)
            .then_some(cached.failure.as_deref())
            .flatten()
    }
    /// Metadata getLinkpathDest's empty target refers to the loaded source
    /// TFile, including a retained raw alias or transient external file. The
    /// core Vault lookup alone only knows indexed canonical paths.
    pub fn resolve_cached_link(&self, source: &str, target: &str) -> Option<String> {
        if target.is_empty()
            && (self.vault.has_file(source)
                || self.raw_file_cache.contains_key(source)
                || external::is_file(source))
        {
            return Some(source.into());
        }
        self.vault.resolve_from_path(source, target)
    }
    fn saved_file_path(&self, path: &str) -> PathBuf {
        external::real_path(path)
            .map(Path::to_owned)
            .unwrap_or_else(|| {
                self.vault.root.join(
                    self.raw_file_destinations
                        .get(path)
                        .map_or(path, String::as_str),
                )
            })
    }
    /// Search adapter preserving current traversal order and sharing parsed cache.
    pub fn ordered_cached_notes(&self) -> impl Iterator<Item = (&Note, &CachedMetadata)> {
        self.ordered_note_paths()
            .into_iter()
            .filter_map(|path| Some((self.vault.get(path)?, self.file_cache(path)?)))
    }
    pub fn ordered_notes_with_cache(
        &self,
    ) -> impl Iterator<Item = (&Note, Option<&CachedMetadata>)> {
        self.ordered_note_paths()
            .into_iter()
            .filter_map(|path| Some((self.vault.get(path)?, self.file_cache(path))))
    }
    /// Service native metadata cleanup deadlines on the retained owner's loop.
    /// A non-due call neither scans cache entries nor performs filesystem IO.
    pub fn poll_metadata_cache_gc(&mut self) -> Result<Vec<String>, EngineError> {
        self.poll_metadata_cache_gc_at(std::time::Instant::now())
    }
    pub fn poll_metadata_cache_gc_at(
        &mut self,
        now: std::time::Instant,
    ) -> Result<Vec<String>, EngineError> {
        if !self.metadata_store.gc_due(now) {
            return Ok(Vec::new());
        }
        let aliases = self.raw_file_cache.values().flatten()
            .filter_map(|cached| cached.metadata.clone()).collect::<Vec<_>>();
        let removed = self.metadata_store.collect_unused(now, &aliases);
        if let Err(error) = self.metadata_store.persist(&self.vault.root) {
            self.config_diagnostics.push(format!("Metadata cache cleanup: {error}"));
            return Err(error);
        }
        Ok(removed)
    }
    /// Refresh typed metadata after direct Vault mutations. Exact source snapshots
    /// let unchanged cache objects survive; full Vault reparsing/scanning and
    /// duplicate saved-source storage remain pending optimization work.
    pub fn refresh_cache(&mut self) {
        if self.metadata_lifecycle.retained {
            self.reconcile_cache_order();
            return;
        }
        self.request_metadata_reclamation();
        self.non_markdown_caches.retain(|path, _| {
            self.vault.has_file(path) && self.vault.get(path).is_none()
        });
        for path in self.vault.files() {
            if self.vault.get(path).is_none() {
                self.non_markdown_caches.entry(path.clone()).or_default();
            }
        }
        self.reconcile_cache_order();
        let changed = self
            .vault
            .notes
            .iter()
            .filter(|(path, note)| {
                self.cached_notes
                    .get(*path)
                    .is_none_or(|cached| cached.source.as_ref() != note.source)
            })
            .map(|(path, _)| path.clone())
            .collect::<Vec<_>>();
        let mut previous = std::mem::take(&mut self.cached_notes);
        self.cached_notes = self.vault.notes.iter().map(|(path, note)| {
            let cached = match self.metadata_store.cached_note(&self.vault.root, path, note) {
                Ok(cached) => cached,
                Err(error) => {
                    self.config_diagnostics.push(format!("Metadata cache lookup: {error}"));
                    previous.remove(path)
                        .filter(|cached| cached.source.as_ref() == note.source)
                        .unwrap_or_else(|| CachedNote::from_note(note))
                }
            };
            (path.clone(), cached)
        }).collect();
        self.metadata_store.retain_paths(self.vault.notes.keys().cloned());
        if let Err(error) = self.metadata_store.persist(&self.vault.root) {
            self.config_diagnostics.push(format!("Metadata cache save: {error}"));
        }
        let mut manager = std::mem::take(&mut self.property_types);
        for path in changed {
            if !self.is_user_ignored(&path) {
                if let Some(metadata) = self
                    .cached_notes
                    .get(&path)
                    .and_then(|cached| cached.metadata.as_deref())
                {
                    if let Err(error) = manager.cache_changed(metadata) {
                        self.config_diagnostics
                            .push(format!("Metadata type changed: {error}"));
                    }
                }
            }
        }
        let outcome = manager.recount(
            self.cached_file_paths()
                .into_iter()
                .filter(|path| !self.is_user_ignored(path))
                .filter_map(|path| self.file_cache(path)),
        );
        if let Err(error) = outcome {
            self.config_diagnostics
                .push(format!("Metadata type occurrences: {error}"));
        }
        self.property_types = manager;
    }
    /// Synchronize saved metadata and the observed clean-editor branch.
    /// Dirty editor text stays intact; precise mutations may call the targeted
    /// status patch below. General concurrent-edit merging remains pending.
    /// Synchronize opened Markdown views after a saved-file change. Metadata is
    /// updated independently; dirty data is patched onto the new saved baseline.
    pub fn file_modified(&mut self, path: &str) {
        let tracked = self.vault.has_file(path)
            || self.chronology.folders().contains(&path)
            || self.raw_file_cache.contains_key(path);
        let kind = self.event_file_kind(path);
        if tracked {
            if let Err(error) = self.update_metadata_stat(path) {
                self.config_diagnostics.push(error.to_string());
            }
            self.emit_before_metadata_vault_event(events::EventName::Modify, path, kind, None);
            if let Err(error) = self.stage_metadata_compute(path) {
                self.config_diagnostics.push(error.to_string());
            }
        }
        self.synchronize_file_modified(path);
        if tracked {
            self.emit_after_metadata_vault_event(events::EventName::Modify, path, kind, None);
        }
        self.acknowledge_retained_path(path);
        self.drain_standalone_metadata();
    }
    /// State-only synchronization after a rename or already-notified mutation.
    /// This never turns a cache refresh/read into another operation callback.
    pub fn synchronize_file_modified(&mut self, path: &str) {
        if !self.session.leaves.iter().any(|leaf| {
            leaf.file.as_deref() == Some(path) && self.session.leaf_buffers.contains_key(&leaf.id)
        }) {
            return;
        }
        let source = self
            .vault
            .get(path)
            .map(|note| note.source.clone())
            .or_else(|| read_text(&self.vault.root.join(path)).ok());
        if let Some(source) = source {
            if let Err(error) = self.saved_content_modified(path, &source) {
                self.config_diagnostics.push(error.to_string());
            }
        }
    }
    /// Integrate already-saved raw file contents into the retained editor view.
    /// Disk writes and metadata refresh remain the caller's responsibility.
    pub fn saved_content_modified(
        &mut self,
        path: &str,
        new_saved_source: &str,
    ) -> Result<(), EngineError> {
        if self.markdown_preview_saved_modified(path, new_saved_source) {
            return Ok(());
        }
        let saved = editor_saved_text(new_saved_source);
        let saving = self.template_saving_leaves(path);
        for leaf in &mut self.session.leaves {
            if leaf.file.as_deref() != Some(path)
                || saving.contains(&leaf.id)
                || leaf.markdown.as_ref().is_some_and(|state| state.mode == markdown::MarkdownMode::Preview)
            {
                continue;
            }
            let Some(buffer) = self.session.leaf_buffers.get_mut(&leaf.id) else { continue; };
            if saved == buffer.saved_text { continue; }
            let old = std::mem::replace(&mut buffer.saved_text, saved.clone());
            if buffer.text == saved {
                if let Some(state) = leaf.markdown.as_mut() {
                    state.template_properties_data = Some(buffer.text.clone());
                }
                continue;
            }
            // A quick-preview peer's programmatic `set` can differ from its
            // saved baseline without being a user-dirty view. Q4 merges only
            // in the dirty branch; clean peers load the new saved contents.
            let dirty = leaf.markdown.as_ref().map_or(buffer.text != old, |state| {
                buffer.edit_revision != state.clean_editor_revision
            });
            let merged = if dirty && buffer.text != old {
                editor_merge::merge(&old, &buffer.text, &saved)?
            } else { saved.clone() };
            editor_merge::load_contents(buffer, &merged)?;
            if let Some(state) = leaf.markdown.as_mut() {
                state.template_properties_data = Some(buffer.text.clone());
            }
        }
        Ok(())
    }
    /// Apply core's original-full-source byte edits to a saved source snapshot,
    /// then integrate the resulting source using the view's shared merge path.
    pub fn apply_saved_source_edits(
        &mut self,
        path: &str,
        old_saved_source: &str,
        edits: &[mnemosyne_core::rewrite::TextEdit],
    ) -> Result<(), EngineError> {
        let mut source = old_saved_source.to_owned();
        let mut ordered = edits.iter().collect::<Vec<_>>();
        ordered.sort_by_key(|edit| (edit.start, edit.end));
        let mut previous_end = 0;
        for edit in &ordered {
            if edit.start < previous_end
                || edit.end < edit.start
                || edit.end > source.len()
                || !source.is_char_boundary(edit.start)
                || !source.is_char_boundary(edit.end)
            {
                return Err(EngineError::InvalidSourceEdit(format!(
                    "{}..{}",
                    edit.start, edit.end
                )));
            }
            previous_end = edit.end;
        }
        for edit in ordered.into_iter().rev() {
            source.replace_range(edit.start..edit.end, &edit.text);
        }
        self.saved_content_modified(path, &source)
    }
    /// Patch one observed task marker coordinate without merging other text.
    /// The caller supplies the zero-based saved-source editor position after
    /// BOM removal and newline normalization. Each open leaf keeps its own text
    /// and selection; the observed marker mutation is applied to each buffer.
    pub fn patch_editor_task_status(
        &mut self,
        path: &str,
        position: EditorPosition,
        new_status: &str,
    ) -> Result<(), EngineError> {
        if new_status.encode_utf16().count() != 1 {
            return Err(EngineError::InvalidTaskEditorStatus);
        }
        for leaf in &self.session.leaves {
            if leaf.file.as_deref() != Some(path) {
                continue;
            }
            let Some(buffer) = self.session.leaf_buffers.get_mut(&leaf.id) else {
                continue;
            };
            let offset = buffer.offset(position)?;
            let previous = buffer.text[offset..]
                .chars()
                .next()
                .ok_or(EngineError::InvalidPosition(position.line, position.ch))?;
            if previous.len_utf16() != 1 {
                return Err(EngineError::InvalidTaskEditorStatus);
            }
            if offset == 0
                || buffer.text.as_bytes().get(offset - 1) != Some(&b'[')
                || buffer.text.as_bytes().get(offset + previous.len_utf8()) != Some(&b']')
            {
                return Err(EngineError::MissingTaskEditorMarker(
                    position.line,
                    position.ch,
                ));
            }
            buffer
                .text
                .replace_range(offset..offset + previous.len_utf8(), new_status);
        }
        Ok(())
    }

    /// Notify successful file/folder creates before refreshing metadata.
    pub fn file_created(&mut self, path: &str) {
        if safe_relative(path)
            && self.vault.root.join(path).is_file()
            && (!self.vault.has_file(path)
                || (mnemosyne_core::extension(path) == "md" && self.vault.get(path).is_none()))
        {
            if let Err(error) = self.vault.reconcile_path_from_disk(path) {
                self.config_diagnostics.push(error.to_string());
                return;
            }
        }
        self.chronology
            .insert(path, self.vault.root.join(path).is_dir());
        self.collect_recent(path);
        let kind = self.event_file_kind(path);
        let file = self.vault.root.join(path).is_file();
        if file {
            if let Err(error) = self.update_metadata_stat(path) {
                self.config_diagnostics.push(error.to_string());
            }
        }
        self.emit_before_metadata_vault_event(events::EventName::Create, path, kind, None);
        if file {
            if let Err(error) = self.stage_metadata_compute(path) {
                self.config_diagnostics.push(error.to_string());
            }
            self.queue_metadata_related(path);
        }
        self.reconcile_cache_order();
        self.emit_after_metadata_vault_event(events::EventName::Create, path, kind, None);
        self.acknowledge_retained_path(path);
        self.drain_standalone_metadata();
    }
    pub fn session(&self) -> &SessionState {
        &self.session
    }
    pub fn active_file(&self) -> Option<&str> {
        let id = self.session.active_leaf.as_deref()?;
        self.session
            .leaves
            .iter()
            .find(|leaf| leaf.id == id)?
            .file
            .as_deref()
            .filter(|path| {
                self.vault.has_file(path)
                    || self.raw_file_cache.contains_key(*path)
                    || external::is_file(path)
            })
    }
    pub fn resolve_target(
        &self,
        target: FileTarget<'_>,
        allow_active: bool,
    ) -> Result<String, EngineError> {
        if let Some(path) = target.path.filter(|s| !s.is_empty()) {
            if self.vault.has_file(path)
                || self.raw_file_cache.contains_key(path)
                || external::is_file(path)
            {
                return Ok(path.into());
            }
            // A filesystem lookup solely diagnoses folders; it cannot broaden
            // exact path resolution to extensions, aliases or normalization.
            if safe_relative(path) && self.vault.root.join(path).is_dir() {
                return Err(EngineError::Folder(path.into()));
            }
            return Err(EngineError::FileNotFound(path.into()));
        }
        if let Some(file) = target.file.filter(|s| !s.is_empty()) {
            let source = Note::parse(PathBuf::new(), String::new());
            return self
                .vault
                .resolve_path(&source, file)
                .ok_or_else(|| EngineError::FileNotFound(file.into()));
        }
        if allow_active {
            self.active_file()
                .map(str::to_owned)
                .ok_or(EngineError::NoActiveFile)
        } else {
            Err(EngineError::MissingTarget)
        }
    }
    pub fn read_saved(&self, target: FileTarget<'_>) -> Result<String, EngineError> {
        let path = self.resolve_target(target, true)?;
        read_text(&self.saved_file_path(&path))
    }
    pub fn open_file(
        &mut self,
        target: FileTarget<'_>,
        options: OpenOptions,
    ) -> Result<String, EngineError> {
        self.open_file_in_pane(target, if options.new_tab { panes::PaneRequest::Tab } else { panes::PaneRequest::Reuse })
    }
    /// Workspace pane requests are shared backend operations. CLI `open` only
    /// supplies Reuse/Tab; its unknown paneType flag remains ignored.
    pub fn open_file_in_pane(
        &mut self,
        target: FileTarget<'_>,
        pane: panes::PaneRequest,
    ) -> Result<String, EngineError> {
        let path = self.resolve_target(target, false)?;
        if let Some(real_path) =
            external::real_path(&path).filter(|_| !self.external_files.contains_key(&path))
        {
            return match self.open_external_file_in_pane(real_path, pane)? {
                external::ExternalOpenResult::Opened(path) => Ok(path),
                other => Err(EngineError::InvalidState(format!(
                    "external view unavailable: {other:?}"
                ))),
            };
        }

        let previous_active = self.active_file().map(str::to_owned);
        if pane == panes::PaneRequest::Reuse
            && previous_active.as_deref().is_some_and(|old| old != path)
            && self.markdown_state().is_some()
        {
            self.save_active()?;
        }
        let id = self.get_leaf(pane)?;
        let index = self.session.leaves.iter().position(|leaf| leaf.id == id).unwrap();
        let new_markdown = mnemosyne_core::extension(&path) == "md";
        let changed_file = self.session.leaves[index].file.as_deref() != Some(&path);
        if new_markdown && (changed_file || self.session.leaves[index].markdown.is_none()) {
            let source = read_text(&self.saved_file_path(&path))?;
            let state = if self.session.leaves[index].view_type == "markdown" {
                self.session.leaves[index].markdown.take().map(|mut state| {
                    state.load(&source);
                    state
                })
            } else {
                None
            }
            .unwrap_or_else(|| self.new_markdown_state(&source));
            self.session.leaves[index].markdown = Some(state);
            self.session.leaf_buffers.insert(id.clone(), EditorBuffer::new(source));
        } else if !new_markdown {
            self.session.leaves[index].markdown = None;
            self.session.leaf_buffers.remove(&id);
        }
        let leaf = &mut self.session.leaves[index];
        if leaf.file.as_deref() != Some(&path) {
            if let Some(previous) = &leaf.file {
                leaf.back_history.push(previous.clone());
            }
        }
        leaf.file = Some(path.clone());
        leaf.view_type = view_type(&path).into();
        self.activate_leaf(&id)?;
        if previous_active.as_deref() != Some(&path) {
            if let Some(previous) = previous_active {
                self.collect_recent(&previous);
            }
        }
        self.reconcile_external_users()?;
        Ok(path)
    }
    pub fn editor(&self) -> Option<&EditorBuffer> {
        self.editor_for_leaf(self.session.active_leaf.as_deref()?)
    }
    pub fn editor_mut(&mut self) -> Option<&mut EditorBuffer> {
        let id = self.session.active_leaf.clone()?;
        self.editor_for_leaf_mut(&id)
    }
    pub fn editor_for_leaf(&self, id: &str) -> Option<&EditorBuffer> {
        self.session.leaf_buffers.get(id)
    }
    pub fn editor_for_leaf_mut(&mut self, id: &str) -> Option<&mut EditorBuffer> {
        self.session.leaf_buffers.get_mut(id)
    }
    pub fn save_active(&mut self) -> Result<(), EngineError> {
        self.drain_template_saves()?;
        if self.markdown_state().is_some() {
            let id = self
                .session
                .active_leaf
                .clone()
                .ok_or(EngineError::NoActiveFile)?;
            return self.save_markdown_leaf(&id);
        }
        let path = self
            .active_file()
            .ok_or(EngineError::NoActiveFile)?
            .to_owned();
        let text = self
            .markdown_view_data()
            .map(str::to_owned)
            .or_else(|| {
                self.editor().map(|buffer| buffer.text.clone())
            })
            .ok_or(EngineError::NoEditor)?;
        let unchanged = self
            .markdown_state()
            .map(|state| state.last_saved_data() == text)
            .unwrap_or_else(|| {
                self.editor().is_some_and(|buffer| {
                    buffer.saved_text == editor_saved_text(&text)
                })
            });
        if !unchanged {
            if external::real_path(&path).is_some() {
                self.modify_external_saved(&path, &text)?;
            } else {
                self.vault.modify(&path, &text)?;
                self.file_modified(&path);
            }
        }
        self.editor_mut().unwrap().saved_text = editor_saved_text(&text);
        self.mark_markdown_saved(&path, &text);
        Ok(())
    }
    pub fn recents(&self) -> &[String] {
        &self.session.last_open_files[..self.session.last_open_files.len().min(10)]
    }
    /// Called for creates and the previous file when active-file state changes,
    /// matching RecentFileTracker.collect (the new file is not collected).
    pub fn collect_recent(&mut self, path: &str) {
        if self.session.last_open_files.first().map(String::as_str) == Some(path) {
            return;
        }
        let mut quotas = [25usize, 10, 10, 10];
        let mut recent = vec![path.to_owned()];
        for old in &self.session.last_open_files {
            if old == path {
                continue;
            }
            let category = recent_category(old);
            if quotas[category] > 0 {
                recent.push(old.clone());
                quotas[category] -= 1;
            }
        }
        self.session.last_open_files = recent;
    }
    pub fn file_renamed(&mut self, from: &str, to: &str) {
        let kind = self.event_file_kind(from);
        self.rename_loaded_metadata_files(from, to);
        if mnemosyne_core::has_hidden_ancestor(to) {
            self.chronology.remove(from);
        } else {
            self.chronology.rename(from, to);
        }
        self.emit_before_metadata_vault_event(events::EventName::Rename, to, kind, Some(from));
        self.metadata_renamed(from, to);
        self.synchronize_file_renamed(from, to);
        self.stage_standalone_rename_rewrites();
        self.emit_after_metadata_vault_event(events::EventName::Rename, to, kind, Some(from));
        self.acknowledge_retained_path(from);
        self.acknowledge_retained_path(to);
        self.drain_standalone_metadata();
    }
    fn synchronize_file_renamed(&mut self, from: &str, to: &str) {
        if mnemosyne_core::has_hidden_ancestor(to) {
            self.synchronize_file_deleted(from);
            return;
        }
        let mut renamed_cache = Vec::new();
        self.session.metadata_file_order.retain(|path| {
            if let Some(renamed) = renamed_path(path, from, to) {
                renamed_cache.push(renamed);
                false
            } else {
                true
            }
        });
        self.session.metadata_file_order.extend(renamed_cache);
        self.non_markdown_caches = std::mem::take(&mut self.non_markdown_caches)
            .into_iter()
            .map(|(path, cache)| (renamed_path(&path, from, to).unwrap_or(path), cache))
            .collect();
        self.cached_notes = std::mem::take(&mut self.cached_notes)
            .into_iter()
            .map(|(path, cached)| (renamed_path(&path, from, to).unwrap_or(path), cached))
            .collect();
        self.reconcile_cache_order();
        for leaf in &mut self.session.leaves {
            if let Some(path) = &mut leaf.file {
                if let Some(renamed) = renamed_path(path, from, to) {
                    *path = renamed;
                    // A retained file view survives an extension-changing
                    // rename; reopening/reloading chooses a new view separately.
                }
            }
            for previous in &mut leaf.back_history {
                if let Some(renamed) = renamed_path(previous, from, to) {
                    *previous = renamed;
                }
            }
        }
        self.session.buffers = std::mem::take(&mut self.session.buffers)
            .into_iter()
            .map(|(path, buffer)| (renamed_path(&path, from, to).unwrap_or(path), buffer))
            .collect();
        for recent in &mut self.session.last_open_files {
            if let Some(renamed) = renamed_path(recent, from, to) {
                *recent = renamed;
            }
        }
    }
    /// Record the raw cache rename separately from its canonical filesystem/VFS
    /// destination. The CLI adapter has already completed the physical rename.
    /// This preserves the observed stale raw metadata key even if the new file
    /// is outside the vault. It never dereferences a raw key as a disk path.
    pub fn file_renamed_with_raw(&mut self, from: &str, raw: &str, physical: &str) {
        if raw == physical && safe_relative(raw) && !raw.split('/').any(|part| part == ".") {
            self.file_renamed(from, physical);
            return;
        }
        let kind = self.event_file_kind(from);
        self.rename_loaded_metadata_files(from, raw);
        if mnemosyne_core::has_hidden_ancestor(physical) {
            self.chronology.remove(from);
        } else {
            self.chronology.rename(from, physical);
        }
        self.emit_before_metadata_vault_event(events::EventName::Rename, raw, kind, Some(from));
        self.metadata_renamed(from, raw);
        let old = self
            .raw_file_cache
            .remove(from)
            .unwrap_or_else(|| self.cached_notes.remove(from));
        self.raw_file_destinations.remove(from);
        let existed = self
            .session
            .metadata_file_order
            .iter()
            .any(|path| path == from);
        self.session.metadata_file_order.retain(|path| path != from);
        if existed {
            if !self
                .session
                .metadata_file_order
                .iter()
                .any(|path| path == raw)
            {
                self.session.metadata_file_order.push(raw.to_owned());
            }
            self.raw_file_cache.insert(raw.to_owned(), old);
            self.raw_file_destinations
                .insert(raw.to_owned(), physical.to_owned());
        }
        self.synchronize_file_renamed(from, physical);
        if safe_relative(physical) && self.vault.get(physical).is_some() {
            if let Err(error) = self.stage_metadata_compute(physical) {
                self.config_diagnostics.push(error.to_string());
            }
        }
        self.stage_standalone_rename_rewrites();
        self.emit_after_metadata_vault_event(events::EventName::Rename, raw, kind, Some(from));
        self.acknowledge_retained_path(from);
        self.acknowledge_retained_path(physical);
        self.drain_standalone_metadata();
    }
    /// Apply the nonvisual file-view deletion fallback: back history, or an
    /// empty leaf. Detachment and view-specific allowNoFile branches are pending.
    pub fn file_deleted(&mut self, path: &str) {
        let tracked = self.chronology.paths().contains(&path)
            || self.chronology.folders().contains(&path)
            || self.raw_file_cache.contains_key(path);
        let kind = self.event_file_kind(path);
        self.chronology.remove(path);
        if tracked {
            self.emit_before_metadata_vault_event(events::EventName::Delete, path, kind, None);
            self.metadata_deleted(path);
        }
        self.synchronize_file_deleted(path);
        if tracked {
            self.emit_after_metadata_vault_event(events::EventName::Delete, path, kind, None);
        }
        self.acknowledge_retained_path(path);
        self.drain_standalone_metadata();
    }
    fn synchronize_file_deleted(&mut self, path: &str) {
        self.raw_file_cache.remove(path);
        self.raw_file_destinations.remove(path);
        self.chronology.remove(path);
        self.reconcile_cache_order();
        self.session.buffers.remove(path);
        let deleted_active = self.session.active_leaf.as_ref().is_some_and(|id| {
            self.session
                .leaves
                .iter()
                .any(|leaf| &leaf.id == id && leaf.file.as_deref() == Some(path))
        });
        let markdown_defaults = self.new_markdown_state("");
        for leaf in &mut self.session.leaves {
            if leaf.file.as_deref() != Some(path) {
                continue;
            }
            self.session.leaf_buffers.remove(&leaf.id);
            let previous = std::iter::from_fn(|| leaf.back_history.pop())
                .find(|previous| self.vault.has_file(previous));
            leaf.view_type = previous.as_deref().map(view_type).unwrap_or("empty").into();
            if leaf.view_type == "markdown" {
                if let Some(source) = previous
                    .as_ref()
                    .and_then(|path| read_text(&self.vault.root.join(path)).ok())
                {
                    let state = leaf
                        .markdown
                        .get_or_insert_with(|| markdown_defaults.clone());
                    state.load(&source);
                    self.session.leaf_buffers.insert(leaf.id.clone(), EditorBuffer::new(source));
                }
            } else {
                leaf.markdown = None;
            }
            leaf.file = previous;
        }
        if deleted_active {
            self.collect_recent(path);
        }
    }
    /// Refresh metadata after external changes. Preserve dirty editor text while
    /// clean buffers follow disk. File event timing is not yet certified.
    pub fn refresh_vault(&mut self) -> Result<(), EngineError> {
        if self.external_watcher.is_some() {
            self.poll_external_changes()?;
            let old_filter_fingerprint = self.user_ignore_fingerprint.clone();
            self.reload_app_config();
            if read_text(&self.vault.root.join(CONFIG_DIR).join("types.json")).ok() != self.property_config_snapshot {
                self.reload_property_types();
            } else if old_filter_fingerprint != self.user_ignore_fingerprint {
                self.rebuild_property_types();
            }
            return Ok(());
        }
        self.refresh_external_files()?;
        let vault = Vault::open(&self.vault.root)?;
        let buffer_paths = self.session.leaves.iter().filter_map(|leaf| {
            self.session.leaf_buffers.contains_key(&leaf.id).then(|| leaf.file.clone()).flatten()
        }).collect::<std::collections::BTreeSet<_>>();
        for path in buffer_paths {
            if vault.has_file(&path) {
                let text = read_text(&vault.root.join(&path))?;
                if self.session.leaves.iter().any(|leaf| {
                    leaf.file.as_deref() == Some(path.as_str())
                        && leaf
                            .markdown
                            .as_ref()
                            .is_some_and(|state| state.mode == markdown::MarkdownMode::Preview)
                }) && self.markdown_preview_saved_modified(&path, &text)
                {
                    continue;
                }
                for leaf in &self.session.leaves {
                    if leaf.file.as_deref() != Some(path.as_str())
                        || leaf.markdown.as_ref().is_some_and(|state| state.mode == markdown::MarkdownMode::Preview)
                    { continue; }
                    let Some(buffer) = self.session.leaf_buffers.get_mut(&leaf.id) else { continue; };
                    if buffer.is_dirty() { continue; }
                    if editor_saved_text(&text) != buffer.text {
                        let saved = editor_saved_text(&text);
                        editor_merge::load_contents(buffer, &saved)?;
                        buffer.saved_text = saved;
                    }
                }
            }
        }
        self.chronology
            .reconcile(chronology::FolderChronology::open(&vault.root)?);
        self.vault = vault;
        let old_filter_fingerprint = self.user_ignore_fingerprint.clone();
        self.reload_app_config();
        self.refresh_cache();
        let types = read_text(&self.vault.root.join(CONFIG_DIR).join("types.json")).ok();
        if types != self.property_config_snapshot {
            self.reload_property_types();
        } else if old_filter_fingerprint != self.user_ignore_fingerprint {
            self.rebuild_property_types();
        }
        Ok(())
    }
    pub fn persist_session(&self) -> Result<(), EngineError> {
        let directory = self.vault.root.join(".mnemosyne");
        std::fs::create_dir_all(&directory).map_err(|e| io(&directory, e))?;
        let destination = directory.join("session.json");
        let temporary = directory.join(format!(
            "session-{}-{}.tmp",
            std::process::id(),
            NEXT_SAVE.fetch_add(1, Ordering::Relaxed)
        ));
        let mut bytes = serde_json::to_vec_pretty(&self.session)?;
        bytes.push(b'\n');
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|e| io(&temporary, e))?;
        use std::io::Write;
        if let Err(error) = file.write_all(&bytes).and_then(|_| file.sync_all()) {
            let _ = std::fs::remove_file(&temporary);
            return Err(io(&temporary, error));
        }
        drop(file);
        std::fs::rename(&temporary, &destination).map_err(|e| io(&destination, e))?;
        Ok(())
    }
    /// Normal application shutdown saves visible Markdown data, discards
    /// hidden Preview editor dirt, and persists restart state with no cursor.
    /// A private persist_session checkpoint alone never saves editor contents.
    pub fn prepare_shutdown(&mut self) -> Result<(), EngineError> {
        self.flush_file_operation_config()?;
        self.save_changed_markdown_views()?;
        self.drain_metadata_for_shutdown()?;
        self.reset_editors_for_startup()?;
        self.persist_session()
    }
    pub fn reload(&mut self) -> Result<(), EngineError> {
        let retained = self.metadata_lifecycle.retained;
        let watched = self.external_watcher.is_some();
        self.prepare_shutdown()?;
        *self = if retained {Self::open_retained(&self.vault.root)?}else{Self::open(&self.vault.root)?};
        if retained {
            self.enable_metadata_lifecycle()?;
        }
        if watched {
            self.enable_retained_watch()?;
        }
        Ok(())
    }
    fn reset_editors_for_startup(&mut self) -> Result<(), EngineError> {
        // nF.activeTime is constructor state, absent from native workspace
        // serialization; setLayout activates only the chosen attached leaf.
        for leaf in &mut self.session.leaves {
            leaf.active_order = u64::from(Some(&leaf.id) == self.session.active_leaf.as_ref());
            leaf.back_history.clear(); // Native workspace does not serialize navigation history.
        }
        self.session.next_activation = 2;
        let files = self.session.leaves.iter().filter_map(|leaf| leaf.file.as_ref().map(|path| (leaf.id.clone(), path.clone()))).collect::<Vec<_>>();
        for (id, path) in files {
            if self.session.leaves.iter().find(|leaf| leaf.id == id).is_some_and(|leaf| leaf.view_type == "markdown") {
                match read_text(&self.saved_file_path(&path)) {
                    Ok(source) => {
                        self.session.leaf_buffers.insert(id.clone(), EditorBuffer::new(source.clone()));
                        if let Some(state) = self.session.leaves.iter_mut().find(|leaf| leaf.id == id).unwrap().markdown.as_mut() {
                            state.load(&source);
                            state.autosave_due_ms = None;
                            state.autosave_realtime_due = None;
                            state.internal_data_due_ms = None;
                            state.internal_data_realtime_due = None;
                        }
                    }
                    Err(error) if self.vault.has_file(&path) || external::is_file(&path) => return Err(error),
                    Err(_) => {
                        let leaf = self.session.leaves.iter_mut().find(|leaf| leaf.id == id).unwrap();
                        leaf.file = None;
                        leaf.view_type = "empty".into();
                        leaf.markdown = None;
                        self.session.leaf_buffers.remove(&id);
                    }
                }
            }
        }
        self.session.buffers.clear(); // Legacy migration data is never authoritative after startup.
        Ok(())
    }
    pub fn restore(&mut self, mut state: SessionState) -> Result<(), EngineError> {
        if state.schema_version != 1 {
            return Err(EngineError::InvalidState(format!(
                "unsupported schema {}",
                state.schema_version
            )));
        }
        // Cache-source keys may contain raw dot segments. They are opaque and
        // never used for file I/O. Startup reconciliation drops non-VFS keys,
        // matching initialize's deletePath for absent loaded-file identities.
        mnemosyne_core::collation::NaturalCollator::new(&state.collation_locale)?;
        let mut ids = std::collections::BTreeSet::new();
        for leaf in &state.leaves {
            if !ids.insert(&leaf.id) {
                return Err(EngineError::InvalidState("duplicate leaf id".into()));
            }
            if leaf.back_history.iter().any(|path| !safe_relative(path)) {
                return Err(EngineError::InvalidState("unsafe history path".into()));
            }
            if leaf.file.as_ref().is_some_and(|path| !safe_relative(path)) {
                return Err(EngineError::InvalidState("unsafe file path".into()));
            }
        }
        if state
            .active_leaf
            .as_ref()
            .is_some_and(|id| !ids.contains(id))
        {
            return Err(EngineError::InvalidState("active leaf is missing".into()));
        }
        if state
            .last_open_files
            .iter()
            .any(|path| !safe_relative(path))
        {
            return Err(EngineError::InvalidState("unsafe recent file path".into()));
        }
        for (path, buffer) in &state.buffers {
            if !safe_relative(path) {
                return Err(EngineError::InvalidState("unsafe buffer path".into()));
            }
            buffer.offset(buffer.selection.anchor)?;
            buffer.offset(buffer.selection.head)?;
        }
        // Schema-1 sessions written before per-leaf editors used a single
        // file-keyed buffer. Migrate once; never use that projection as live
        // editor ownership after restoring the session.
        for leaf in &state.leaves {
            if !state.leaf_buffers.contains_key(&leaf.id) {
                if let Some(buffer) = leaf.file.as_ref().and_then(|path| state.buffers.get(path)) {
                    state.leaf_buffers.insert(leaf.id.clone(), buffer.clone());
                }
            }
        }
        for (id, buffer) in &state.leaf_buffers {
            if !state.leaves.iter().any(|leaf| &leaf.id == id && leaf.file.is_some()) {
                return Err(EngineError::InvalidState("editor leaf is missing".into()));
            }
            buffer.offset(buffer.selection.anchor)?;
            buffer.offset(buffer.selection.head)?;
        }
        if let Some(layout) = &state.pane_layout {
            let layout_ids = layout.leaf_ids();
            let unique = layout_ids.iter().collect::<std::collections::BTreeSet<_>>();
            if unique.len() != layout_ids.len() || unique != ids {
                return Err(EngineError::InvalidState("workspace leaf identities differ".into()));
            }
        }
        state.next_activation = state.next_activation.max(state.leaves.iter().map(|leaf| leaf.active_order).max().unwrap_or(0).saturating_add(1));
        self.session = state;
        self.ensure_pane_layout();
        for index in 0..self.session.leaves.len() {
            if self.session.leaves[index].view_type == "markdown"
                && self.session.leaves[index].markdown.is_none()
            {
                if let Some(path) = self.session.leaves[index].file.clone() {
                    let source = read_text(&self.saved_file_path(&path)).ok().or_else(|| {
                        self.session
                            .leaf_buffers
                            .get(&self.session.leaves[index].id)
                            .map(|buffer| buffer.saved_text.clone())
                    });
                    if let Some(source) = source {
                        self.session.leaves[index].markdown =
                            Some(self.new_markdown_state(&source));
                    }
                }
            }
        }
        self.restore_external_bindings()?;
        self.reconcile_cache_order();
        self.rebuild_property_types();
        Ok(())
    }
    /// Import main and floating leaf identities and nonvisual pane topology.
    /// Selection is ephemeral: native workspace restart initializes it at zero.
    /// Visual/sidebar fields remain opaque inside the retained layout.
    pub fn import_workspace(&mut self, workspace: serde_json::Value) -> Result<(), EngineError> {
        let mut state = SessionState::default();
        let mut leaves = Vec::new();
        if let Some(main) = workspace.get("main") {
            import_leaves(main, &mut leaves, &self.new_markdown_state(""));
        }
        if let Some(floating) = workspace.get("floating") {
            import_leaves(floating, &mut leaves, &self.new_markdown_state(""));
        }
        if workspace.get("main").is_some() {
            let mut layout: panes::WorkspaceLayout = serde_json::from_value(workspace.clone())?;
            panes::normalize_imported_layout(&mut layout, &mut state.next_leaf_id);
            state.pane_layout = Some(layout);
        }
        if !leaves.is_empty() {
            state.active_leaf = workspace
                .get("active")
                .and_then(|v| v.as_str())
                .filter(|id| leaves.iter().any(|leaf| leaf.id == *id))
                .map(str::to_owned)
                .or_else(|| Some(leaves[0].id.clone()));
            state.leaves = leaves;
        }
        state.last_open_files = workspace
            .get("lastOpenFiles")
            .and_then(|v| v.as_array())
            .map(|values| {
                values
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        // deserializeLayout returns null for an empty non-sidebar root.
        // setLayout then allocates a new root/group/leaf and tries only the
        // first visible recent-file entry, rather than rejecting the state.
        if state.pane_layout.as_ref().is_some_and(|layout| layout.main.children.is_empty()) {
            let next_id = format!("mnemosyne-pane-{}", state.next_leaf_id);
            state.next_leaf_id += 1;
            let group_id = format!("mnemosyne-pane-{}", state.next_leaf_id);
            state.next_leaf_id += 1;
            let leaf_id = format!("mnemosyne-{}", state.next_leaf_id);
            state.next_leaf_id += 1;
            let recent = state.last_open_files.iter().filter(|path| recent_category(path) < 3).next().filter(|path| self.vault.has_file(path));
            let path = recent.cloned();
            let leaf = LeafState { id: leaf_id.clone(), pinned: false, active_order: 0,
                view_type: path.as_deref().map(view_type).unwrap_or("empty").into(),
                markdown: path.as_deref().filter(|path| view_type(path) == "markdown").map(|_| self.new_markdown_state("")),
                file: path, back_history: Vec::new() };
            let layout = state.pane_layout.as_mut().unwrap();
            let floating_ids = layout.floating.as_ref().map(|floating| { let mut ids = Vec::new(); floating.leaf_ids(&mut ids); ids }).unwrap_or_default();
            state.leaves.retain(|leaf| floating_ids.contains(&leaf.id));
            state.leaves.insert(0, leaf);
            let mut group = panes::LayoutNode::new(group_id, "tabs");
            group.children.push(panes::LayoutNode::new(leaf_id.clone(), "leaf"));
            layout.main = panes::LayoutNode::new(next_id, "split");
            layout.main.direction = Some(panes::SplitDirection::Vertical);
            layout.main.children.push(group);
            state.active_leaf = workspace.get("active").and_then(serde_json::Value::as_str).filter(|id| floating_ids.iter().any(|leaf| leaf == id)).map(str::to_owned).or(Some(leaf_id));
        }
        for leaf in &mut state.leaves {
            if let Some(path) = leaf
                .file
                .as_ref()
                .filter(|path| mnemosyne_core::extension(path) == "md" && self.vault.has_file(path))
            {
                let source = read_text(&self.vault.root.join(path))?;
                if let Some(markdown) = &mut leaf.markdown {
                    markdown.load(&source);
                }
                let buffer = EditorBuffer::new(source);
                state.leaf_buffers.insert(leaf.id.clone(), buffer.clone());
                state.buffers.entry(path.clone()).or_insert(buffer);
            }
        }
        if let Some(active) = state.active_leaf.as_ref().and_then(|id| state.leaves.iter_mut().find(|leaf| &leaf.id == id)) {
            active.active_order = state.next_activation;
            state.next_activation += 1;
        }
        state.imported_workspace = Some(workspace);
        self.restore(state)
    }
}

fn import_leaves(
    value: &serde_json::Value,
    out: &mut Vec<LeafState>,
    defaults: &markdown::MarkdownViewState,
) {
    if value.get("type").and_then(|v| v.as_str()) == Some("leaf") {
        if let (Some(id), Some(state)) =
            (value.get("id").and_then(|v| v.as_str()), value.get("state"))
        {
            let markdown = if state.get("type").and_then(|value| value.as_str()) == Some("markdown")
            {
                let mut markdown = defaults.clone();
                let values = &state["state"];
                if let Some(mode) = values.get("mode").and_then(|value| value.as_str()) {
                    match mode {
                        "source" => markdown.mode = markdown::MarkdownMode::Source,
                        "preview" => markdown.mode = markdown::MarkdownMode::Preview,
                        _ => {}
                    }
                }
                if let Some(source) = values.get("source").and_then(|value| value.as_bool()) {
                    markdown.source = source;
                }
                if let Some(backlinks) = values.get("backlinks").and_then(|value| value.as_bool()) {
                    markdown.backlinks = backlinks;
                }
                Some(markdown)
            } else {
                None
            };
            out.push(LeafState {
                id: id.into(),
                pinned: value.get("pinned").or_else(|| state.get("pinned")).and_then(serde_json::Value::as_bool).unwrap_or(false),
                active_order: 0,
                view_type: state
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("empty")
                    .into(),
                file: state
                    .get("state")
                    .and_then(|v| v.get("file"))
                    .and_then(|v| v.as_str())
                    .map(str::to_owned),
                back_history: Vec::new(),
                markdown,
            });
        }
    }
    if let Some(children) = value.get("children").and_then(|v| v.as_array()) {
        for child in children {
            import_leaves(child, out, defaults);
        }
    }
}
fn safe_relative(path: &str) -> bool {
    !path.is_empty() && !Path::new(path).is_absolute() && !path.split('/').any(|part| part == "..")
}
fn view_type(path: &str) -> &str {
    match mnemosyne_core::extension(path).as_str() {
        "md" => "markdown",
        "canvas" => "canvas",
        "base" => "bases",
        "pdf" => "pdf",
        "png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" | "avif" | "bmp" => "image",
        _ => "file",
    }
}
fn recent_category(path: &str) -> usize {
    match path.rsplit('.').next().unwrap_or("") {
        "md" => 0,
        "canvas" => 1,
        "bmp" | "png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" | "avif" => 2,
        _ => 3,
    }
}
fn io(path: &Path, error: std::io::Error) -> EngineError {
    EngineError::Io(path.display().to_string(), error)
}
fn read_text(path: &Path) -> Result<String, EngineError> {
    std::fs::read(path)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .map_err(|e| io(path, e))
}

fn normalize_editor_newlines(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

fn renamed_path(path: &str, from: &str, to: &str) -> Option<String> {
    if path == from {
        return Some(to.into());
    }
    path.strip_prefix(from)
        .filter(|suffix| suffix.starts_with('/'))
        .map(|suffix| format!("{to}{suffix}"))
}

fn editor_saved_text(source: &str) -> String {
    normalize_editor_newlines(source.strip_prefix('\u{feff}').unwrap_or(source))
}

#[cfg(test)]
mod ignore_cache_tests {
    use super::*;
    #[test]
    fn exact_path_answers_survive_same_fingerprint_and_other_config_updates() {
        let root = std::env::temp_dir().join(format!("mnemo-ignore-cache-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let mut engine = Engine::open(&root).unwrap();
        engine
            .set_config("userIgnoreFilters", serde_json::json!(["Foo"]))
            .unwrap();
        assert!(engine.is_user_ignored("Foo.md"));
        assert!(engine.is_user_ignored("foo.md"));
        assert!(!engine.is_user_ignored("./Foo.md"));
        assert_eq!(engine.user_ignore_cache.borrow().len(), 3);
        let before = engine.user_ignore_cache.borrow().clone();
        engine
            .set_config("unknown", serde_json::json!(123))
            .unwrap();
        assert_eq!(*engine.user_ignore_cache.borrow(), before);
        engine
            .set_config("userIgnoreFilters", serde_json::json!(["Foo"]))
            .unwrap();
        assert_eq!(*engine.user_ignore_cache.borrow(), before);
        engine.refresh_vault().unwrap();
        assert_eq!(*engine.user_ignore_cache.borrow(), before);
        engine
            .set_config("userIgnoreFilters", serde_json::json!(["Bar"]))
            .unwrap();
        assert!(engine.user_ignore_cache.borrow().is_empty());
        assert!(!engine.is_user_ignored("Foo.md"));
        assert!(engine.is_user_ignored("Bar.md"));
        engine
            .set_config("userIgnoreFilters", serde_json::Value::Null)
            .unwrap();
        assert!(!engine.is_user_ignored("Foo.md"));
        assert!(engine.user_ignore_cache.borrow().is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }
}
