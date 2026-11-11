//! Genuine OS notifications enqueue paths; the retained owner reconciles them.
//! DesktopAdapter's observed external rename is creation plus a delayed deletion,
//! never an inferred Vault.rename. A quiet tick does no vault walk or stat scan.
use crate::{Engine, EngineError};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, TrySendError},
        Arc,
    },
    time::{Duration, Instant},
};

#[derive(Debug, Clone, PartialEq, Eq)]
struct Stat {
    ctime: i64,
    mtime: i64,
    size: u64,
}
fn stat(path: &Path) -> Option<Stat> {
    let value = std::fs::symlink_metadata(path).ok()?;
    if !value.is_file() {
        return None;
    }
    Some(Stat {
        ctime: value
            .created()
            .map(crate::external::rounded_millis)
            .unwrap_or(0),
        mtime: crate::external::rounded_millis(value.modified().ok()?),
        size: value.len(),
    })
}
#[derive(Debug, Clone)]
enum Change {
    Created(PathBuf),
    Modified(PathBuf),
    Deleted(PathBuf),
}

pub struct ExternalWatcher {
    watcher: RecommendedWatcher,
    receiver: Receiver<notify::Result<Event>>,
    overflow: Arc<AtomicBool>,
    root: PathBuf,
    canonical_root: PathBuf,
    known: BTreeMap<PathBuf, Stat>,
    deleted: BTreeMap<PathBuf, Instant>,
    external: BTreeSet<PathBuf>,
    external_parents: BTreeSet<PathBuf>,
    external_aliases: BTreeMap<PathBuf, PathBuf>,
    bootstrap: bool,
    pending: BTreeSet<PathBuf>,
    pending_changes: VecDeque<Change>,
}
impl std::fmt::Debug for ExternalWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExternalWatcher")
            .field("root", &self.root)
            .field("known_paths", &self.known.len())
            .field("pending_deletions", &self.deleted.len())
            .finish()
    }
}
impl ExternalWatcher {
    fn new(root: &Path, known: BTreeMap<PathBuf, Stat>) -> Result<Self, EngineError> {
        let (sender, receiver) = mpsc::sync_channel(4096);
        let overflow = Arc::new(AtomicBool::new(false));
        let callback_overflow = overflow.clone();
        let mut watcher = notify::recommended_watcher(move |event| {
            if let Err(TrySendError::Full(_)) = sender.try_send(event) {
                callback_overflow.store(true, Ordering::Release);
            }
        })
        .map_err(watch_error)?;
        watcher
            .watch(root, RecursiveMode::Recursive)
            .map_err(watch_error)?;
        let canonical_root = std::fs::canonicalize(root).map_err(|error| crate::io(root, error))?;
        Ok(Self {
            watcher,
            receiver,
            overflow,
            root: root.into(),
            canonical_root,
            known,
            deleted: BTreeMap::new(),
            external: BTreeSet::new(),
            external_parents: BTreeSet::new(),
            external_aliases: BTreeMap::new(),
            bootstrap: true,
            pending: BTreeSet::new(),
            pending_changes: VecDeque::new(),
        })
    }
    /// Own mutations acknowledge their completed actual stat, suppressing echoes
    /// while a later genuinely different filesystem stat remains observable.
    pub(crate) fn acknowledge(&mut self, path: &Path) {
        self.deleted.retain(|pending, _| !pending.starts_with(path));
        self.known.retain(|known, _| !known.starts_with(path));
        if let Some(value) = stat(path) {
            self.known.insert(path.into(), value);
        } else if std::fs::symlink_metadata(path).is_ok_and(|value| value.is_dir()) {
            let mut paths = BTreeSet::new();
            self.expand(path, &mut paths);
            for path in paths {
                if let Some(value) = stat(&path) {
                    self.known.insert(path, value);
                }
            }
        }
    }
    fn sync_external(&mut self, retained: BTreeMap<PathBuf, Stat>) -> Result<(), EngineError> {
        let paths = retained.keys().cloned().collect::<BTreeSet<_>>();
        let parents = paths
            .iter()
            .filter_map(|path| path.parent().map(Path::to_path_buf))
            .filter(|parent| !parent.starts_with(&self.root))
            .collect::<BTreeSet<_>>();
        for parent in parents.difference(&self.external_parents) {
            self.watcher
                .watch(parent, RecursiveMode::NonRecursive)
                .map_err(watch_error)?;
        }
        for parent in self.external_parents.difference(&parents) {
            self.watcher.unwatch(parent).map_err(watch_error)?;
        }
        for path in paths.difference(&self.external) {
            self.known.insert(path.clone(), retained[path].clone());
            self.pending.insert(path.clone());
        }
        for path in self.external.difference(&paths) {
            self.known.remove(path);
            self.deleted.remove(path);
        }
        self.external_aliases = paths
            .iter()
            .filter_map(|path| {
                let parent = std::fs::canonicalize(path.parent()?).ok()?;
                Some((parent.join(path.file_name()?), path.clone()))
            })
            .collect();
        self.external = paths;
        self.external_parents = parents;
        Ok(())
    }
    fn visible(&self, path: &Path) -> bool {
        if self.external.contains(path) {
            return true;
        }
        path.strip_prefix(&self.root).is_ok_and(|relative| {
            relative
                .components()
                .all(|component| !component.as_os_str().to_string_lossy().starts_with('.'))
        })
    }
    fn expand(&self, path: &Path, paths: &mut BTreeSet<PathBuf>) {
        if !self.visible(path) {
            return;
        }
        paths.insert(path.into());
        // A real directory notification reconciles only its subtree. This also
        // handles backend coalescing; no directory is walked on an idle tick.
        if path.starts_with(&self.root) {
            paths.extend(
                self.known
                    .keys()
                    .filter(|known| known.starts_with(path))
                    .cloned(),
            );
            if std::fs::symlink_metadata(path).is_ok_and(|value| value.is_dir()) {
                if let Ok(entries) = std::fs::read_dir(path) {
                    for entry in entries.flatten() {
                        self.expand(&entry.path(), paths);
                    }
                }
            }
        }
    }
    /// notify's macOS backend canonicalizes its subscriptions/event paths.
    /// Translate observations back to retained logical identities, including
    /// removed files whose complete path can no longer be canonicalized.
    fn notified_paths(&self, path: &Path, paths: &mut BTreeSet<PathBuf>) {
        if let Some(logical) = self.external_aliases.get(path) {
            self.expand(logical, paths);
            return;
        }
        if let Ok(relative) = path.strip_prefix(&self.canonical_root) {
            self.expand(&self.root.join(relative), paths);
            return;
        }
        self.expand(path, paths);
        // A coalesced outside-directory event reconciles only its active users.
        for (canonical, logical) in &self.external_aliases {
            if canonical.parent() == Some(path) || logical.parent() == Some(path) {
                self.expand(logical, paths);
            }
        }
    }
    fn poll(&mut self, now: Instant) -> Result<Vec<Change>, EngineError> {
        let mut paths = std::mem::take(&mut self.pending);
        if self.bootstrap {
            self.bootstrap = false;
            self.expand(&self.root, &mut paths);
        }
        if self.overflow.swap(false, Ordering::AcqRel) {
            self.expand(&self.root, &mut paths);
            paths.extend(self.external.iter().cloned());
        }
        while let Ok(event) = self.receiver.try_recv() {
            let event = match event {
                Ok(event) => event,
                Err(error) => {
                    self.pending.extend(paths);
                    return Err(watch_error(error));
                }
            };
            if matches!(event.kind, EventKind::Access(_)) {
                continue;
            }
            for path in event.paths {
                self.notified_paths(&path, &mut paths);
            }
        }
        let mut changed = Vec::new();
        for path in paths {
            if !self.visible(&path) {
                continue;
            }
            match (self.known.get(&path), stat(&path)) {
                (None, Some(value)) => {
                    self.deleted.remove(&path);
                    self.known.insert(path.clone(), value);
                    changed.push(Change::Created(path));
                }
                (Some(old), Some(value)) => {
                    self.deleted.remove(&path);
                    let different = old.mtime != value.mtime
                        || old.size != value.size
                        || self.external.contains(&path) && old.ctime != value.ctime;
                    if different {
                        self.known.insert(path.clone(), value);
                        changed.push(Change::Modified(path));
                    }
                }
                (Some(_), None) => {
                    if self.external.contains(&path) {
                        self.known.remove(&path);
                        changed.push(Change::Deleted(path));
                    } else {
                        self.deleted
                            .entry(path)
                            .or_insert(now + Duration::from_millis(100));
                    }
                }
                _ => {}
            }
        }
        let due = self
            .deleted
            .iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(path, _)| path.clone())
            .collect::<Vec<_>>();
        for path in due {
            self.deleted.remove(&path);
            match stat(&path) {
                None if self.known.remove(&path).is_some() => changed.push(Change::Deleted(path)),
                Some(value) => {
                    if self
                        .known
                        .get(&path)
                        .is_some_and(|old| old.mtime != value.mtime || old.size != value.size)
                    {
                        self.known.insert(path.clone(), value);
                        changed.push(Change::Modified(path));
                    }
                }
                _ => {}
            }
        }
        Ok(changed)
    }
}
fn watch_error(error: notify::Error) -> EngineError {
    EngineError::InvalidState(format!("Filesystem watch: {error}"))
}

impl Engine {
    pub fn enable_retained_watch(&mut self) -> Result<(), EngineError> {
        self.enable_metadata_lifecycle()?;
        if self.external_watcher.is_none() {
            let root = self.vault.root.clone();
            let retained = self
                .retained_vault_file_stats()
                .iter()
                .map(|(path, value)| {
                    (
                        root.join(path),
                        Stat {
                            ctime: value.ctime,
                            mtime: value.mtime,
                            size: value.size,
                        },
                    )
                })
                .collect();
            self.external_watcher = Some(ExternalWatcher::new(&root, retained)?);
            // One reconciliation after subscribing closes the activation gap.
            // The baseline came from retained objects, never current disk stats.
            self.poll_external_changes()?;
        }
        Ok(())
    }
    pub(crate) fn acknowledge_retained_path(&mut self, path: &str) {
        let absolute = crate::external::real_path(path)
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.vault.root.join(path));
        if let Some(watcher) = self.external_watcher.as_mut() {
            watcher.acknowledge(&absolute);
        }
    }
    pub fn poll_external_changes(&mut self) -> Result<Vec<String>, EngineError> {
        self.poll_external_changes_at(Instant::now())
    }
    pub fn poll_external_changes_at(&mut self, now: Instant) -> Result<Vec<String>, EngineError> {
        let external = self
            .external_files
            .values()
            .filter(|file| file.users > 0)
            .map(|file| {
                (
                    file.real_path.clone(),
                    Stat {
                        ctime: file.stat.ctime,
                        mtime: file.stat.mtime,
                        size: file.stat.size,
                    },
                )
            })
            .collect();
        let Some(watcher) = self.external_watcher.as_mut() else {
            return Ok(Vec::new());
        };
        watcher.sync_external(external)?;
        let changes = watcher.poll(now)?;
        watcher.pending_changes.extend(changes);
        let mut paths = Vec::new();
        while let Some(change) = self
            .external_watcher
            .as_ref()
            .and_then(|watcher| watcher.pending_changes.front().cloned())
        {
            let absolute = match &change {
                Change::Created(path) | Change::Modified(path) | Change::Deleted(path) => path,
            };
            let identity = if self
                .external_files
                .values()
                .any(|file| file.users > 0 && file.real_path == *absolute)
            {
                format!("file:{}", absolute.display())
            } else {
                let Ok(relative) = absolute.strip_prefix(&self.vault.root) else {
                    self.external_watcher
                        .as_mut()
                        .unwrap()
                        .pending_changes
                        .pop_front();
                    continue;
                };
                let path = relative.to_string_lossy().replace('\\', "/");
                if !crate::safe_relative(&path) {
                    self.external_watcher
                        .as_mut()
                        .unwrap()
                        .pending_changes
                        .pop_front();
                    continue;
                }
                path
            };
            match change {
                Change::Created(_) => self.external_fs_created(&identity)?,
                Change::Modified(_) => self.external_fs_modified(&identity)?,
                Change::Deleted(_) => self.external_fs_deleted(&identity)?,
            }
            // Commit only after owner reconciliation succeeds. On any error the
            // failed and remaining obligations stay queued for the next tick.
            self.external_watcher
                .as_mut()
                .unwrap()
                .pending_changes
                .pop_front();
            paths.push(identity);
        }
        Ok(paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{ModifyKind, RemoveKind};
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "mn-watch-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn watcher(
            &self,
            paths: &[PathBuf],
        ) -> (ExternalWatcher, mpsc::SyncSender<notify::Result<Event>>) {
            let known = paths
                .iter()
                .filter_map(|path| stat(path).map(|value| (path.clone(), value)))
                .collect();
            let mut watcher = ExternalWatcher::new(&self.0, known).unwrap();
            watcher.bootstrap = false;
            let (sender, receiver) = mpsc::sync_channel(8);
            watcher.receiver = receiver;
            (watcher, sender)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn coalesced_removed_directory_reconciles_known_children_after_delay() {
        let fixture = Fixture::new();
        let dir = fixture.0.join("Folder");
        std::fs::create_dir(&dir).unwrap();
        let a = dir.join("A.md");
        let b = dir.join("B.md");
        std::fs::write(&a, "a").unwrap();
        std::fs::write(&b, "b").unwrap();
        let (mut watcher, sender) = fixture.watcher(&[a, b]);
        std::fs::remove_dir_all(&dir).unwrap();
        sender
            .send(Ok(
                Event::new(EventKind::Remove(RemoveKind::Folder)).add_path(dir)
            ))
            .unwrap();
        let now = Instant::now();
        assert!(watcher.poll(now).unwrap().is_empty());
        assert!(watcher
            .poll(now + Duration::from_millis(99))
            .unwrap()
            .is_empty());
        let changes = watcher.poll(now + Duration::from_millis(100)).unwrap();
        assert_eq!(changes.len(), 2);
        assert!(changes
            .iter()
            .all(|change| matches!(change, Change::Deleted(_))));
        assert!(watcher.known.is_empty());
    }
    #[test]
    fn recreated_file_is_reconciled_when_only_removal_notification_arrived() {
        let fixture = Fixture::new();
        let path = fixture.0.join("Note.md");
        std::fs::write(&path, "old").unwrap();
        let (mut watcher, sender) = fixture.watcher(&[path.clone()]);
        std::fs::remove_file(&path).unwrap();
        sender
            .send(Ok(
                Event::new(EventKind::Remove(RemoveKind::File)).add_path(path.clone())
            ))
            .unwrap();
        let now = Instant::now();
        assert!(watcher.poll(now).unwrap().is_empty());
        std::fs::write(&path, "new longer").unwrap();
        assert!(
            matches!(watcher.poll(now+Duration::from_millis(100)).unwrap().as_slice(),[Change::Modified(changed)] if changed==&path)
        );
        assert_eq!(watcher.known[&path], stat(&path).unwrap());
    }
    #[test]
    fn acknowledged_self_write_echo_and_private_event_produce_no_change() {
        let fixture = Fixture::new();
        let path = fixture.0.join("Note.md");
        std::fs::write(&path, "old").unwrap();
        let (mut watcher, sender) = fixture.watcher(&[path.clone()]);
        std::fs::write(&path, "new longer").unwrap();
        watcher.acknowledge(&path);
        sender
            .send(Ok(
                Event::new(EventKind::Modify(ModifyKind::Any)).add_path(path)
            ))
            .unwrap();
        let private = fixture.0.join(".mnemosyne");
        std::fs::create_dir(&private).unwrap();
        let cache = private.join("metadata-cache.json");
        std::fs::write(&cache, "{}").unwrap();
        sender
            .send(Ok(Event::new(EventKind::Create(
                notify::event::CreateKind::File,
            ))
            .add_path(cache)))
            .unwrap();
        assert!(watcher.poll(Instant::now()).unwrap().is_empty());
    }
    #[test]
    fn canonical_backend_paths_preserve_logical_vault_and_external_identities() {
        let fixture = Fixture::new();
        let path = fixture.0.join("Note.md");
        std::fs::write(&path, "old").unwrap();
        let (mut watcher, sender) = fixture.watcher(&[path.clone()]);
        let physical = watcher.canonical_root.join("Note.md");
        std::fs::write(&path, "new longer").unwrap();
        sender
            .send(Ok(
                Event::new(EventKind::Modify(ModifyKind::Any)).add_path(physical.clone())
            ))
            .unwrap();
        assert!(
            matches!(watcher.poll(Instant::now()).unwrap().as_slice(),[Change::Modified(actual)] if actual==&path)
        );
        // Translation works after removal, without canonicalizing the file.
        std::fs::remove_file(&path).unwrap();
        sender
            .send(Ok(
                Event::new(EventKind::Remove(RemoveKind::File)).add_path(physical)
            ))
            .unwrap();
        let now = Instant::now();
        assert!(watcher.poll(now).unwrap().is_empty());
        assert!(
            matches!(watcher.poll(now+Duration::from_millis(100)).unwrap().as_slice(),[Change::Deleted(actual)] if actual==&path)
        );
        let outside = Fixture::new();
        let logical = outside.0.join("Outside.md");
        std::fs::write(&logical, "old").unwrap();
        watcher
            .sync_external(
                [(logical.clone(), stat(&logical).unwrap())]
                    .into_iter()
                    .collect(),
            )
            .unwrap();
        watcher.pending.clear();
        let physical = std::fs::canonicalize(&outside.0)
            .unwrap()
            .join("Outside.md");
        std::fs::write(&logical, "new longer").unwrap();
        sender
            .send(Ok(
                Event::new(EventKind::Modify(ModifyKind::Any)).add_path(physical)
            ))
            .unwrap();
        assert!(
            matches!(watcher.poll(Instant::now()).unwrap().as_slice(),[Change::Modified(actual)] if actual==&logical)
        );
    }
}
