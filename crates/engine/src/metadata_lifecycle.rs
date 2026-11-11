//! Retained metadata work and resolution queues. Collection is an independent observer.
use crate::{events, external, io, metadata_store::RecordId, Engine, EngineError};
use mnemosyne_core::{extension, graph_refs};
use std::{
    collections::{BTreeMap, VecDeque},
    time::{Duration, Instant},
};
#[derive(Debug, Default)]
pub(crate) struct StartupState {
    phase: StartupPhase,
    entries: VecDeque<(String, bool)>,
    loaded_files: Vec<String>,
    pub(crate) preload_promise_present: bool,
    pub(crate) layout_ready: bool,
}
#[derive(Debug, Default, PartialEq, Eq)]
enum StartupPhase {
    #[default]
    Prepared,
    Inventory,
    Preload,
    Initialize,
    Layout,
    Ready,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct FileId(u64);
#[derive(Debug)]
struct FileIdentity {
    path: String,
    stat: events::MetadataFileStat,
}
#[derive(Debug)]
struct MetadataRequest {
    file: FileId,
    original_key: String,
    stat: events::MetadataFileStat,
    record: RecordId,
    read: Option<Vec<u8>>,
}
#[derive(Debug, Default)]
pub(crate) struct MetadataLifecycle {
    pub(crate) retained: bool,
    initialized: bool,
    outside_jobs: VecDeque<(String, external::ExternalStat)>,
    jobs: VecDeque<MetadataRequest>,
    links: VecDeque<Option<FileId>>,
    files: BTreeMap<FileId, FileIdentity>,
    paths: BTreeMap<String, FileId>,
    next_file: u64,
    resolver_failed: bool,
    reclamation_needed: bool,
    running: bool,
    finished_due: Option<Instant>,
    resolved: events::MetadataLinkMap,
    unresolved: events::MetadataLinkMap,
    stats: BTreeMap<String, events::MetadataFileStat>,
}
impl Engine {
    pub(crate) fn ensure_startup_layout(&mut self) -> Result<(), EngineError> {
        self.start_startup_loading()?;
        while !self.poll_startup()? {}
        Ok(())
    }
    pub fn start_startup_loading(&mut self) -> Result<(), EngineError> {
        if self.startup.phase != StartupPhase::Prepared {
            return Ok(());
        }
        self.startup.entries =
            crate::chronology::FolderChronology::startup_entries(&self.vault.root)?;
        self.startup.preload_promise_present = true;
        self.startup.phase = StartupPhase::Inventory;
        Ok(())
    }
    /// Advance actual startup without running queued metadata worker results.
    pub fn poll_startup(&mut self) -> Result<bool, EngineError> {
        match self.startup.phase {
            StartupPhase::Prepared => return Ok(false),
            StartupPhase::Inventory => {
                if let Some((path, folder)) = self.startup.entries.pop_front() {
                    self.vault.load_startup_path_from_disk(&path)?;
                    self.chronology.insert(&path, folder);
                    if !folder {
                        self.update_metadata_stat(&path)?;
                        self.startup.loaded_files.push(path.clone());
                    }
                    self.emit_vault_event(
                        events::EventName::Create,
                        &path,
                        if folder {
                            events::FileKind::Folder
                        } else {
                            events::FileKind::File
                        },
                        None,
                    );
                } else {
                    self.vault.finish_startup_indexes();
                    self.startup.phase = StartupPhase::Preload;
                }
            }
            StartupPhase::Preload => {
                let (store, diagnostics) =
                    crate::metadata_store::MetadataStore::open(&self.vault.root);
                self.metadata_store = store;
                self.config_diagnostics.extend(diagnostics);
                self.startup.phase = StartupPhase::Initialize;
            }
            StartupPhase::Initialize => {
                self.startup.preload_promise_present = false;
                self.metadata_store.arm_periodic_gc(Instant::now());
                let stored = self
                    .metadata_store
                    .file_paths()
                    .cloned()
                    .collect::<Vec<_>>();
                self.session.metadata_file_order = stored.clone();
                for path in stored {
                    if !self.vault.has_file(&path) {
                        self.metadata_store.delete_path(&path);
                        self.session.metadata_file_order.retain(|key| key != &path);
                        self.queue_resolution(None);
                        if self.metadata_lifecycle.jobs.is_empty() {
                            self.metadata_lifecycle.finished_due =
                                Instant::now().checked_add(Duration::from_millis(10));
                        }
                        continue;
                    }
                    if let Some(note) = self.vault.get(&path) {
                        if let Some(cached) = self.metadata_store.hydrated_note(&path, &note.source)
                        {
                            self.cached_notes.insert(path.clone(), cached);
                        }
                    }
                    self.stage_metadata_compute(&path)?;
                }
                let loaded = self.startup.loaded_files.clone();
                for path in loaded {
                    if self.metadata_store.file_info(&path).is_none() {
                        self.stage_metadata_compute(&path)?;
                    }
                }
                self.reload_property_types();
                self.metadata_lifecycle.initialized = true;
                self.metadata_store.arm_startup_gc(Instant::now());
                self.emit_metadata(events::MetadataEvent::Finished);
                self.metadata_lifecycle.reclamation_needed = true;
                self.prune_metadata_identities();
                self.startup.phase = StartupPhase::Layout;
            }
            StartupPhase::Layout => {
                while !self.metadata_lifecycle.links.is_empty()
                    && !self.metadata_lifecycle.resolver_failed
                {
                    self.poll_metadata_resolution();
                }
                self.finish_startup_layout()?;
                self.startup.layout_ready = true;
                self.startup.phase = StartupPhase::Ready;
            }
            StartupPhase::Ready => return Ok(true),
        }
        Ok(self.startup.phase == StartupPhase::Ready)
    }
    pub(crate) fn drain_metadata_startup(&mut self) -> Result<(), EngineError> {
        self.drain_metadata_for_shutdown()?;
        if let Some(due) = self.metadata_lifecycle.finished_due {
            if let Some(wait) = due.checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            }
            self.poll_metadata_lifecycle()?;
        }
        Ok(())
    }
    pub fn enable_metadata_lifecycle(&mut self) -> Result<(), EngineError> {
        self.metadata_lifecycle.retained = true;
        Ok(())
    }
    pub(crate) fn metadata_job_pending(&self, path: &str) -> bool {
        self.metadata_lifecycle.jobs.iter().any(|job| {
            self.metadata_lifecycle
                .files
                .get(&job.file)
                .is_some_and(|file| file.path == path)
        })
    }
    pub fn metadata_lifecycle_enabled(&self) -> bool {
        self.metadata_lifecycle.retained
    }
    pub fn metadata_readiness(&self) -> events::MetadataReadiness {
        let owner = &self.metadata_lifecycle;
        events::MetadataReadiness {
            initialized: owner.initialized,
            in_progress: owner.jobs.len(),
            queue_length: owner.links.len(),
            resolver_running: owner.running,
            is_cache_clean: owner.jobs.is_empty() && owner.links.is_empty() && !owner.running,
        }
    }
    pub fn start_lifecycle_journal(&mut self) -> u64 {
        self.lifecycle_journal_precedes_metadata = !self.metadata_lifecycle.initialized;
        self.lifecycle_journal.start()
    }
    pub fn stop_lifecycle_journal(&mut self) {
        self.lifecycle_journal.stop();
    }
    pub fn lifecycle_cursor(&self) -> u64 {
        self.lifecycle_journal.cursor()
    }
    pub fn lifecycle_journal_since(
        &self,
        cursor: u64,
    ) -> Result<&[events::LifecycleEvent], events::EventJournalError> {
        self.lifecycle_journal.since(cursor)
    }
    pub(crate) fn retained_vault_file_stats(&self) -> &BTreeMap<String, events::MetadataFileStat> {
        &self.metadata_lifecycle.stats
    }
    pub(crate) fn published_metadata_cache(
        &self,
        path: &str,
    ) -> Option<&mnemosyne_core::cache::CachedMetadata> {
        if external::real_path(path).is_some() {
            return self.external_files.get(path)?.metadata.as_ref();
        }
        if let Some(cached) = self.raw_file_cache.get(path) {
            return cached.as_ref()?.metadata.as_deref();
        }
        self.metadata_store
            .metadata_for_path(path)
            .or_else(|| self.non_markdown_caches.get(path))
    }
    pub fn metadata_lifecycle_snapshot(&self) -> events::CallbackFrame {
        let mut frame = events::CallbackFrame {
            readiness: self.metadata_readiness(),
            files: self
                .chronology
                .paths()
                .into_iter()
                .map(str::to_owned)
                .collect(),
            preload_promise_present: self.startup.preload_promise_present,
            layout_ready: self.startup.layout_ready,
            resolved: self.metadata_lifecycle.resolved.clone(),
            unresolved: self.metadata_lifecycle.unresolved.clone(),
            ..Default::default()
        };
        let mut paths = std::collections::BTreeSet::new();
        paths.extend(self.chronology.paths().into_iter().map(str::to_owned));
        paths.extend(self.metadata_store.file_paths().cloned());
        paths.extend(self.external_files.keys().cloned());
        for path in paths {
            frame.cache_json.insert(
                path.clone(),
                self.published_metadata_cache(&path)
                    .and_then(|cache| serde_json::to_string(cache).ok()),
            );
            let info = self.metadata_store.file_info(&path);
            let stat = if let Some(file) = self.external_files.get(&path) {
                Some(events::MetadataFileStat {
                    ctime: file.stat.ctime,
                    mtime: file.stat.mtime,
                    size: file.stat.size,
                })
            } else if self.vault.has_file(&path) {
                self.metadata_lifecycle.stats.get(&path).cloned()
            } else {
                None
            };
            frame.info_stat_matches.insert(
                path.clone(),
                info.as_ref()
                    .zip(stat.as_ref())
                    .map(|(info, stat)| info.mtime == stat.mtime && info.size == stat.size),
            );
            frame.file_info.insert(path.clone(), info);
            frame.file_stat.insert(path, stat);
        }
        frame
    }
    fn emit_metadata(&mut self, event: events::MetadataEvent) {
        if !self.lifecycle_journal.enabled() {
            return;
        }
        let frame = self.metadata_lifecycle_snapshot();
        if let Err(error) = self.lifecycle_journal.emit_metadata(event, frame) {
            self.config_diagnostics.push(error.to_string());
        }
    }
    pub fn metadata_file_records_share_identity(&self, left: &str, right: &str) -> Option<bool> {
        self.metadata_store.records_share_identity(left, right)
    }
    pub(crate) fn file_operation_loaded_paths(&self) -> std::collections::BTreeSet<String> {
        self.metadata_lifecycle
            .paths
            .keys()
            .map(|path| {
                self.raw_file_destinations
                    .get(path)
                    .cloned()
                    .unwrap_or_else(|| path.clone())
            })
            .chain(self.vault.files().iter().cloned())
            .collect()
    }
    pub(crate) fn file_operation_identity(&self, path: &str) -> Option<FileId> {
        self.metadata_lifecycle.paths.get(path).copied()
    }
    pub(crate) fn file_operation_path(&self, id: FileId) -> Option<&str> {
        let file = self.metadata_lifecycle.files.get(&id)?;
        (self.metadata_lifecycle.paths.get(&file.path) == Some(&id)).then_some(file.path.as_str())
    }
    pub(crate) fn update_metadata_stat(&mut self, path: &str) -> Result<(), EngineError> {
        let absolute = self.vault.root.join(path);
        let stat = std::fs::symlink_metadata(&absolute).map_err(|error| io(&absolute, error))?;
        if stat.is_file() {
            let stat = external::ExternalStat::from_metadata(&stat)
                .map_err(|error| io(&absolute, error))?;
            let stat = events::MetadataFileStat {
                ctime: stat.ctime,
                mtime: stat.mtime,
                size: stat.size,
            };
            self.metadata_lifecycle
                .stats
                .insert(path.into(), stat.clone());
            if let Some(id) = self.metadata_lifecycle.paths.get(path).copied() {
                self.metadata_lifecycle.files.get_mut(&id).unwrap().stat = stat;
            } else {
                let id = FileId(self.metadata_lifecycle.next_file);
                self.metadata_lifecycle.next_file = self
                    .metadata_lifecycle
                    .next_file
                    .checked_add(1)
                    .expect("metadata file identity exhausted");
                self.metadata_lifecycle.files.insert(
                    id,
                    FileIdentity {
                        path: path.into(),
                        stat,
                    },
                );
                self.metadata_lifecycle.paths.insert(path.into(), id);
            }
        }
        Ok(())
    }
    fn queue_resolution(&mut self, path: Option<String>) {
        let identity = match path {
            Some(path) => match self.metadata_lifecycle.paths.get(&path).copied() {
                Some(id) => Some(id),
                None => return,
            },
            None => None,
        };
        self.queue_identity_resolution(identity);
    }
    fn queue_identity_resolution(&mut self, file: Option<FileId>) {
        if !self.metadata_lifecycle.links.contains(&file) {
            self.metadata_lifecycle.links.push_back(file);
        }
    }
    fn queue_related(&mut self, path: &str) {
        // Native updateRelatedLinks compares basename keys in the published
        // graphs. A failed worker's stale graph remains authoritative here.
        let name = std::path::Path::new(path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(path)
            .to_lowercase();
        let mut unresolved_names = vec![name.clone()];
        if let Some(stem) = name.strip_suffix(".md") {
            unresolved_names.push(stem.into());
        }
        let related = self
            .cached_file_paths()
            .into_iter()
            .filter(|source| {
                let matches = |map: Option<&indexmap::IndexMap<String, usize>>,
                               names: &[String]| {
                    map.is_some_and(|map| {
                        map.keys().any(|key| {
                            let base = std::path::Path::new(key)
                                .file_name()
                                .and_then(|name| name.to_str())
                                .unwrap_or(key)
                                .to_lowercase();
                            names.contains(&base)
                        })
                    })
                };
                matches(
                    self.metadata_lifecycle.resolved.get(*source),
                    std::slice::from_ref(&name),
                ) || matches(
                    self.metadata_lifecycle.unresolved.get(*source),
                    &unresolved_names,
                )
            })
            .map(str::to_owned)
            .collect::<Vec<_>>();
        for source in related {
            self.queue_resolution(Some(source));
        }
    }
    pub(crate) fn stage_metadata_compute(&mut self, path: &str) -> Result<(), EngineError> {
        if !self.metadata_lifecycle.paths.contains_key(path) {
            self.update_metadata_stat(path)?;
        }
        let inserted = self.metadata_store.file_info(path).is_none();
        let fast = self.metadata_store.prepare_path(&self.vault.root, path)?;
        if inserted {
            self.append_metadata_cache_key(path);
        }
        if extension(path) != "md" {
            self.metadata_lifecycle.reclamation_needed = true;
            self.non_markdown_caches
                .insert(path.into(), Default::default());
            return Ok(());
        }
        let file = self.metadata_lifecycle.paths[path];
        if fast {
            self.queue_identity_resolution(Some(file));
        } else {
            let stat = self.metadata_lifecycle.files[&file].stat.clone();
            let record = self.metadata_store.record_identity(path).unwrap();
            self.metadata_lifecycle.jobs.push_back(MetadataRequest {
                file,
                original_key: path.into(),
                stat,
                record,
                read: None,
            });
        }
        Ok(())
    }
    pub fn compute_file_metadata(&mut self, path: &str) -> Result<(), EngineError> {
        self.stage_metadata_compute(path)
    }
    fn resolve_metadata_path(&mut self, path: &str) {
        let Some(cache) = self.file_cache(path) else {
            return;
        };
        let mut resolved = indexmap::IndexMap::new();
        let mut unresolved = indexmap::IndexMap::new();
        for link in graph_refs::references(cache) {
            let target = graph_refs::link_path(link);
            if let Some(destination) = self.resolve_cached_link(path, target) {
                *resolved.entry(destination).or_insert(0) += 1;
            } else {
                let target = if extension(target) == "md" {
                    &target[..target.len() - 3]
                } else {
                    target
                };
                *unresolved.entry(target.to_owned()).or_insert(0) += 1;
            }
        }
        self.metadata_lifecycle
            .resolved
            .insert(path.into(), resolved);
        self.metadata_lifecycle
            .unresolved
            .insert(path.into(), unresolved);
    }
    fn advance_resolution(&mut self) -> usize {
        if self.metadata_lifecycle.resolver_failed || self.metadata_lifecycle.links.is_empty() {
            return 0;
        }
        self.metadata_lifecycle.running = true;
        let mut count = 0;
        // The native async generator yields between resolver entries. Keep
        // running true while another real entry is waiting for the next pump.
        if let Some(file) = self.metadata_lifecycle.links.pop_front() {
            if let Some(file) = file {
                self.metadata_lifecycle.reclamation_needed = true;
                let path = self.metadata_lifecycle.files[&file].path.clone();
                if self.published_metadata_cache(&path).is_some()
                    && !self.vault.has_file(&path)
                    && !self.raw_file_cache.contains_key(&path)
                {
                    // Native resolveLinks passes a missing loaded file to iterateRefsForFile.
                    // Its consumer exits while the runnable remains running (CACHE04 row5).
                    self.metadata_lifecycle.resolver_failed = true;
                    self.config_diagnostics
                        .push(format!("Metadata resolution has no loaded file: {path}"));
                    return count;
                }
                self.resolve_metadata_path(&path);
                self.emit_metadata(events::MetadataEvent::Resolve { path });
                count += 1;
            }
        }
        if self.metadata_lifecycle.links.is_empty() {
            self.metadata_lifecycle.running = false;
            self.emit_metadata(events::MetadataEvent::Resolved);
            count += 1;
        }
        count
    }
    /// Advance one real link-resolver entry without delivering worker results.
    pub fn poll_metadata_resolution(&mut self) -> usize {
        self.advance_resolution()
    }
    /// Commit already-requested per-key snapshots; this does not run metadata work.
    pub fn poll_metadata_persistence(&mut self) -> Result<bool, EngineError> {
        let pending = self.metadata_store.has_pending_persistence();
        if let Err(error) = self.metadata_store.persist(&self.vault.root) {
            self.config_diagnostics
                .push(format!("Metadata cache save: {error}"));
            return Err(error);
        }
        Ok(pending)
    }
    pub fn poll_metadata_lifecycle(&mut self) -> Result<usize, EngineError> {
        self.poll_metadata_lifecycle_at(Instant::now())
    }
    pub fn poll_metadata_lifecycle_at(&mut self, now: Instant) -> Result<usize, EngineError> {
        let mut count = self.poll_metadata_resolution();
        while let Some((path, expected_stat)) = self.metadata_lifecycle.outside_jobs.pop_front() {
            if let Some(file) = self
                .external_files
                .get_mut(&path)
                .filter(|file| file.users > 0 && file.stat == expected_stat)
            {
                let source = crate::read_text(&file.real_path)?;
                let actual = std::fs::symlink_metadata(&file.real_path)
                    .map_err(|error| io(&file.real_path, error))?;
                if external::ExternalStat::from_metadata(&actual)
                    .map_err(|error| io(&file.real_path, error))?
                    != expected_stat
                {
                    continue;
                }
                external::update_metadata(file, &source);
                if let Some(cache) = file.metadata.as_ref() {
                    let cache_json = serde_json::to_string(cache)?;
                    self.emit_metadata(events::MetadataEvent::Changed {
                        path,
                        data: source
                            .strip_prefix('\u{feff}')
                            .unwrap_or(&source)
                            .to_owned(),
                        cache_json,
                    });
                    count += 1;
                }
            }
        }
        // One production read/completion boundary; the front request remains a
        // counted serial task until its exact captured buffer is published.
        if self
            .metadata_lifecycle
            .jobs
            .front()
            .is_some_and(|job| job.read.is_some())
        {
            let job = self.metadata_lifecycle.jobs.pop_front().unwrap();
            self.metadata_lifecycle.reclamation_needed = true;
            let path = self.metadata_lifecycle.files[&job.file].path.clone();
            let bytes = job.read.as_ref().unwrap();
            let source = self
                .vault
                .get(&path)
                .map(|note| note.source.clone())
                .unwrap_or_else(|| String::from_utf8_lossy(bytes).into_owned());
            match self.metadata_store.complete_read(
                &job.original_key,
                job.record,
                job.stat.mtime,
                job.stat.size,
                bytes,
                &source,
            ) {
                Ok(cached) => {
                    let payload = cached.metadata.clone();
                    let success = payload.is_some();
                    let data = cached.computed_data.as_ref().unwrap().to_string();
                    self.cached_notes.insert(path.clone(), cached);
                    self.append_metadata_cache_key(&job.original_key);
                    if success {
                        if let Err(error) = self.apply_metadata_changed_event(&path) {
                            self.config_diagnostics
                                .push(format!("Metadata type changed: {error}"));
                        }
                        self.queue_identity_resolution(Some(job.file));
                        // Keep the completing request in the count through changed.
                        self.metadata_lifecycle.jobs.push_front(job);
                        if self.lifecycle_journal.enabled() {
                            match serde_json::to_string(payload.as_deref().unwrap()) {
                                Ok(cache_json) => {
                                    self.emit_metadata(events::MetadataEvent::Changed {
                                        path: path.clone(),
                                        data,
                                        cache_json,
                                    })
                                }
                                Err(error) => self
                                    .config_diagnostics
                                    .push(format!("Metadata changed payload {path}: {error}")),
                            }
                        }
                        self.metadata_lifecycle.jobs.pop_front();
                        count += 1;
                    } else {
                        self.recount_metadata_properties();
                    }
                }
                Err(error) => self
                    .config_diagnostics
                    .push(format!("Metadata work {path}: {error}")),
            }
            if self.metadata_lifecycle.jobs.is_empty() {
                self.metadata_lifecycle.finished_due = now.checked_add(Duration::from_millis(10));
            }
            count += self.advance_resolution();
        } else if let Some(job) = self.metadata_lifecycle.jobs.front() {
            let path = self.metadata_lifecycle.files[&job.file].path.clone();
            let absolute = self.vault.root.join(&path);
            match std::fs::read(&absolute) {
                Ok(bytes) => self.metadata_lifecycle.jobs.front_mut().unwrap().read = Some(bytes),
                Err(error) => {
                    self.config_diagnostics
                        .push(format!("Metadata work {path}: {}", io(&absolute, error)));
                    self.metadata_lifecycle.jobs.pop_front();
                    self.metadata_lifecycle.reclamation_needed = true;
                    if self.metadata_lifecycle.jobs.is_empty() {
                        self.metadata_lifecycle.finished_due =
                            now.checked_add(Duration::from_millis(10));
                    }
                }
            }
        }
        // In the captured small-queue branch, runnable resolver Promise
        // continuations (including the null sentinel) precede the finished
        // timer task. A failed consumer cannot run those continuations and
        // must not suppress finished merely because running remains true.
        if self
            .metadata_lifecycle
            .finished_due
            .is_some_and(|due| due <= now)
            && (self.metadata_lifecycle.links.is_empty() || self.metadata_lifecycle.resolver_failed)
        {
            self.metadata_lifecycle.finished_due = None;
            self.emit_metadata(events::MetadataEvent::Finished);
            count += 1;
        }
        let _ = self.poll_metadata_persistence();
        self.prune_metadata_identities();
        Ok(count)
    }
    fn recount_metadata_properties(&mut self) {
        let mut manager = std::mem::take(&mut self.property_types);
        let outcome = manager.recount(
            self.cached_file_paths()
                .into_iter()
                .filter(|path| !self.is_user_ignored(path))
                .filter_map(|path| self.file_cache(path)),
        );
        self.property_types = manager;
        if let Err(error) = outcome {
            self.config_diagnostics
                .push(format!("Metadata type occurrences: {error}"));
        }
    }
    pub(crate) fn queue_metadata_related(&mut self, path: &str) {
        self.queue_related(path);
    }
    pub(crate) fn rename_loaded_metadata_files(&mut self, from: &str, to: &str) {
        let moved = self
            .metadata_lifecycle
            .paths
            .iter()
            .filter_map(|(path, id)| {
                crate::renamed_path(path, from, to).map(|new| (path.clone(), new, *id))
            })
            .collect::<Vec<_>>();
        for (old, new, id) in moved {
            self.metadata_lifecycle.paths.remove(&old);
            self.metadata_lifecycle.paths.insert(new.clone(), id);
            self.metadata_lifecycle.files.get_mut(&id).unwrap().path = new;
        }
        self.metadata_lifecycle.stats = std::mem::take(&mut self.metadata_lifecycle.stats)
            .into_iter()
            .map(|(path, stat)| (crate::renamed_path(&path, from, to).unwrap_or(path), stat))
            .collect();
    }
    pub(crate) fn metadata_renamed(&mut self, from: &str, to: &str) {
        self.metadata_lifecycle.reclamation_needed = true;
        let changed_paths = self
            .metadata_store
            .file_paths()
            .filter_map(|path| {
                crate::renamed_path(path, from, to).map(|renamed| (path.clone(), renamed))
            })
            .collect::<Vec<_>>();
        self.metadata_store.rename_path(from, to);
        self.metadata_lifecycle.resolved = std::mem::take(&mut self.metadata_lifecycle.resolved)
            .into_iter()
            .map(|(path, map)| (crate::renamed_path(&path, from, to).unwrap_or(path), map))
            .collect();
        self.metadata_lifecycle.unresolved =
            std::mem::take(&mut self.metadata_lifecycle.unresolved)
                .into_iter()
                .map(|(path, map)| (crate::renamed_path(&path, from, to).unwrap_or(path), map))
                .collect();
        for (old, new) in changed_paths {
            self.queue_related(&old);
            self.queue_related(&new);
        }
        self.queue_related(from);
        self.queue_related(to);
        self.queue_resolution(None);
    }
    pub(crate) fn metadata_deleted(&mut self, path: &str) {
        self.metadata_lifecycle.reclamation_needed = true;
        let cache_json = self
            .published_metadata_cache(path)
            .and_then(|cache| serde_json::to_string(cache).ok());
        self.emit_metadata(events::MetadataEvent::Deleted {
            path: path.into(),
            cache_json,
        });
        let prefix = format!("{path}/");
        let descendants = self
            .metadata_store
            .file_paths()
            .filter(|key| key.starts_with(&prefix))
            .cloned()
            .collect::<Vec<_>>();
        for descendant in descendants {
            self.queue_related(&descendant);
        }
        self.queue_related(path);
        self.queue_resolution(None);
        self.metadata_store.delete_path(path);
        self.metadata_lifecycle
            .paths
            .retain(|key, _| key != path && !key.starts_with(&prefix));
        self.cached_notes
            .retain(|key, _| key != path && !key.starts_with(&prefix));
        self.non_markdown_caches
            .retain(|key, _| key != path && !key.starts_with(&prefix));
        self.recount_metadata_properties();
        self.metadata_lifecycle
            .stats
            .retain(|key, _| key != path && !key.starts_with(&prefix));
        self.metadata_lifecycle
            .resolved
            .retain(|key, _| key != path && !key.starts_with(&prefix));
        self.metadata_lifecycle
            .unresolved
            .retain(|key, _| key != path && !key.starts_with(&prefix));
        self.metadata_lifecycle.finished_due =
            Instant::now().checked_add(Duration::from_millis(10));
    }
    pub(crate) fn stage_standalone_rename_rewrites(&mut self) {
        if self.metadata_lifecycle.retained {
            return;
        }
        // Core's high-level rename writes referencing notes itself. Retained
        // callers notify each rewrite separately; the synchronous adapter must
        // also publish these real changed inputs before returning.
        let changed = self
            .vault
            .notes
            .iter()
            .filter(|(path, note)| {
                self.cached_notes
                    .get(*path)
                    .is_some_and(|cached| cached.source.as_ref() != note.source)
            })
            .map(|(path, _)| path.clone())
            .collect::<Vec<_>>();
        for path in changed {
            if let Err(error) = self.stage_metadata_compute(&path) {
                self.config_diagnostics.push(error.to_string());
            }
        }
    }
    pub(crate) fn drain_standalone_metadata(&mut self) {
        if !self.metadata_lifecycle.retained {
            while !self.metadata_lifecycle.jobs.is_empty() {
                if let Err(error) = self.poll_metadata_lifecycle() {
                    self.config_diagnostics.push(error.to_string());
                    break;
                }
            }
            let _ = self.poll_metadata_lifecycle();
        }
    }
    fn append_metadata_cache_key(&mut self, path: &str) {
        if !self
            .session
            .metadata_file_order
            .iter()
            .any(|key| key == path)
        {
            self.session.metadata_file_order.push(path.into());
        }
    }
    pub(crate) fn request_metadata_reclamation(&mut self) {
        self.metadata_lifecycle.reclamation_needed = true;
    }
    fn prune_metadata_identities(&mut self) {
        if !std::mem::take(&mut self.metadata_lifecycle.reclamation_needed) {
            return;
        }
        let mut retained = self
            .metadata_lifecycle
            .paths
            .values()
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        retained.extend(self.metadata_lifecycle.jobs.iter().map(|job| job.file));
        retained.extend(self.metadata_lifecycle.links.iter().flatten().copied());
        retained.extend(self.file_operations.retained_file_ids());
        self.metadata_lifecycle
            .files
            .retain(|id, _| retained.contains(id));
        let records = self
            .metadata_lifecycle
            .jobs
            .iter()
            .map(|job| job.record)
            .collect::<Vec<_>>();
        self.metadata_store.prune_records(&records);
    }
    pub(crate) fn drain_metadata_for_shutdown(&mut self) -> Result<(), EngineError> {
        while !self.metadata_lifecycle.jobs.is_empty()
            || (!self.metadata_lifecycle.links.is_empty()
                && !self.metadata_lifecycle.resolver_failed)
        {
            self.poll_metadata_lifecycle()?;
        }
        self.poll_metadata_lifecycle()?;
        Ok(())
    }
    pub(crate) fn stage_external_metadata(&mut self, path: &str, opened: bool) {
        if opened && self.lifecycle_journal.enabled() {
            let frame = self.metadata_lifecycle_snapshot();
            if let Err(error) = self.lifecycle_journal.emit_external_watch(path, frame) {
                self.config_diagnostics.push(error.to_string());
            }
        }
        if let Some(file) = self.external_files.get(path) {
            self.metadata_lifecycle
                .outside_jobs
                .push_back((path.into(), file.stat.clone()));
        }
    }
    pub(crate) fn external_fs_created(&mut self, path: &str) -> Result<(), EngineError> {
        if external::real_path(path).is_some() {
            return self.external_fs_modified(path);
        }
        if !crate::safe_relative(path) {
            return Ok(());
        }
        self.vault.reconcile_path_from_disk(path)?;
        self.file_created(path);
        Ok(())
    }
    pub(crate) fn external_fs_modified(&mut self, path: &str) -> Result<(), EngineError> {
        if external::real_path(path).is_some() {
            return self.retained_external_modified(path);
        }
        if !crate::safe_relative(path) {
            return Ok(());
        }
        self.vault.reconcile_path_from_disk(path)?;
        self.file_modified(path);
        Ok(())
    }
    pub(crate) fn external_fs_deleted(&mut self, path: &str) -> Result<(), EngineError> {
        if external::real_path(path).is_some() {
            return Ok(());
        }
        if !crate::safe_relative(path) {
            return Ok(());
        }
        // Preserve retained stat/cache until metadata deleted has observed them.
        self.vault.reconcile_path_from_disk(path)?;
        self.file_deleted(path);
        Ok(())
    }
}
