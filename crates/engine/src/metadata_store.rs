//! Private content-addressed metadata persistence. Native storage is IndexedDB;
//! this file implements its accepted-cache/content-reuse behavior, not its format.
use crate::{io, CachedNote, EngineError};
use mnemosyne_core::{cache::CachedMetadata, Note};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
static NEXT_WRITE: AtomicU64 = AtomicU64::new(0);
const SCHEMA_VERSION: u32 = 1;
const PARSER_REVISION: u32 = 1;
#[cfg(test)]
mod gc_tests;
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FileRecord {
    mtime_ms: i64,
    size: u64,
    hash: String,
}
#[derive(Serialize, Deserialize)]
struct Snapshot {
    schema_version: u32,
    parser_revision: u32,
    files: BTreeMap<String, FileRecord>,
    metadata: BTreeMap<String, serde_json::Value>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct RecordId(u64);
#[derive(Debug, Default)]
pub(crate) struct MetadataStore {
    files: BTreeMap<String, RecordId>,
    records: BTreeMap<RecordId, FileRecord>,
    durable_files: BTreeMap<String, FileRecord>,
    next_record: u64,
    metadata: BTreeMap<String, Arc<CachedMetadata>>,
    dirty: bool,
    startup_gc_due: Option<std::time::Instant>,
    periodic_gc_due: Option<std::time::Instant>,
}
impl MetadataStore {
    pub(crate) fn open(root: &Path) -> (Self, Vec<String>) {
        let path = root.join(".mnemosyne/metadata-cache.json");
        let mut diagnostics = Vec::new();
        let mut store = Self {
            dirty: true,
            ..Self::default()
        };
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return (store, diagnostics)
            }
            Err(error) => {
                diagnostics.push(format!("Metadata cache load: {error}"));
                return (store, diagnostics);
            }
        };
        let snapshot: Snapshot = match serde_json::from_slice(&bytes) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                diagnostics.push(format!("Metadata cache load: {error}"));
                return (store, diagnostics);
            }
        };
        if snapshot.schema_version != SCHEMA_VERSION || snapshot.parser_revision != PARSER_REVISION
        {
            diagnostics.push("Metadata cache revision mismatch; reparsing saved files".into());
            return (store, diagnostics);
        }
        for (path, record) in snapshot.files {
            if crate::safe_relative(&path) && (record.hash.is_empty() || valid_hash(&record.hash)) {
                let id = store.allocate_record(record.clone());
                store.files.insert(path.clone(), id);
                store.durable_files.insert(path, record);
            }
        }
        store.dirty = false;
        for (hash, value) in snapshot.metadata {
            if !valid_hash(&hash) {
                store.dirty = true;
                continue;
            }
            // Vm migration runs for every stored metadata record, including
            // unreferenced hashes, and rewrites the raw record when v < 1.
            let migrated = value
                .get("v")
                .and_then(serde_json::Value::as_u64)
                .is_none_or(|version| version < 1);
            match CachedMetadata::decode_persistent(value) {
                Ok(metadata) => {
                    store.dirty |= migrated;
                    store.metadata.insert(hash, Arc::new(metadata));
                }
                Err(error) => {
                    store.dirty = true;
                    diagnostics.push(format!("Metadata cache record: {error}"));
                }
            }
        }
        (store, diagnostics)
    }
    fn allocate_record(&mut self, record: FileRecord) -> RecordId {
        let id = RecordId(self.next_record);
        self.next_record = self
            .next_record
            .checked_add(1)
            .expect("metadata record identity exhausted");
        self.records.insert(id, record);
        id
    }
    fn record(&self, path: &str) -> Option<&FileRecord> {
        self.files.get(path).and_then(|id| self.records.get(id))
    }
    fn save_record(&mut self, path: &str, id: RecordId) {
        self.files.insert(path.into(), id);
        self.durable_files
            .insert(path.into(), self.records[&id].clone());
        self.dirty = true;
    }
    pub(crate) fn file_info(&self, path: &str) -> Option<crate::events::MetadataFileInfo> {
        self.record(path)
            .map(|record| crate::events::MetadataFileInfo {
                mtime: record.mtime_ms,
                size: record.size,
                hash: record.hash.clone(),
            })
    }
    pub(crate) fn metadata_for_path(&self, path: &str) -> Option<&CachedMetadata> {
        self.metadata.get(&self.record(path)?.hash).map(Arc::as_ref)
    }
    pub(crate) fn hydrated_note(&self, path: &str, source: &str) -> Option<CachedNote> {
        let metadata = self.metadata.get(&self.record(path)?.hash)?.clone();
        Some(CachedNote {
            source: Arc::from(source),
            computed_data: None,
            metadata: Some(metadata),
            failure: None,
        })
    }
    pub(crate) fn record_identity(&self, path: &str) -> Option<RecordId> {
        self.files.get(path).copied()
    }
    pub(crate) fn records_share_identity(&self, left: &str, right: &str) -> Option<bool> {
        Some(self.record_identity(left)? == self.record_identity(right)?)
    }
    pub(crate) fn file_paths(&self) -> impl Iterator<Item = &String> {
        self.files.keys()
    }
    pub(crate) fn prepare_path(&mut self, root: &Path, path: &str) -> Result<bool, EngineError> {
        let absolute = root.join(path);
        let stat = std::fs::metadata(&absolute).map_err(|error| io(&absolute, error))?;
        let mtime =
            crate::external::rounded_millis(stat.modified().map_err(|error| io(&absolute, error))?);
        if mnemosyne_core::extension(path) != "md" {
            let id = self.allocate_record(FileRecord {
                mtime_ms: mtime,
                size: stat.len(),
                hash: String::new(),
            });
            self.save_record(path, id);
            return Ok(true);
        }
        if !self.files.contains_key(path) {
            let id = self.allocate_record(FileRecord {
                mtime_ms: 0,
                size: 0,
                hash: String::new(),
            });
            self.save_record(path, id);
        }
        let record = self.record(path).unwrap();
        Ok(record.mtime_ms == mtime
            && record.size == stat.len()
            && self.metadata.contains_key(&record.hash))
    }
    pub(crate) fn rename_path(&mut self, from: &str, to: &str) {
        if from == to {
            return;
        }
        let changed = self
            .files
            .iter()
            .filter_map(|(path, id)| {
                crate::renamed_path(path, from, to).map(|new| (path.clone(), new, *id))
            })
            .collect::<Vec<_>>();
        for (old, new, id) in changed {
            self.save_record(&new, id);
            self.files.remove(&old);
            self.durable_files.remove(&old);
            self.dirty = true;
        }
    }
    pub(crate) fn delete_path(&mut self, path: &str) {
        let prefix = format!("{path}/");
        let before = self.files.len();
        self.files
            .retain(|key, _| key != path && !key.starts_with(&prefix));
        self.durable_files
            .retain(|key, _| key != path && !key.starts_with(&prefix));
        self.dirty |= before != self.files.len();
    }
    pub(crate) fn prune_records(&mut self, pending: &[RecordId]) {
        let mut retained = self
            .files
            .values()
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        retained.extend(pending.iter().copied());
        self.records.retain(|id, _| retained.contains(id));
    }
    pub(crate) fn arm_periodic_gc(&mut self, now: std::time::Instant) {
        self.periodic_gc_due = now.checked_add(std::time::Duration::from_secs(600));
    }
    pub(crate) fn arm_startup_gc(&mut self, now: std::time::Instant) {
        self.startup_gc_due = now.checked_add(std::time::Duration::from_secs(60));
    }
    pub(crate) fn gc_due(&self, now: std::time::Instant) -> bool {
        self.startup_gc_due.is_some_and(|due| due <= now)
            || self.periodic_gc_due.is_some_and(|due| due <= now)
    }
    pub(crate) fn collect_unused(
        &mut self,
        now: std::time::Instant,
        raw_aliases: &[Arc<CachedMetadata>],
    ) -> Vec<String> {
        if self.startup_gc_due.is_some_and(|due| due <= now) {
            self.startup_gc_due = None;
        }
        if let Some(due) = self.periodic_gc_due.filter(|due| *due <= now) {
            let intervals = now.duration_since(due).as_secs() / 600 + 1;
            self.periodic_gc_due = due.checked_add(std::time::Duration::from_secs(
                intervals.saturating_mul(600),
            ));
        }
        let referenced: std::collections::BTreeSet<_> = self
            .files
            .values()
            .filter_map(|id| self.records.get(id))
            .map(|record| record.hash.as_str())
            .collect();
        let mut removed = Vec::new();
        self.metadata.retain(|hash, metadata| {
            let keep = referenced.contains(hash.as_str())
                || raw_aliases.iter().any(|alias| Arc::ptr_eq(alias, metadata));
            if !keep {
                removed.push(hash.clone());
            }
            keep
        });
        self.dirty |= !removed.is_empty();
        removed
    }
    pub(crate) fn cached_note(
        &mut self,
        root: &Path,
        path: &str,
        note: &Note,
    ) -> Result<CachedNote, EngineError> {
        let absolute = root.join(path);
        let stat = std::fs::metadata(&absolute).map_err(|error| io(&absolute, error))?;
        let mtime_ms =
            crate::external::rounded_millis(stat.modified().map_err(|error| io(&absolute, error))?);
        // Native startup trusts this stat tuple and does not rehash the source.
        // The Note may contain different bytes with a deliberately restored stat.
        if let Some(record) = self
            .record(path)
            .filter(|record| record.mtime_ms == mtime_ms && record.size == stat.len())
        {
            if let Some(metadata) = self.metadata.get(&record.hash) {
                return Ok(CachedNote {
                    source: Arc::from(note.source.as_str()),
                    computed_data: None,
                    metadata: Some(metadata.clone()),
                    failure: None,
                });
            }
        }
        let bytes = std::fs::read(&absolute).map_err(|error| io(&absolute, error))?;
        let after = std::fs::metadata(&absolute).map_err(|error| io(&absolute, error))?;
        let after_mtime = crate::external::rounded_millis(
            after.modified().map_err(|error| io(&absolute, error))?,
        );
        if after_mtime != mtime_ms || after.len() != stat.len() || bytes.len() as u64 != stat.len()
        {
            return Err(EngineError::InvalidState(format!(
                "Metadata source changed while reading {path}"
            )));
        }
        let id = self.record_identity(path).unwrap_or_else(|| {
            self.allocate_record(FileRecord {
                mtime_ms: 0,
                size: 0,
                hash: String::new(),
            })
        });
        self.complete_read(path, id, mtime_ms, stat.len(), &bytes, note.source.as_str())
    }
    pub(crate) fn complete_read(
        &mut self,
        original_key: &str,
        id: RecordId,
        mtime: i64,
        size: u64,
        bytes: &[u8],
        snapshot_source: &str,
    ) -> Result<CachedNote, EngineError> {
        let hash = format!("{:x}", Sha256::digest(bytes));
        let record = self
            .records
            .get_mut(&id)
            .expect("queued metadata record retained");
        record.mtime_ms = mtime;
        record.size = size;
        record.hash = hash.clone();
        self.save_record(original_key, id);
        let source = String::from_utf8_lossy(bytes);
        let computed_data: Arc<str> = Arc::from(source.strip_prefix('\u{feff}').unwrap_or(&source));
        if let Some(metadata) = self.metadata.get(&hash) {
            return Ok(CachedNote {
                source: Arc::from(snapshot_source),
                computed_data: Some(computed_data),
                metadata: Some(metadata.clone()),
                failure: None,
            });
        }
        let note = Note::parse(original_key.into(), source.into_owned());
        match CachedMetadata::try_from_note(&note) {
            Ok(metadata) => {
                let metadata = Arc::new(metadata);
                self.metadata.insert(hash, metadata.clone());
                self.dirty = true;
                Ok(CachedNote {
                    source: Arc::from(snapshot_source),
                    computed_data: Some(computed_data),
                    metadata: Some(metadata),
                    failure: None,
                })
            }
            Err(failure) => Ok(CachedNote {
                source: Arc::from(snapshot_source),
                computed_data: Some(computed_data),
                metadata: None,
                failure: Some(Arc::new(failure)),
            }),
        }
    }
    pub(crate) fn retain_paths(&mut self, paths: impl Iterator<Item = String>) {
        let paths: std::collections::BTreeSet<_> = paths.collect();
        let previous = self.files.len();
        self.files.retain(|path, _| paths.contains(path));
        self.durable_files.retain(|path, _| paths.contains(path));
        self.dirty |= self.files.len() != previous;
    }
    pub(crate) fn has_pending_persistence(&self) -> bool {
        self.dirty
    }
    pub(crate) fn persist(&mut self, root: &Path) -> Result<(), EngineError> {
        if !self.dirty {
            return Ok(());
        }
        let metadata = self
            .metadata
            .iter()
            .map(|(hash, metadata)| {
                metadata
                    .encode_persistent()
                    .map(|value| (hash.clone(), value))
            })
            .collect::<Result<BTreeMap<_, _>, _>>()
            .map_err(|error| {
                EngineError::InvalidState(format!("Metadata cache encode: {error}"))
            })?;
        let snapshot = Snapshot {
            schema_version: SCHEMA_VERSION,
            parser_revision: PARSER_REVISION,
            files: self.durable_files.clone(),
            metadata,
        };
        let bytes = serde_json::to_vec(&snapshot)?;
        let directory = root.join(".mnemosyne");
        std::fs::create_dir_all(&directory).map_err(|error| io(&directory, error))?;
        let destination = directory.join("metadata-cache.json");
        let temporary: PathBuf = directory.join(format!(
            "metadata-{}-{}.tmp",
            std::process::id(),
            NEXT_WRITE.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| io(&temporary, error))?;
        use std::io::Write;
        if let Err(error) = file.write_all(&bytes).and_then(|_| file.sync_all()) {
            let _ = std::fs::remove_file(&temporary);
            return Err(io(&temporary, error));
        }
        drop(file);
        if let Err(error) = std::fs::rename(&temporary, &destination) {
            let _ = std::fs::remove_file(&temporary);
            return Err(io(&destination, error));
        }
        self.dirty = false;
        Ok(())
    }
}
fn valid_hash(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
}
