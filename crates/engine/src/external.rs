//! Transient desktop `file:` objects, separate from the vault's indexed files.
//!
//! Recovered nI/KP/GP behavior and native external-graph observations: lstat
//! rejects symlinks; each open view is a user; last-user removal clears metadata;
//! stat-change events reload watched Markdown. Scheduling/fs.watch delivery is
//! deliberately a caller boundary, not an invented background polling interval.
use crate::{
    editor_saved_text, io, read_text, EditorBuffer, Engine, EngineError, FileTarget, OpenOptions,
};
use mnemosyne_core::{cache::CachedMetadata, Note};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalStat {
    pub ctime: i64,
    pub mtime: i64,
    pub size: u64,
}
impl ExternalStat {
    pub fn from_metadata(metadata: &std::fs::Metadata) -> Result<Self, std::io::Error> {
        Ok(Self {
            ctime: rounded_millis(metadata.created()?),
            mtime: rounded_millis(metadata.modified()?),
            size: metadata.len(),
        })
    }
}
pub(crate) fn rounded_millis(value: SystemTime) -> i64 {
    let nanos: i128 = match value.duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_nanos() as i128,
        Err(error) => -(error.duration().as_nanos() as i128),
    };
    // JavaScript Math.round: ties go toward positive infinity, also pre-epoch.
    ((nanos + 500_000).div_euclid(1_000_000)).clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

#[derive(Debug)]
pub struct ExternalFile {
    pub path: String,
    pub real_path: PathBuf,
    pub stat: ExternalStat,
    pub users: usize,
    pub metadata: Option<CachedMetadata>,
    pub metadata_failure: Option<mnemosyne_core::cache::MetadataWorkerFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalOpenResult {
    Opened(String),
    Missing,
    UnsupportedExtension,
    /// A registered native view exists but its external-file backend has not
    /// been implemented here; do not report an unsupported-file notice instead.
    PendingViewType(String),
}

pub(crate) fn real_path(path: &str) -> Option<&Path> {
    path.strip_prefix("file:")
        .map(Path::new)
        .filter(|path| path.is_absolute())
}
pub(crate) fn is_file(path: &str) -> bool {
    real_path(path).is_some_and(|path| std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file()))
}
pub(crate) fn update_metadata(file: &mut ExternalFile, source: &str) {
    // Same binary worker as vault Markdown. CachedMetadata/Note already account
    // for the decoder's one leading BOM; stripping here would do it twice.
    match CachedMetadata::try_from_note(&Note::parse(PathBuf::from(&file.path), source.into())) {
        Ok(metadata) => {
            file.metadata = Some(metadata);
            file.metadata_failure = None;
        }
        Err(failure) => {
            file.metadata = None;
            file.metadata_failure = Some(failure);
        }
    }
}

impl Engine {
    pub fn external_file(&self, path: &str) -> Option<&ExternalFile> {
        self.external_files.get(path)
    }
    /// Desktop OBS_OPEN_FILE's observed Markdown subset. The caller supplies an
    /// absolute main-process path; symlinks are rejected by lstat, not followed.
    pub fn open_external_file(
        &mut self,
        path: &Path,
        options: OpenOptions,
    ) -> Result<ExternalOpenResult, EngineError> {
        self.open_external_file_in_pane(
            path,
            if options.new_tab {
                crate::panes::PaneRequest::Tab
            } else {
                crate::panes::PaneRequest::Reuse
            },
        )
    }
    pub fn open_external_file_in_pane(
        &mut self,
        path: &Path,
        pane: crate::panes::PaneRequest,
    ) -> Result<ExternalOpenResult, EngineError> {
        if !path.is_absolute() {
            return Ok(ExternalOpenResult::Missing);
        }
        let Ok(stat) = std::fs::symlink_metadata(path) else {
            return Ok(ExternalOpenResult::Missing);
        };
        if !stat.is_file() {
            return Ok(ExternalOpenResult::Missing);
        }
        let identity = format!("file:{}", path.display());
        if mnemosyne_core::extension(&identity) != "md" {
            return Ok(if crate::view_type(&identity) == "file" {
                ExternalOpenResult::UnsupportedExtension
            } else {
                ExternalOpenResult::PendingViewType(crate::view_type(&identity).into())
            });
        }
        if !self.external_files.contains_key(&identity) {
            self.external_files.insert(
                identity.clone(),
                ExternalFile {
                    path: identity.clone(),
                    real_path: path.into(),
                    stat: ExternalStat::from_metadata(&stat).map_err(|e| io(path, e))?,
                    users: 0,
                    metadata: None,
                    metadata_failure: None,
                },
            );
        }
        self.open_file_in_pane(FileTarget::path(&identity), pane)?;
        Ok(ExternalOpenResult::Opened(identity))
    }
    pub(crate) fn reconcile_external_users(&mut self) -> Result<(), EngineError> {
        let mut opened = Vec::new();
        for (path, file) in &mut self.external_files {
            let users = self
                .session
                .leaves
                .iter()
                .filter(|leaf| leaf.file.as_ref() == Some(path))
                .count();
            if users == 0 {
                file.metadata = None;
                file.metadata_failure = None;
                self.session.buffers.remove(path);
            } else if file.users == 0 {
                let source = read_text(&file.real_path)?;
                if self.metadata_lifecycle.retained {
                    opened.push(path.clone());
                } else {
                    update_metadata(file, &source);
                }
                self.session
                    .buffers
                    .entry(path.clone())
                    .or_insert_with(|| EditorBuffer::new(source.clone()));
                for leaf in &self.session.leaves {
                    if leaf.file.as_ref() == Some(path) {
                        self.session
                            .leaf_buffers
                            .entry(leaf.id.clone())
                            .or_insert_with(|| EditorBuffer::new(source.clone()));
                    }
                }
            }
            file.users = users;
        }
        for path in opened {
            self.stage_external_metadata(&path, true);
        }
        Ok(())
    }
    /// Rebuild transient bindings for our private session import. This is not
    /// the desktop app's external-file workspace serialization or crash-save contract.
    pub(crate) fn restore_external_bindings(&mut self) -> Result<(), EngineError> {
        for leaf in &self.session.leaves {
            let Some(identity) = leaf.file.as_deref() else {
                continue;
            };
            let Some(real) = real_path(identity) else {
                continue;
            };
            if self.external_files.contains_key(identity) {
                continue;
            }
            let Ok(stat) = std::fs::symlink_metadata(real) else {
                continue;
            };
            if !stat.is_file() || mnemosyne_core::extension(identity) != "md" {
                continue;
            }
            self.external_files.insert(
                identity.into(),
                ExternalFile {
                    path: identity.into(),
                    real_path: real.into(),
                    stat: ExternalStat::from_metadata(&stat).map_err(|e| io(real, e))?,
                    users: 0,
                    metadata: None,
                    metadata_failure: None,
                },
            );
        }
        self.reconcile_external_users()
    }
    /// Process observed external stat-change events. Native GP monitors compare
    /// rounded ctime/mtime/size, not content hashes. This is synchronous event
    /// consumption; installing an OS watcher and native event timing is pending.
    pub fn refresh_external_files(&mut self) -> Result<(), EngineError> {
        if self.metadata_lifecycle.retained {
            return Ok(());
        }
        for (path, file) in &mut self.external_files {
            if file.users == 0 {
                continue;
            }
            let stat = std::fs::symlink_metadata(&file.real_path)
                .ok()
                .filter(|s| s.is_file());
            let Some(stat) = stat else {
                file.metadata = None;
                file.metadata_failure = None;
                continue;
            };
            let stat = ExternalStat::from_metadata(&stat).map_err(|e| io(&file.real_path, e))?;
            if stat == file.stat {
                continue;
            }
            let source = read_text(&file.real_path)?;
            file.stat = stat;
            update_metadata(file, &source);
            for leaf in &self.session.leaves {
                if leaf.file.as_ref() != Some(path) {
                    continue;
                }
                let Some(buffer) = self.session.leaf_buffers.get_mut(&leaf.id) else {
                    continue;
                };
                let text = editor_saved_text(&source);
                // Actual external monitor mutation replaces unrelated dirty
                // contents. Cursor mapping uses the shared native loadContents.
                crate::editor_merge::load_contents(buffer, &text)?;
                buffer.saved_text = text;
            }
        }
        Ok(())
    }
    /// Saved modify while e.saving is nonzero refreshes cached data/stat, leaving
    /// the view untouched in the captured native snapshot. No autosave timing is
    /// implied. This method is separate from an external monitor change.
    pub fn modify_external_saved(&mut self, path: &str, source: &str) -> Result<(), EngineError> {
        let real = real_path(path)
            .filter(|_| is_file(path))
            .ok_or_else(|| EngineError::FileNotFound(path.into()))?;
        std::fs::write(real, source).map_err(|e| io(real, e))?;
        if let Some(file) = self.external_files.get_mut(path) {
            let stat = std::fs::symlink_metadata(real).map_err(|e| io(real, e))?;
            file.stat = ExternalStat::from_metadata(&stat).map_err(|e| io(real, e))?;
            if self.metadata_lifecycle.retained {
                file.metadata = None;
                file.metadata_failure = None;
            } else if file.users > 0 {
                update_metadata(file, source);
            }
        }
        if self.metadata_lifecycle.retained {
            self.emit_vault_event(
                crate::events::EventName::Modify,
                path,
                crate::events::FileKind::File,
                None,
            );
            self.stage_external_metadata(path, false);
        }
        self.acknowledge_retained_path(path);
        Ok(())
    }
    pub(crate) fn retained_external_modified(&mut self, path: &str) -> Result<(), EngineError> {
        let Some(file) = self
            .external_files
            .get_mut(path)
            .filter(|file| file.users > 0)
        else {
            return Ok(());
        };
        let stat = std::fs::symlink_metadata(&file.real_path)
            .map_err(|error| io(&file.real_path, error))?;
        file.stat =
            ExternalStat::from_metadata(&stat).map_err(|error| io(&file.real_path, error))?;
        file.metadata = None;
        file.metadata_failure = None;
        let source = read_text(&file.real_path)?;
        for leaf in &mut self.session.leaves {
            if leaf.file.as_deref() == Some(path) {
                if let Some(buffer) = self.session.leaf_buffers.get_mut(&leaf.id) {
                    let saved = editor_saved_text(&source);
                    crate::editor_merge::load_contents(buffer, &saved)?;
                    buffer.saved_text = saved;
                }
                if let Some(state) = leaf.markdown.as_mut() {
                    state.load(&source);
                    let revision = self
                        .session
                        .leaf_buffers
                        .get(&leaf.id)
                        .map_or(0, |buffer| buffer.edit_revision);
                    state.clean_editor_revision = revision;
                    state.scheduled_editor_revision = revision;
                }
            }
        }
        self.emit_vault_event(
            crate::events::EventName::Modify,
            path,
            crate::events::FileKind::File,
            None,
        );
        self.stage_external_metadata(path, false);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stat_rounding_matches_math_round_including_negative_ties() {
        for (nanos, expected) in [(499_999, 0), (500_000, 1), (1_500_000, 2)] {
            assert_eq!(
                rounded_millis(UNIX_EPOCH + std::time::Duration::from_nanos(nanos)),
                expected
            );
            assert_eq!(
                rounded_millis(UNIX_EPOCH - std::time::Duration::from_nanos(nanos)),
                if nanos == 499_999 {
                    0
                } else if nanos == 500_000 {
                    0
                } else {
                    -1
                }
            );
        }
    }
}
