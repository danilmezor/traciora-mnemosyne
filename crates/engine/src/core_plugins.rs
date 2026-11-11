//! Core-plugin configuration resolution. Desired flags are not a certification
//! of plugin lifecycle hooks, readiness, registrations or service side effects.
use indexmap::{IndexMap, IndexSet};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::sync::LazyLock;
use std::time::{Duration, Instant};
#[derive(Debug, Clone, Deserialize)]
pub struct CorePluginDescriptor {
    pub id: String,
    pub default_on: bool,
}
static CATALOG: LazyLock<Vec<CorePluginDescriptor>> = LazyLock::new(|| {
    serde_json::from_str(include_str!("core_plugin_defaults.json"))
        .expect("verified descriptor data")
});
pub fn catalog() -> &'static [CorePluginDescriptor] {
    &CATALOG
}
pub const LEGACY_IDS: &[&str] = &[
    "file-explorer",
    "global-search",
    "switcher",
    "graph",
    "backlink",
    "outgoing-link",
    "tag-pane",
    "page-preview",
    "daily-notes",
    "templates",
    "note-composer",
    "command-palette",
    "slash-command",
    "editor-status",
    "starred",
    "markdown-importer",
    "zk-prefixer",
    "random-note",
    "outline",
    "word-count",
    "slides",
    "audio-recorder",
    "workspaces",
    "file-recovery",
    "publish",
    "sync",
];
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::String(s) => !s.is_empty(),
        Value::Number(n) => n.as_f64() != Some(0.0),
        _ => true,
    }
}
#[derive(Debug, Clone)]
pub struct CorePluginSettings {
    config: Map<String, Value>,
    desired: IndexMap<String, bool>,
}
#[derive(Debug, thiserror::Error)]
pub enum CorePluginConfigError {
    #[error("Core-plugin config with a replaced hasOwnProperty requires JavaScript startup error propagation.")]
    PropertyShadow,
    #[error("Array-shaped core-plugin migration persistence remains unverified.")]
    ArrayMigration,
}
impl CorePluginSettings {
    pub fn resolve(
        document: Option<&Value>,
        migration: Option<&Value>,
    ) -> Result<Self, CorePluginConfigError> {
        let config = match document {
            Some(Value::Object(map)) => map.clone(),
            Some(Value::Array(ids)) => {
                if matches!(migration, Some(Value::Array(_))) {
                    return Err(CorePluginConfigError::ArrayMigration);
                }
                let mut config = migration
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                for id in LEGACY_IDS {
                    if catalog().iter().any(|p| p.id == *id) {
                        config.insert(
                            (*id).into(),
                            Value::Bool(ids.iter().any(|v| v.as_str() == Some(id))),
                        );
                    }
                }
                config
            }
            _ => Map::new(),
        };
        if config.contains_key("hasOwnProperty") {
            return Err(CorePluginConfigError::PropertyShadow);
        }
        let mut desired = IndexMap::new();
        for descriptor in catalog() {
            let enabled = config
                .get(&descriptor.id)
                .map(truthy)
                .unwrap_or(descriptor.default_on);
            desired.insert(descriptor.id.clone(), enabled);
        }
        // Resolution does not persist here. Source manager requests a debounced save
        // for missing entries, and its save path records all currently loaded flags.
        Ok(Self { config, desired })
    }
    pub fn enabled(&self, id: &str) -> bool {
        self.desired.get(id).copied().unwrap_or(false)
    }
    pub fn configured(&self) -> &Map<String, Value> {
        &self.config
    }
    pub fn desired(&self) -> &IndexMap<String, bool> {
        &self.desired
    }
    pub fn save_document(&self) -> Value {
        let mut config = self.config.clone();
        for (id, enabled) in &self.desired {
            config.insert(id.clone(), Value::Bool(*enabled));
        }
        Value::Object(config)
    }
}

impl Default for CorePluginSettings {
    fn default() -> Self {
        Self::resolve(None, None).expect("default config is ordinary JSON")
    }
}

/// Retained wrapper flags and the observed nonvisual registration subset.
///
/// Recording these transitions does not run plugin hooks or initialize their
/// backends. It is a reusable state model for a lifecycle executor, not a
/// substitute for that executor. In particular a configured true flag alone
/// does not certify plugin readiness.
#[derive(Debug, Clone)]
pub struct CorePluginState {
    enabled: IndexMap<String, bool>,
    file_creators: IndexSet<String>,
    local_cli_handlers: IndexSet<String>,
    save_due_ms: Option<u64>,
    save_realtime_due: Option<Instant>,
}
const DAILY_HANDLERS: &[&str] = &[
    "daily",
    "daily:path",
    "daily:read",
    "daily:append",
    "daily:prepend",
];
const TEMPLATE_HANDLERS: &[&str] = &["templates", "template:read", "template:insert"];
impl Default for CorePluginState {
    fn default() -> Self {
        Self {
            enabled: catalog().iter().map(|p| (p.id.clone(), false)).collect(),
            file_creators: ["md".into()].into_iter().collect(),
            local_cli_handlers: IndexSet::new(),
            save_due_ms: None,
            save_realtime_due: None,
        }
    }
}
impl CorePluginState {
    pub fn enabled(&self, id: &str) -> bool {
        self.enabled.get(id).copied().unwrap_or(false)
    }
    pub fn flags(&self) -> &IndexMap<String, bool> {
        &self.enabled
    }
    pub fn file_creators(&self) -> impl Iterator<Item = &str> {
        self.file_creators.iter().map(String::as_str)
    }
    pub fn local_cli_handlers(&self) -> impl Iterator<Item = &str> {
        self.local_cli_handlers.iter().map(String::as_str)
    }
    pub fn can_create_file_with_ext(&self, extension: &str) -> bool {
        self.file_creators.contains(if extension.is_empty() {
            "md"
        } else {
            extension
        })
    }
    /// Models manager.enable(), which never disables an already enabled wrapper.
    pub fn apply_enable_configuration(&mut self, settings: &CorePluginSettings) {
        for (id, enabled) in settings.desired() {
            if *enabled {
                self.record_enable(id);
            }
        }
        if catalog()
            .iter()
            .any(|descriptor| !settings.configured().contains_key(&descriptor.id))
        {
            self.request_save();
        }
    }
    fn request_save(&mut self) {
        // Gl's omitted third argument defaults false: an outstanding timer
        // retains the FIRST request's deadline, while the callback sees latest state.
        if self.save_due_ms.is_some() || self.save_realtime_due.is_some() {
            return;
        }
        self.save_due_ms = None;
        self.save_realtime_due = Instant::now().checked_add(Duration::from_millis(500));
    }
    fn request_save_at(&mut self, now_ms: u64) {
        self.save_realtime_due = None;
        self.save_due_ms = Some(now_ms.saturating_add(500));
    }
    fn cancel_save(&mut self) {
        self.save_due_ms = None;
        self.save_realtime_due = None;
    }
    /// Import the observed enabled transition after an executor runs the hook.
    /// Unknown IDs and already enabled wrappers are no-ops, with false returned.
    pub fn record_enable(&mut self, id: &str) -> bool {
        let Some(enabled) = self.enabled.get_mut(id) else {
            return false;
        };
        if *enabled {
            return false;
        }
        *enabled = true;
        match id {
            "canvas" => {
                self.file_creators.insert("canvas".into());
            }
            "bases" => {
                self.file_creators.insert("base".into());
            }
            _ => {}
        }
        let handlers = match id {
            "daily-notes" => DAILY_HANDLERS,
            "templates" => TEMPLATE_HANDLERS,
            _ => &[],
        };
        self.local_cli_handlers
            .extend(handlers.iter().map(|s| (*s).into()));
        self.request_save();
        true
    }
    /// Records the reference wrapper's synchronous disable state transition.
    /// Bases intentionally retains its parent-file creator after disabling.
    pub fn record_disable(&mut self, id: &str) -> bool {
        let Some(enabled) = self.enabled.get_mut(id) else {
            return false;
        };
        if !*enabled {
            return false;
        }
        *enabled = false;
        if id == "canvas" {
            self.file_creators.shift_remove("canvas");
        }
        let handlers = match id {
            "daily-notes" => DAILY_HANDLERS,
            "templates" => TEMPLATE_HANDLERS,
            _ => &[],
        };
        for handler in handlers {
            self.local_cli_handlers.shift_remove(*handler);
        }
        self.request_save();
        true
    }
    pub fn save_document(&self, settings: &CorePluginSettings) -> Value {
        let mut document = settings.configured().clone();
        for (id, enabled) in &self.enabled {
            document.insert(id.clone(), Value::Bool(*enabled));
        }
        Value::Object(document)
    }
}

impl crate::Engine {
    /// Apply the retained nonvisual Q3.enable(false) transition. This executes
    /// the supported owner/resource registrations, not unimplemented service
    /// backends. Enabled state is separate from backend readiness.
    pub fn enable_core_plugin(&mut self, id: &str) -> bool {
        self.core_plugin_state.record_enable(id)
    }
    pub fn enable_core_plugin_at(&mut self, id: &str, now_ms: u64) -> bool {
        let pending = self.core_plugin_state.save_due_ms.is_some()
            || self.core_plugin_state.save_realtime_due.is_some();
        let changed = self.enable_core_plugin(id);
        if changed && !pending {
            self.core_plugin_state.request_save_at(now_ms);
        }
        changed
    }
    /// Apply Q3.disable(false). Builtin command removal checks the exact Rust
    /// handler identity, just as source unregisterHandler checks its callback.
    /// A foreign registration of the same command remains caller-owned.
    pub fn disable_core_plugin(&mut self, id: &str) -> bool {
        let changed = self.core_plugin_state.record_disable(id);
        if changed {
            let handlers: &[&str] = match id {
                "global-search" => &["search", "search:context"],
                "outgoing-link" => &["links"],
                "word-count" => &["wordcount"],
                "outline" => &["outline"],
                "bookmarks" => &["bookmarks", "bookmark"],
                "daily-notes" => DAILY_HANDLERS,
                "templates" => TEMPLATE_HANDLERS,
                _ => &[],
            };
            for name in handlers {
                let identity = format!("builtin:{name}");
                self.commands
                    .unregister_if(name, |handler| handler == &identity);
            }
        }
        changed
    }
    pub fn disable_core_plugin_at(&mut self, id: &str, now_ms: u64) -> bool {
        let pending = self.core_plugin_state.save_due_ms.is_some()
            || self.core_plugin_state.save_realtime_due.is_some();
        let changed = self.disable_core_plugin(id);
        if changed && !pending {
            self.core_plugin_state.request_save_at(now_ms);
        }
        changed
    }
    /// Source saveConfig mutates retained config before attempting writeJson;
    /// its adapter failures are swallowed by the existing shared writer.
    pub fn save_core_plugin_config(&mut self) -> Result<(), crate::EngineError> {
        let document = self
            .core_plugin_state
            .save_document(&self.core_plugin_settings);
        self.core_plugin_settings = CorePluginSettings::resolve(Some(&document), None)?;
        self.core_plugin_state.cancel_save();
        self.write_config_json("core-plugins", &document)
    }
    /// Drive the 500ms Gl debounce in a caller-supplied logical clock domain.
    /// The return value means a save attempt, not a successful adapter write.
    pub fn process_core_plugin_timers(&mut self, now_ms: u64) -> Result<bool, crate::EngineError> {
        if self
            .core_plugin_state
            .save_due_ms
            .is_some_and(|due| due <= now_ms)
        {
            self.save_core_plugin_config()?;
            return Ok(true);
        }
        Ok(false)
    }
    /// Consume normal process-clock core owner saves, including startup defaults.
    pub fn poll_core_plugin_config(&mut self) -> Result<bool, crate::EngineError> {
        self.poll_core_plugin_config_at(Instant::now())
    }
    pub fn poll_core_plugin_config_at(&mut self, now: Instant) -> Result<bool, crate::EngineError> {
        if self
            .core_plugin_state
            .save_realtime_due
            .is_some_and(|due| due <= now)
        {
            self.save_core_plugin_config()?;
            return Ok(true);
        }
        Ok(false)
    }
}
