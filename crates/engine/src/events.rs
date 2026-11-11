//! Vault operation notifications and an opt-in frozen-payload collector.
//!
//! The native four-operation fixture establishes create/modify/rename/delete
//! names and path/kind/old_path projections. This is not a filesystem-difference
//! detector, complete mutable TFile payload, watcher or SDK listener scheduler.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventName {
    Create,
    Modify,
    Rename,
    Delete,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileKind {
    File,
    Folder,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultEvent {
    /// Internal collector cursor; not a plugin API or persisted session field.
    pub sequence: u64,
    pub name: EventName,
    pub path: String,
    pub kind: FileKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EventJournalError {
    #[error("Vault event collection is disabled")]
    Disabled,
    #[error("Lifecycle event collection is disabled")]
    LifecycleDisabled,
    #[error("Event cursor {requested} precedes this collector's start {start}")]
    BeforeStart { requested: u64, start: u64 },
    #[error("Event cursor {requested} is beyond current cursor {current}")]
    FutureCursor { requested: u64, current: u64 },
    #[error("Internal vault event cursor exhausted")]
    CursorExhausted,
}
#[derive(Debug, Default)]
pub(crate) struct EventJournal {
    cursor: u64,
    start: u64,
    records: Option<Vec<VaultEvent>>,
    exhausted: bool,
}
impl EventJournal {
    pub fn start(&mut self) -> u64 {
        self.records = Some(Vec::new());
        self.start = self.cursor;
        self.cursor
    }
    pub fn stop(&mut self) {
        self.records = None;
    }
    pub fn cursor(&self) -> u64 {
        self.cursor
    }
    pub fn since(&self, cursor: u64) -> Result<&[VaultEvent], EventJournalError> {
        if self.exhausted {
            return Err(EventJournalError::CursorExhausted);
        }
        let records = self.records.as_deref().ok_or(EventJournalError::Disabled)?;
        if cursor < self.start {
            return Err(EventJournalError::BeforeStart {
                requested: cursor,
                start: self.start,
            });
        }
        if cursor > self.cursor {
            return Err(EventJournalError::FutureCursor {
                requested: cursor,
                current: self.cursor,
            });
        }
        Ok(&records[records.partition_point(|event| event.sequence <= cursor)..])
    }
    pub fn emit(
        &mut self,
        name: EventName,
        path: &str,
        kind: FileKind,
        old_path: Option<&str>,
    ) -> Result<(), EventJournalError> {
        let Some(next) = self.cursor.checked_add(1) else {
            self.exhausted = true;
            return Err(EventJournalError::CursorExhausted);
        };
        self.cursor = next;
        if let Some(records) = &mut self.records {
            records.push(VaultEvent {
                sequence: next,
                name,
                path: path.into(),
                kind,
                old_path: old_path.map(str::to_owned),
            });
        }
        Ok(())
    }
}
/// Native metadata notification names. These are emitted by owner transitions,
/// independently of whether an observer is collecting callbacks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MetadataEventName {
    Changed,
    Resolve,
    Resolved,
    Deleted,
    Finished,
}
impl MetadataEventName {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Changed => "changed",
            Self::Resolve => "resolve",
            Self::Resolved => "resolved",
            Self::Deleted => "deleted",
            Self::Finished => "finished",
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetadataEvent {
    Changed {
        path: String,
        data: String,
        cache_json: String,
    },
    Resolve {
        path: String,
    },
    Resolved,
    Deleted {
        path: String,
        cache_json: Option<String>,
    },
    Finished,
}
impl MetadataEvent {
    pub fn name(&self) -> MetadataEventName {
        match self {
            Self::Changed { .. } => MetadataEventName::Changed,
            Self::Resolve { .. } => MetadataEventName::Resolve,
            Self::Resolved => MetadataEventName::Resolved,
            Self::Deleted { .. } => MetadataEventName::Deleted,
            Self::Finished => MetadataEventName::Finished,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LifecycleDomain {
    Vault,
    Metadata,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetadataReadiness {
    pub initialized: bool,
    pub is_cache_clean: bool,
    pub in_progress: usize,
    pub queue_length: usize,
    pub resolver_running: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetadataFileInfo {
    pub mtime: i64,
    pub size: u64,
    pub hash: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetadataFileStat {
    pub ctime: i64,
    pub mtime: i64,
    pub size: u64,
}
pub type MetadataLinkMap = indexmap::IndexMap<String, indexmap::IndexMap<String, usize>>;
/// Frozen state at a genuine callback. Cache JSON strings preserve every UTF16
/// unit and do not project rich values through serde_json::Value.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallbackFrame {
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub preload_promise_present: bool,
    #[serde(default)]
    pub layout_ready: bool,
    pub cache_json: BTreeMap<String, Option<String>>,
    pub file_info: BTreeMap<String, Option<MetadataFileInfo>>,
    pub file_stat: BTreeMap<String, Option<MetadataFileStat>>,
    pub info_stat_matches: BTreeMap<String, Option<bool>>,
    pub resolved: MetadataLinkMap,
    pub unresolved: MetadataLinkMap,
    pub readiness: MetadataReadiness,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleEvent {
    pub sequence: u64,
    pub domain: LifecycleDomain,
    pub name: String,
    pub path: Option<String>,
    pub old_path: Option<String>,
    pub data: Option<String>,
    pub cache_argument_json: Option<String>,
    pub state: CallbackFrame,
}
impl LifecycleEvent {
    pub fn qualified_name(&self) -> String {
        format!(
            "{}:{}",
            match self.domain {
                LifecycleDomain::Vault => "vault",
                LifecycleDomain::Metadata => "metadata",
            },
            self.name
        )
    }
}
/// Separate collector/cursor preserves the established Vault-only API. Turning
/// collection on changes storage only, never task scheduling or owner commits.
#[derive(Debug, Default)]
pub(crate) struct LifecycleJournal {
    cursor: u64,
    start: u64,
    records: Option<Vec<LifecycleEvent>>,
    exhausted: bool,
}
impl LifecycleJournal {
    pub fn enabled(&self) -> bool {
        self.records.is_some()
    }
    pub fn start(&mut self) -> u64 {
        self.records = Some(Vec::new());
        self.start = self.cursor;
        self.cursor
    }
    pub fn stop(&mut self) {
        self.records = None;
    }
    pub fn cursor(&self) -> u64 {
        self.cursor
    }
    pub fn since(&self, cursor: u64) -> Result<&[LifecycleEvent], EventJournalError> {
        if self.exhausted {
            return Err(EventJournalError::CursorExhausted);
        }
        let records = self
            .records
            .as_deref()
            .ok_or(EventJournalError::LifecycleDisabled)?;
        if cursor < self.start {
            return Err(EventJournalError::BeforeStart {
                requested: cursor,
                start: self.start,
            });
        }
        if cursor > self.cursor {
            return Err(EventJournalError::FutureCursor {
                requested: cursor,
                current: self.cursor,
            });
        }
        Ok(&records[records.partition_point(|event| event.sequence <= cursor)..])
    }
    pub fn emit_vault(
        &mut self,
        name: EventName,
        path: &str,
        _kind: FileKind,
        old_path: Option<&str>,
        state: CallbackFrame,
    ) -> Result<(), EventJournalError> {
        self.emit(LifecycleEvent {
            sequence: 0,
            domain: LifecycleDomain::Vault,
            name: match name {
                EventName::Create => "create",
                EventName::Modify => "modify",
                EventName::Rename => "rename",
                EventName::Delete => "delete",
            }
            .into(),
            path: Some(path.into()),
            old_path: old_path.map(str::to_owned),
            data: None,
            cache_argument_json: None,
            state,
        })
    }
    pub fn emit_metadata(
        &mut self,
        event: MetadataEvent,
        state: CallbackFrame,
    ) -> Result<(), EventJournalError> {
        let name = event.name().as_str().to_owned();
        let (path, data, cache_argument_json) = match event {
            MetadataEvent::Changed {
                path,
                data,
                cache_json,
            } => (Some(path), Some(data), Some(cache_json)),
            MetadataEvent::Resolve { path } => (Some(path), None, None),
            MetadataEvent::Deleted { path, cache_json } => (Some(path), None, cache_json),
            MetadataEvent::Resolved | MetadataEvent::Finished => (None, None, None),
        };
        self.emit(LifecycleEvent {
            sequence: 0,
            domain: LifecycleDomain::Metadata,
            name,
            path,
            old_path: None,
            data,
            cache_argument_json,
            state,
        })
    }
    pub fn emit_external_watch(
        &mut self,
        path: &str,
        state: CallbackFrame,
    ) -> Result<(), EventJournalError> {
        self.emit(LifecycleEvent {
            sequence: 0,
            domain: LifecycleDomain::Vault,
            name: "external-watch".into(),
            path: Some(path.into()),
            old_path: None,
            data: None,
            cache_argument_json: None,
            state,
        })
    }
    fn emit(&mut self, mut event: LifecycleEvent) -> Result<(), EventJournalError> {
        let Some(next) = self.cursor.checked_add(1) else {
            self.exhausted = true;
            return Err(EventJournalError::CursorExhausted);
        };
        self.cursor = next;
        if let Some(records) = &mut self.records {
            event.sequence = next;
            records.push(event);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn opt_in_collection_preserves_cursor_and_rejects_unavailable_history() {
        let mut journal = EventJournal::default();
        assert_eq!(journal.since(0), Err(EventJournalError::Disabled));
        journal
            .emit(EventName::Create, "A.md", FileKind::File, None)
            .unwrap();
        let cursor = journal.start();
        assert_eq!(cursor, 1);
        assert!(journal.since(cursor).unwrap().is_empty());
        journal
            .emit(EventName::Rename, "B.md", FileKind::File, Some("A.md"))
            .unwrap();
        let retained = journal.since(cursor).unwrap()[0].clone();
        let reset = journal.start();
        assert_eq!(reset, 2);
        assert!(matches!(
            journal.since(cursor),
            Err(EventJournalError::BeforeStart { .. })
        ));
        assert!(matches!(
            journal.since(3),
            Err(EventJournalError::FutureCursor { .. })
        ));
        journal
            .emit(EventName::Delete, "B.md", FileKind::File, None)
            .unwrap();
        assert_eq!(retained.path, "B.md");
        assert_eq!(retained.old_path.as_deref(), Some("A.md"));
        journal.stop();
        assert_eq!(journal.since(reset), Err(EventJournalError::Disabled));
        assert_eq!(journal.cursor(), 3);
    }
}
