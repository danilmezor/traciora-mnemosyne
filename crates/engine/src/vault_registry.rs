//! Desktop vault registry and CLI vault selection rules.
//! This models registry and live focus inputs, separate from renderer
//! creation, readiness queues, URI/path side effects and OS window rendering.
use indexmap::IndexMap;
use serde_json::{Map, Value};
use std::path::{Component, Path, PathBuf};

/// Names shared with existing desktop installs, kept so their vault registry,
/// bundled help vault and vault links keep working.
pub const APP_DIR: &str = "obsidian";
pub const REGISTRY_FILE: &str = "obsidian.json";
pub const HELP_VAULT: &str = "Obsidian Help";
pub const URI_SCHEME: &str = "obsidian://";

/// The desktop vault registry under `home`: the macOS application-support
/// location when present, else the XDG config location.
pub fn registry_path(home: &Path) -> PathBuf {
    let mac = home
        .join("Library/Application Support")
        .join(APP_DIR)
        .join(REGISTRY_FILE);
    if mac.exists() {
        return mac;
    }
    home.join(".config").join(APP_DIR).join(REGISTRY_FILE)
}

#[derive(Debug, Clone)]
pub struct VaultRegistration {
    pub id: String,
    pub path: PathBuf,
    pub metadata: Map<String, Value>,
}
#[derive(Debug, Clone, Copy, Default)]
pub struct VaultWindowState {
    pub focus_time: Option<i64>,
    pub destroyed: bool,
}
#[derive(Debug, Clone)]
pub struct VaultRegistry {
    document: Map<String, Value>,
    entries: IndexMap<String, VaultRegistration>,
    windows: IndexMap<String, VaultWindowState>,
    cwd: PathBuf,
    pub diagnostics: Vec<String>,
}
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("Unsupported vault registry entry {0:?}.")]
    Entry(String),
    #[error("Unsupported non-object vault registry schema.")]
    Schema,
    #[error("Registry selection requires an absolute process cwd.")]
    Cwd,
}
pub const CLI_DISABLED: &str =
    "Command line interface is not enabled. Please turn it on in Settings > General > Advanced.";
pub const VAULT_NOT_FOUND: &str = "Vault not found.";

/// Main-process request routing, before the renderer's command registry runs.
/// URI/path opening and interactive/chooser execution are not implemented here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliDispatchPlan {
    pub open_chooser_first: bool,
    pub action: CliDispatchAction,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliDispatchAction {
    ProcessUri(String),
    OpenPath(String),
    Disabled,
    Interactive {
        vault_id: Option<String>,
    },
    Execute {
        vault_id: Option<String>,
        argv: Vec<String>,
    },
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilesystemOpenPlan {
    pub absolute_path: PathBuf,
    pub action: FilesystemOpenAction,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilesystemOpenAction {
    ActivateVault {
        vault_id: String,
    },
    /// Exact OBS_ACT open payload; leading separator and literal # are retained.
    OpenVaultFile {
        vault_id: String,
        file: String,
    },
    OpenExternalFile {
        vault_id: String,
    },
    /// Main We opens stored-open vaults, then He repeats focused-vault lookup.
    RestoreOpenVaults,
    ChooseVault,
    Noop,
}
fn resolved(path: &Path, cwd: &Path) -> PathBuf {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            other => result.push(other.as_os_str()),
        }
    }
    result
}
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}
impl VaultRegistry {
    /// Explicit profile input only. Reading does not delete profile partitions,
    /// stale per-vault JSON or mutate the user's registry as native startup does.
    pub fn from_profile(profile_json: &Path, process_cwd: &Path) -> Result<Self, RegistryError> {
        let mut diagnostics = Vec::new();
        let document = match std::fs::read(profile_json) {
            Ok(bytes) => match serde_json::from_str::<Value>(&String::from_utf8_lossy(&bytes)) {
                Ok(value) => value,
                Err(error) => {
                    diagnostics.push(format!("Ignored registry JSON: {error}"));
                    Value::Object(Map::new())
                }
            },
            Err(error) => {
                diagnostics.push(format!("Ignored registry read: {error}"));
                Value::Object(Map::new())
            }
        };
        let mut registry = Self::from_document(
            document,
            process_cwd,
            profile_json
                .parent()
                .map(|p| p.join(HELP_VAULT))
                .as_deref(),
        )?;
        registry.diagnostics = diagnostics;
        Ok(registry)
    }
    pub fn from_document(
        document: Value,
        process_cwd: &Path,
        excluded_help_path: Option<&Path>,
    ) -> Result<Self, RegistryError> {
        if !process_cwd.is_absolute() {
            return Err(RegistryError::Cwd);
        }
        let mut document = match document {
            Value::Object(map) => map,
            Value::Array(_) => return Err(RegistryError::Schema),
            _ => Map::new(),
        };
        let mut entries = IndexMap::new();
        let mut vaults = Map::new();
        match document.get("vaults") {
            Some(Value::Object(raw)) => {
                for (id, value) in crate::settings::enumerable(&Value::Object(raw.clone())) {
                    let mut metadata = value
                        .as_object()
                        .cloned()
                        .ok_or_else(|| RegistryError::Entry(id.clone()))?;
                    let path = metadata
                        .get("path")
                        .and_then(Value::as_str)
                        .ok_or_else(|| RegistryError::Entry(id.clone()))?;
                    let path = resolved(Path::new(path), process_cwd);
                    if excluded_help_path == Some(path.as_path()) || !path.exists() {
                        continue;
                    }
                    metadata.insert(
                        "path".into(),
                        Value::String(path.to_string_lossy().into_owned()),
                    );
                    vaults.insert(id.clone(), Value::Object(metadata.clone()));
                    entries.insert(id.clone(), VaultRegistration { id, path, metadata });
                }
            }
            Some(Value::Null) | None => {}
            Some(_) => return Err(RegistryError::Schema),
        }
        document.insert("vaults".into(), Value::Object(vaults));
        Ok(Self {
            document,
            entries,
            windows: IndexMap::new(),
            cwd: process_cwd.into(),
            diagnostics: Vec::new(),
        })
    }
    pub fn document(&self) -> &Map<String, Value> {
        &self.document
    }
    pub fn registrations(&self) -> impl Iterator<Item = &VaultRegistration> {
        self.entries.values()
    }
    pub fn cli_enabled(&self) -> bool {
        self.document.get("cli").is_some_and(truthy)
    }
    /// Main IPC cli accepts only boolean toggles, emits/saves only on a truthy
    /// state transition, and deletes the global key when disabling.
    pub fn set_cli_enabled(&mut self, enabled: bool) -> bool {
        if enabled == self.cli_enabled() {
            return false;
        }
        if enabled {
            self.document.insert("cli".into(), Value::Bool(true));
        } else {
            self.document.shift_remove("cli");
        }
        true
    }
    pub fn set_window_state(&mut self, id: &str, state: VaultWindowState) -> bool {
        if !self.entries.contains_key(id) {
            return false;
        }
        self.windows.insert(id.into(), state);
        true
    }
    pub fn remove_window_state(&mut self, id: &str) -> Option<VaultWindowState> {
        self.windows.shift_remove(id)
    }
    pub fn record_focus(&mut self, id: &str, timestamp: i64) -> bool {
        let Some(state) = self.windows.get_mut(id) else {
            return false;
        };
        if state.destroyed {
            return false;
        }
        state.focus_time = Some(timestamp);
        true
    }
    /// Source yt updates registration.open without changing its insertion slot
    /// or its separate last-open timestamp ts. Disk persistence is caller-owned.
    pub fn set_registration_open(&mut self, id: &str, open: bool) -> bool {
        let Some(registration) = self.entries.get_mut(id) else {
            return false;
        };
        registration
            .metadata
            .insert("open".into(), Value::Bool(open));
        self.document
            .get_mut("vaults")
            .and_then(Value::as_object_mut)
            .expect("normalized vault object")
            .insert(id.to_owned(), Value::Object(registration.metadata.clone()));
        true
    }
    pub fn registration(&self, id: &str) -> Option<&VaultRegistration> {
        self.entries.get(id)
    }
    pub fn find_selector(&self, selector: &str) -> Option<&str> {
        let upper = selector.to_uppercase();
        self.entries.iter().find_map(|(id, entry)| {
            let name = entry
                .path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            (id == selector || name.to_uppercase() == upper).then_some(id.as_str())
        })
    }
    /// Native CLI cwd selection returns the first containing registered root,
    /// unlike the separate longest-containing-root URI/file routing functions.
    pub fn find_cwd(&self, cwd: &Path) -> Option<&str> {
        let candidate = resolved(cwd, &self.cwd);
        let candidate = candidate.to_string_lossy();
        self.entries.iter().find_map(|(id, entry)| {
            let root = entry.path.to_string_lossy();
            (candidate == root
                || candidate.starts_with(&format!("{root}{}", std::path::MAIN_SEPARATOR)))
            .then_some(id.as_str())
        })
    }
    /// Main He/Ze choose the longest containing root, keeping the first entry
    /// for equal roots. This intentionally differs from first-match CLI cwd.
    pub fn longest_containing_vault(&self, path: &Path) -> Option<&str> {
        let candidate = resolved(path, &self.cwd);
        let candidate = candidate.to_string_lossy();
        let mut best: Option<(&str, usize)> = None;
        for (id, entry) in &self.entries {
            let root = entry.path.to_string_lossy();
            if (candidate == root
                || candidate.starts_with(&format!("{root}{}", std::path::MAIN_SEPARATOR)))
                && best.is_none_or(|(_, length)| root.len() > length)
            {
                best = Some((id, root.len()));
            }
        }
        best.map(|(id, _)| id)
    }
    /// Native main He routing only; it never substitutes client cwd, resolves
    /// symlink identities or assumes a requested focus event was delivered.
    pub fn plan_filesystem_open(&self, path: &Path) -> FilesystemOpenPlan {
        let absolute_path = resolved(path, &self.cwd);
        let containing = self.longest_containing_vault(&absolute_path);
        let file = std::fs::metadata(&absolute_path).is_ok_and(|metadata| metadata.is_file());
        let action = if !file {
            containing
                .map(|id| FilesystemOpenAction::ActivateVault {
                    vault_id: id.into(),
                })
                .unwrap_or(FilesystemOpenAction::Noop)
        } else if let Some(id) = containing {
            let root = self.entries[id].path.to_string_lossy();
            let path = absolute_path.to_string_lossy();
            FilesystemOpenAction::OpenVaultFile {
                vault_id: id.into(),
                file: path[root.len()..].into(),
            }
        } else if let Some(id) = self.focused_vault() {
            FilesystemOpenAction::OpenExternalFile {
                vault_id: id.into(),
            }
        } else if self
            .entries
            .values()
            .any(|entry| entry.metadata.get("open").is_some_and(truthy))
        {
            FilesystemOpenAction::RestoreOpenVaults
        } else {
            FilesystemOpenAction::ChooseVault
        };
        FilesystemOpenPlan {
            absolute_path,
            action,
        }
    }
    pub fn focused_vault(&self) -> Option<&str> {
        let mut best: Option<(&str, VaultWindowState)> = None;
        let mut windows: Vec<_> = self.windows.iter().collect();
        // Y is a JavaScript object; normal random 16-hex IDs retain insertion
        // order, but an explicitly imported uint32 key is enumerated first.
        windows.sort_by(|(a, _), (b, _)| {
            match (
                crate::settings::array_index(a),
                crate::settings::array_index(b),
            ) {
                (Some(a), Some(b)) => a.cmp(&b),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                _ => std::cmp::Ordering::Equal,
            }
        });
        for (id, state) in windows {
            if state.destroyed {
                continue;
            }
            let newer = best.is_none_or(|(_, old)| match (state.focus_time, old.focus_time) {
                (Some(new), Some(old)) => new > old,
                _ => false,
            });
            if newer {
                best = Some((id, *state));
            }
        }
        best.map(|(id, _)| id)
    }
    /// An explicit unmatched selector does not fall back to cwd or focused vault.
    pub fn select_cli(&self, selector: Option<&str>, cwd: &Path) -> Option<&str> {
        match selector {
            Some(selector) => self.find_selector(selector),
            None => self.find_cwd(cwd).or_else(|| self.focused_vault()),
        }
    }
    /// `process_cwd` supplied at construction is the native MAIN process cwd.
    /// Native last-argument existsSync uses it, whereas vault containment uses
    /// the CLIENT cwd in this request. These are intentionally different inputs.
    pub fn plan_cli_request(
        &self,
        argv: &[String],
        tty: bool,
        client_cwd: &Path,
    ) -> CliDispatchPlan {
        let open_chooser_first = !tty && argv.is_empty();
        let action = if let Some(uri) = argv.last().filter(|s| s.starts_with(URI_SCHEME)) {
            CliDispatchAction::ProcessUri(uri.clone())
        } else if let Some(path) = argv
            .last()
            .filter(|s| !s.is_empty() && resolved(Path::new(s), &self.cwd).exists())
        {
            CliDispatchAction::OpenPath(path.clone())
        } else if !self.cli_enabled() {
            CliDispatchAction::Disabled
        } else {
            let (selector, command) =
                if let Some(selector) = argv.first().and_then(|s| s.strip_prefix("vault=")) {
                    (Some(selector), &argv[1..])
                } else {
                    (None, argv)
                };
            let vault_id = self.select_cli(selector, client_cwd).map(str::to_owned);
            if tty && command.is_empty() {
                CliDispatchAction::Interactive { vault_id }
            } else {
                CliDispatchAction::Execute {
                    vault_id,
                    argv: command.to_vec(),
                }
            }
        };
        CliDispatchPlan {
            open_chooser_first,
            action,
        }
    }
}
