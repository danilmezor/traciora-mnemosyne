//! Retained headless multi-vault host. Routing is recovered desktop behavior;
//! readiness/events are explicit internal Rust state, not a claim of full
//! renderer or plugin-backend initialization, native event names or scheduling.
use crate::{
    core_plugins,
    vault_registry::{
        CliDispatchPlan, FilesystemOpenAction, FilesystemOpenPlan, RegistryError, VaultRegistry,
        VaultWindowState,
    },
    Engine, EngineError, FileTarget, OpenOptions,
};
use indexmap::IndexMap;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VaultPhase {
    Opening,
    EngineLoaded,
    Failed,
    Closed,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliReadiness {
    Pending,
    HandlerInstalled,
    DispatchReady,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendReadiness {
    Disabled,
    Pending,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultReadiness {
    pub phase: VaultPhase,
    pub cli: CliReadiness,
    pub layout_ready: bool,
    /// Initial preload is available (or synchronous open drained its snapshot).
    /// Worker completion and resolution cleanliness are separate owner facts.
    pub metadata_snapshot_loaded: bool,
    pub plugin_backends: BTreeMap<String, BackendReadiness>,
}
impl Default for VaultReadiness {
    fn default() -> Self {
        Self {
            phase: VaultPhase::Closed,
            cli: CliReadiness::Pending,
            layout_ready: false,
            metadata_snapshot_loaded: false,
            plugin_backends: BTreeMap::new(),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingCliRequest {
    pub ticket: u64,
    pub argv: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliQueueResult {
    Queued { ticket: u64 },
    Dispatch(PendingCliRequest),
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostEvent {
    CliEnabledChanged { enabled: bool },
    Opening { vault_id: String },
    EngineLoaded { vault_id: String },
    OpenFailed { vault_id: String, error: String },
    CliQueued { vault_id: String, ticket: u64 },
    HandlerInstalled { vault_id: String },
    LayoutReady { vault_id: String },
    DispatchReady { vault_id: String },
    CliCancelled { vault_id: String, ticket: u64 },
    Focused { vault_id: String, timestamp: i64 },
    Closed { vault_id: String },
}
#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error("Vault not found.")]
    VaultNotFound,
    #[error("{0}")]
    Registry(#[from] RegistryError),
    #[error("{0}")]
    Engine(#[from] EngineError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilesystemOpenResult {
    /// Native se requests window focus. This is not a delivered focus event and
    /// does not update the registry's focus timestamp.
    pub focus_requested: Option<String>,
    pub effect: FilesystemOpenEffect,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilesystemOpenEffect {
    ActivatedVault {
        vault_id: String,
    },
    Opened {
        vault_id: String,
        file: String,
        pending_subpath: Option<String>,
    },
    /// WorkspaceLeaf.openFile's unregistered-extension branch. The caller owns
    /// the platform launcher; no editor leaf has been changed by this result.
    OpenWithDefaultApplication {
        vault_id: String,
        path: PathBuf,
    },
    FileNotFound {
        vault_id: String,
        file: String,
    },
    ExternalFileMissing {
        vault_id: String,
    },
    UnsupportedExternalExtension {
        vault_id: String,
    },
    PendingExternalView {
        vault_id: String,
        view_type: String,
    },
    ChooseVault,
    Noop,
}

#[derive(Debug)]
pub struct VaultHost {
    registry: VaultRegistry,
    engines: IndexMap<String, Engine>,
    readiness: BTreeMap<String, VaultReadiness>,
    pending: BTreeMap<String, Vec<PendingCliRequest>>,
    profile_path: Option<PathBuf>,
    events: Vec<HostEvent>,
    next_ticket: u64,
    retained_services: bool,
}
impl VaultHost {
    pub fn from_registry(registry: VaultRegistry) -> Self {
        Self {
            registry,
            engines: IndexMap::new(),
            readiness: BTreeMap::new(),
            pending: BTreeMap::new(),
            profile_path: None,
            events: Vec::new(),
            next_ticket: 1,
            retained_services: false,
        }
    }
    /// Retained hosts activate services before any newly loaded engine can
    /// receive a command or filesystem-open action. Short-lived hosts opt out.
    pub fn enable_retained_services(&mut self) -> Result<(), HostError> {
        self.retained_services = true;
        for engine in self.engines.values_mut() {
            engine.enable_metadata_lifecycle()?;
            engine.enable_retained_watch()?;
        }
        Ok(())
    }
    /// Explicit profile only: does not discover or modify a personal profile.
    /// Open flags are restored separately with restore_open_vaults so a caller
    /// can install handlers and decide how to report individual loading errors.
    pub fn from_profile(profile_path: &Path, main_cwd: &Path) -> Result<Self, HostError> {
        let mut host = Self::from_registry(VaultRegistry::from_profile(profile_path, main_cwd)?);
        host.profile_path = Some(profile_path.to_owned());
        Ok(host)
    }
    pub fn registry(&self) -> &VaultRegistry {
        &self.registry
    }
    pub fn record_dispatch_diagnostic(&mut self, message: impl Into<String>) {
        self.registry.diagnostics.push(message.into());
    }
    pub fn set_cli_enabled(&mut self, enabled: bool) -> bool {
        if self.registry.set_cli_enabled(enabled) {
            self.save_profile();
            self.events.push(HostEvent::CliEnabledChanged { enabled });
        }
        self.registry.cli_enabled()
    }
    pub fn plan_cli_request(
        &self,
        argv: &[String],
        tty: bool,
        client_cwd: &Path,
    ) -> CliDispatchPlan {
        self.registry.plan_cli_request(argv, tty, client_cwd)
    }
    pub fn plan_filesystem_open(&self, path: &Path) -> FilesystemOpenPlan {
        self.registry.plan_filesystem_open(path)
    }
    /// Execute the nonvisual main He/Je routing and renderer's Markdown opening
    /// subset. OS default-app launching and chooser presentation are returned
    /// effects. Existing registrations are loaded lazily; native load ordering,
    /// did-finish-load queues and visual focus delivery remain separate inputs.
    pub fn open_filesystem_path(&mut self, path: &Path) -> Result<FilesystemOpenResult, HostError> {
        let mut plan = self.plan_filesystem_open(path);
        if plan.action == FilesystemOpenAction::RestoreOpenVaults {
            // We restores every stored-open window before He retries ve().
            // Individual failures stay visible in HostEvent::OpenFailed.
            self.restore_open_vaults();
            plan = self.plan_filesystem_open(&plan.absolute_path);
        }
        let no_focus = |effect| FilesystemOpenResult {
            focus_requested: None,
            effect,
        };
        let (id, effect) = match plan.action {
            FilesystemOpenAction::ActivateVault { vault_id } => {
                self.open_vault(&vault_id)?;
                let effect = FilesystemOpenEffect::ActivatedVault {
                    vault_id: vault_id.clone(),
                };
                (vault_id, effect)
            }
            FilesystemOpenAction::OpenVaultFile { vault_id, file } => {
                // qm splits the first # before Fl normalizes the file path.
                // This intentionally opens Hash.md for physical Hash#literal.md.
                let (file, subpath) = file
                    .split_once('#')
                    .map_or((file.as_str(), None), |(file, fragment)| {
                        (file, Some(format!("#{fragment}")))
                    });
                let normalized = mnemosyne_core::normalize_path(file);
                let engine = self.open_vault(&vault_id)?;
                let effect = if let Some(path) = engine.vault.resolve_from_path("", &normalized) {
                    if crate::view_type(&path) == "file" {
                        FilesystemOpenEffect::OpenWithDefaultApplication {
                            vault_id: vault_id.clone(),
                            path: engine.vault.root.join(path),
                        }
                    } else {
                        let path =
                            engine.open_file(FileTarget::path(&path), OpenOptions::default())?;
                        let pending_subpath = if mnemosyne_core::extension(&path) == "md" {
                            if let Some(fragment) = subpath.as_deref() {
                                engine.apply_subpath(fragment)?;
                            }
                            None
                        } else {
                            subpath
                        };
                        FilesystemOpenEffect::Opened {
                            vault_id: vault_id.clone(),
                            file: path,
                            pending_subpath,
                        }
                    }
                } else {
                    FilesystemOpenEffect::FileNotFound {
                        vault_id: vault_id.clone(),
                        file: normalized,
                    }
                };
                (vault_id, effect)
            }
            FilesystemOpenAction::OpenExternalFile { vault_id } => {
                let result = self
                    .open_vault(&vault_id)?
                    .open_external_file(&plan.absolute_path, OpenOptions::default())?;
                let effect = match result {
                    crate::external::ExternalOpenResult::Opened(file) => {
                        FilesystemOpenEffect::Opened {
                            vault_id: vault_id.clone(),
                            file,
                            pending_subpath: None,
                        }
                    }
                    crate::external::ExternalOpenResult::Missing => {
                        FilesystemOpenEffect::ExternalFileMissing {
                            vault_id: vault_id.clone(),
                        }
                    }
                    crate::external::ExternalOpenResult::UnsupportedExtension => {
                        FilesystemOpenEffect::UnsupportedExternalExtension {
                            vault_id: vault_id.clone(),
                        }
                    }
                    crate::external::ExternalOpenResult::PendingViewType(view_type) => {
                        FilesystemOpenEffect::PendingExternalView {
                            vault_id: vault_id.clone(),
                            view_type,
                        }
                    }
                };
                (vault_id, effect)
            }
            FilesystemOpenAction::RestoreOpenVaults | FilesystemOpenAction::ChooseVault => {
                return Ok(no_focus(FilesystemOpenEffect::ChooseVault))
            }
            FilesystemOpenAction::Noop => return Ok(no_focus(FilesystemOpenEffect::Noop)),
        };
        Ok(FilesystemOpenResult {
            focus_requested: Some(id),
            effect,
        })
    }
    pub fn engine(&self, id: &str) -> Option<&Engine> {
        self.engines.get(id)
    }
    pub fn engine_mut(&mut self, id: &str) -> Option<&mut Engine> {
        self.engines.get_mut(id)
    }
    pub fn loaded_vault_ids(&self) -> impl Iterator<Item = &str> {
        self.engines.keys().map(String::as_str)
    }
    /// Application shutdown preserves registration.open for startup restore;
    /// it is different from the explicit close-vault transition.
    pub fn persist_sessions(&self) -> Vec<(String, Result<(), EngineError>)> {
        self.engines
            .iter()
            .map(|(id, engine)| (id.clone(), engine.persist_session()))
            .collect()
    }
    /// A real application shutdown saves view data and resets startup-only
    /// editor state, independently of an internal session checkpoint.
    pub fn prepare_shutdown_sessions(&mut self) -> Vec<(String, Result<(), EngineError>)> {
        self.engines
            .iter_mut()
            .map(|(id, engine)| (id.clone(), engine.prepare_shutdown()))
            .collect()
    }
    pub fn readiness(&self, id: &str) -> Option<&VaultReadiness> {
        self.readiness.get(id)
    }
    pub fn drain_events(&mut self) -> Vec<HostEvent> {
        std::mem::take(&mut self.events)
    }
    /// Engine loading is synchronous. EngineLoaded is distinct from the CLI
    /// handler-ready boundary and from pending enabled plugin backends.
    pub fn open_vault(&mut self, id: &str) -> Result<&mut Engine, HostError> {
        if !self.engines.contains_key(id) {
            let path = self
                .registry
                .registration(id)
                .ok_or(HostError::VaultNotFound)?
                .path
                .clone();
            let readiness = self.readiness.entry(id.to_owned()).or_default();
            readiness.phase = VaultPhase::Opening;
            self.events.push(HostEvent::Opening {
                vault_id: id.into(),
            });
            let retained = self.retained_services;
            let opened = (if retained {
                Engine::open_retained(&path)
            } else {
                Engine::open(&path)
            }).and_then(|mut engine| {
                if retained {
                    engine.enable_metadata_lifecycle()?;
                    engine.enable_retained_watch()?;
                }
                Ok(engine)
            });
            match opened {
                Ok(engine) => {
                    let plugins = core_plugins::catalog()
                        .iter()
                        .map(|p| {
                            (
                                p.id.clone(),
                                if engine.is_core_plugin_enabled(&p.id) {
                                    BackendReadiness::Pending
                                } else {
                                    BackendReadiness::Disabled
                                },
                            )
                        })
                        .collect();
                    self.engines.insert(id.into(), engine);
                    self.readiness.insert(
                        id.into(),
                        VaultReadiness {
                            phase: VaultPhase::EngineLoaded,
                            cli: CliReadiness::Pending,
                            layout_ready: false,
                            metadata_snapshot_loaded: true,
                            plugin_backends: plugins,
                        },
                    );
                    self.registry
                        .set_window_state(id, VaultWindowState::default());
                    self.registry.set_registration_open(id, true);
                    self.save_profile();
                    self.events.push(HostEvent::EngineLoaded {
                        vault_id: id.into(),
                    });
                }
                Err(error) => {
                    self.readiness.get_mut(id).unwrap().phase = VaultPhase::Failed;
                    self.events.push(HostEvent::OpenFailed {
                        vault_id: id.into(),
                        error: error.to_string(),
                    });
                    return Err(error.into());
                }
            }
        }
        Ok(self.engines.get_mut(id).unwrap())
    }
    pub fn restore_open_vaults(&mut self) -> Vec<(String, Result<(), HostError>)> {
        let ids = self
            .registry
            .registrations()
            .filter(|entry| {
                entry
                    .metadata
                    .get("open")
                    .is_some_and(crate::settings::js_truthy)
            })
            .map(|entry| entry.id.clone())
            .collect::<Vec<_>>();
        ids.into_iter()
            .map(|id| {
                let result = self.open_vault(&id).map(|_| ());
                (id, result)
            })
            .collect()
    }
    /// Model the main→renderer cliQueue boundary. DispatchReady must only be
    /// marked by the caller after it has actually installed command dispatch.
    pub fn enqueue_cli(
        &mut self,
        id: &str,
        argv: Vec<String>,
    ) -> Result<CliQueueResult, HostError> {
        self.open_vault(id)?;
        let request = PendingCliRequest {
            ticket: self.next_ticket,
            argv,
        };
        self.next_ticket += 1;
        if self.readiness[id].cli != CliReadiness::Pending {
            return Ok(CliQueueResult::Dispatch(request));
        }
        let ticket = request.ticket;
        self.pending.entry(id.into()).or_default().push(request);
        self.events.push(HostEvent::CliQueued {
            vault_id: id.into(),
            ticket,
        });
        Ok(CliQueueResult::Queued { ticket })
    }
    /// Source NF.init installs handleCli before layout-ready. New requests can
    /// dispatch immediately, while older main-process cliQueue entries remain
    /// queued until the workspace onLayoutReady callback runs.
    pub fn mark_handler_installed(&mut self, id: &str) -> Result<(), HostError> {
        if !self.engines.contains_key(id) {
            return Err(HostError::VaultNotFound);
        }
        if self.readiness[id].cli == CliReadiness::Pending {
            self.readiness.get_mut(id).unwrap().cli = CliReadiness::HandlerInstalled;
            self.events.push(HostEvent::HandlerInstalled {
                vault_id: id.into(),
            });
        }
        Ok(())
    }
    /// Returns queued requests in source insertion order at workspace readiness.
    /// The caller owns execution and response promises/streams.
    pub fn mark_layout_ready(&mut self, id: &str) -> Result<Vec<PendingCliRequest>, HostError> {
        self.mark_handler_installed(id)?;
        if !self.readiness[id].layout_ready {
            self.readiness.get_mut(id).unwrap().layout_ready = true;
            self.events.push(HostEvent::LayoutReady {
                vault_id: id.into(),
            });
        }
        if self.readiness[id].cli != CliReadiness::DispatchReady {
            self.readiness.get_mut(id).unwrap().cli = CliReadiness::DispatchReady;
            self.events.push(HostEvent::DispatchReady {
                vault_id: id.into(),
            });
        }
        Ok(self.pending.remove(id).unwrap_or_default())
    }
    /// Convenience for callers that have completed both explicit boundaries.
    /// No output is fabricated by either readiness method.
    pub fn mark_dispatch_ready(&mut self, id: &str) -> Result<Vec<PendingCliRequest>, HostError> {
        self.mark_layout_ready(id)
    }
    pub fn record_focus(&mut self, id: &str, timestamp: i64) -> bool {
        if !self.registry.record_focus(id, timestamp) {
            return false;
        }
        self.events.push(HostEvent::Focused {
            vault_id: id.into(),
            timestamp,
        });
        true
    }
    /// Close the retained vault after the same view-save/startup boundary as a
    /// normal application shutdown. Other loaded vaults retain their live state.
    pub fn close_vault(&mut self, id: &str) -> Result<bool, HostError> {
        let Some(engine) = self.engines.get_mut(id) else {
            return Ok(false);
        };
        engine.prepare_shutdown()?;
        self.engines.shift_remove(id);
        self.registry.remove_window_state(id);
        self.registry.set_registration_open(id, false);
        self.readiness.insert(id.into(), VaultReadiness::default());
        for request in self.pending.remove(id).unwrap_or_default() {
            self.events.push(HostEvent::CliCancelled {
                vault_id: id.into(),
                ticket: request.ticket,
            });
        }
        self.save_profile();
        self.events.push(HostEvent::Closed {
            vault_id: id.into(),
        });
        Ok(true)
    }
    fn save_profile(&mut self) {
        let Some(path) = &self.profile_path else {
            return;
        };
        let result = serde_json::to_vec(self.registry.document())
            .map_err(std::io::Error::other)
            .and_then(|bytes| std::fs::write(path, bytes));
        // Main ae→he catches writes and logs Ignored rather than rejecting the
        // already-completed window/registry transition. Retain diagnostics here.
        if let Err(error) = result {
            self.registry
                .diagnostics
                .push(format!("Ignored registry write: {error}"));
        }
    }
}
