//! Serial FileManager rename operations and their nonvisual link-update decision.
//! A decision belongs to the application, independently of any CLI connection.
use crate::{metadata_lifecycle::FileId, raw_links::RawLinkUpdatePlan, Engine, EngineError};
use mnemosyne_core::vault::RenamePlan;
use std::collections::{BTreeMap, VecDeque};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct FileOperationId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkUpdateDecision {
    DoNotUpdate,
    Once,
    Always,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingLinkUpdate {
    pub id: FileOperationId,
    pub from: String,
    pub to: String,
    pub affected_references: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum FileOperationError {
    #[error("file operation {0:?} is not awaiting a link-update decision")]
    NoPendingDecision(FileOperationId),
}

#[derive(Debug)]
struct Request {
    id: FileOperationId,
    from: String,
    file: Option<FileId>,
    raw_to: String,
    physical_to: String,
}

#[derive(Debug)]
struct AwaitingDecision {
    request: Request,
    plan: RenamePlan,
    raw_plan: RawLinkUpdatePlan,
    decision: Option<LinkUpdateDecision>,
}

#[derive(Debug, Default)]
pub(crate) struct FileOperations {
    next: u64,
    requests: VecDeque<Request>,
    active: Option<AwaitingDecision>,
    completed: BTreeMap<FileOperationId, Result<(), EngineError>>,
    abandoned: std::collections::BTreeSet<FileOperationId>,
    pub(crate) config_save_due: Option<std::time::Instant>,
}

impl FileOperations {
    pub(crate) fn retained_file_ids(&self) -> impl Iterator<Item = FileId> + '_ {
        self.requests
            .iter()
            .filter_map(|request| request.file)
            .chain(self.active.iter().filter_map(|active| active.request.file))
    }
    fn complete(&mut self, id: FileOperationId, result: Result<(), EngineError>) {
        if !self.abandoned.remove(&id) {
            self.completed.insert(id, result);
        }
    }
}

impl Engine {
    fn set_file_operation_config(&mut self, key: &str, value: serde_json::Value) {
        self.app_config.insert(key.into(), value);
        self.config_updated(key);
        self.request_file_operation_config_save();
    }

    pub(crate) fn request_file_operation_config_save(&mut self) {
        self.file_operations.config_save_due =
            std::time::Instant::now().checked_add(std::time::Duration::from_secs(1));
    }

    /// Poll the source one-second trailing configuration save requested by the
    /// initial configuration setup or FileManager Always choice. The callback
    /// reads the current config.
    pub fn poll_file_operation_config(&mut self) -> Result<bool, EngineError> {
        self.poll_file_operation_config_at(std::time::Instant::now())
    }
    pub fn poll_file_operation_config_at(
        &mut self,
        now: std::time::Instant,
    ) -> Result<bool, EngineError> {
        if self
            .file_operations
            .config_save_due
            .is_none_or(|due| due > now)
        {
            return Ok(false);
        }
        self.flush_file_operation_config()
    }
    pub(crate) fn flush_file_operation_config(&mut self) -> Result<bool, EngineError> {
        if self.file_operations.config_save_due.take().is_none() {
            return Ok(false);
        }
        if let Err(error) = self.save_config() {
            self.config_diagnostics
                .push(format!("FileManager config save: {error}"));
            return Err(error);
        }
        Ok(true)
    }

    /// Queue a FileManager rename. Metadata must become clean before references
    /// are captured; queued application requests share this serial owner.
    pub fn begin_file_rename(
        &mut self,
        from: &str,
        raw_to: &str,
        physical_to: &str,
    ) -> FileOperationId {
        self.file_operations.next += 1;
        let id = FileOperationId(self.file_operations.next);
        let file = self.file_operation_identity(from);
        self.file_operations.requests.push_back(Request {
            id,
            from: from.into(),
            file,
            raw_to: raw_to.into(),
            physical_to: physical_to.into(),
        });
        id
    }

    pub fn pending_link_update(&self) -> Option<PendingLinkUpdate> {
        let active = self.file_operations.active.as_ref()?;
        active.decision.is_none().then(|| PendingLinkUpdate {
            id: active.request.id,
            from: active.request.from.clone(),
            to: active.request.raw_to.clone(),
            affected_references: active.plan.affected_references(),
        })
    }

    pub fn decide_link_update(
        &mut self,
        id: FileOperationId,
        decision: LinkUpdateDecision,
    ) -> Result<(), FileOperationError> {
        let active = self
            .file_operations
            .active
            .as_mut()
            .filter(|active| active.request.id == id && active.decision.is_none())
            .ok_or(FileOperationError::NoPendingDecision(id))?;
        active.decision = Some(decision);
        Ok(())
    }

    /// Advance one serial application operation. This does not pump metadata or
    /// wait for cache cleanliness after a rename/write has completed.
    pub fn poll_file_operations(&mut self) -> usize {
        let _ = self.poll_file_operation_config();
        if self.file_operations.active.is_none() {
            if !self.metadata_readiness().is_cache_clean {
                return 0;
            }
            let Some(mut request) = self.file_operations.requests.pop_front() else {
                return 0;
            };
            let current = request
                .file
                .and_then(|id| self.file_operation_path(id))
                .map(str::to_owned);
            let Some(current) = current else {
                let error = mnemosyne_core::VaultError::NotFound(request.from.clone());
                self.file_operations.complete(request.id, Err(error.into()));
                self.request_metadata_reclamation();
                return 1;
            };
            request.from = current;
            let raw_plan = self.prepare_raw_link_update();
            let plan = match self
                .vault
                .rename_and_plan(&request.from, &request.physical_to)
            {
                Ok(plan) => plan,
                Err(error) => {
                    self.file_operations.complete(request.id, Err(error.into()));
                    self.request_metadata_reclamation();
                    return 1;
                }
            };
            if request.from != request.physical_to {
                self.file_renamed_with_raw(&request.from, &request.raw_to, &request.physical_to);
                self.synchronize_file_modified(&request.physical_to);
            }
            let automatic = crate::settings::js_truthy(self.get_config("alwaysUpdateLinks"));
            let decision =
                (automatic || plan.affected_references() == 0).then_some(LinkUpdateDecision::Once);
            self.file_operations.active = Some(AwaitingDecision {
                request,
                plan,
                raw_plan,
                decision,
            });
        }
        if self
            .file_operations
            .active
            .as_ref()
            .is_some_and(|active| active.decision.is_none())
        {
            return 0;
        }
        let active = self.file_operations.active.take().unwrap();
        let id = active.request.id;
        let result = self.complete_file_rename(active);
        self.file_operations.complete(id, result);
        self.request_metadata_reclamation();
        1
    }

    pub fn take_file_operation_result(
        &mut self,
        id: FileOperationId,
    ) -> Option<Result<(), EngineError>> {
        self.file_operations.completed.remove(&id)
    }

    /// Detach a consumer without cancelling a started application operation.
    pub fn abandon_file_operation_result(&mut self, id: FileOperationId) {
        if self.file_operations.completed.remove(&id).is_none()
            && (self
                .file_operations
                .requests
                .iter()
                .any(|request| request.id == id)
                || self
                    .file_operations
                    .active
                    .as_ref()
                    .is_some_and(|active| active.request.id == id))
        {
            self.file_operations.abandoned.insert(id);
        }
    }

    fn complete_file_rename(&mut self, active: AwaitingDecision) -> Result<(), EngineError> {
        let request = active.request;
        match active.decision.unwrap() {
            LinkUpdateDecision::DoNotUpdate => {}
            decision => {
                if decision == LinkUpdateDecision::Always {
                    self.set_file_operation_config(
                        "alwaysUpdateLinks",
                        serde_json::Value::Bool(true),
                    );
                }
                let loaded = self.file_operation_loaded_paths();
                let edits = self
                    .vault
                    .apply_rename_plan_with_loaded_files(active.plan, &loaded)?;
                if request.from != request.physical_to {
                    for record in &edits {
                        self.apply_saved_source_edits(&record.path, &record.before, &record.edits)?;
                        self.synchronize_file_modified(&record.path);
                    }
                    let raw_modified = self.complete_raw_link_update(
                        active.raw_plan,
                        &request.from,
                        &request.physical_to,
                    )?;
                    for record in &edits {
                        if !raw_modified.contains(&record.path) {
                            self.file_modified(&record.path);
                        }
                    }
                }
            }
        }
        if request
            .raw_to
            .split('/')
            .any(|part| part == "." || part == "..")
        {
            self.refresh_vault()?;
        }
        self.persist_session()?;
        Ok(())
    }
}
