//! Folder child insertion order, separate from sorted metadata indexes.
//! Source-derived: Vault.recurseChildren pops a stack and appends folder.children.
use crate::{io, EngineError};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, Default)]
pub(crate) struct FolderChronology {
    children: BTreeMap<String, Vec<String>>,
}

impl FolderChronology {
    pub(crate) fn empty() -> Self {
        let mut order = Self::default();
        order.children.insert(String::new(), Vec::new());
        order
    }
    /// Actual loader traversal; each returned entry is published separately.
    pub(crate) fn startup_entries(
        root: &Path,
    ) -> Result<std::collections::VecDeque<(String, bool)>, EngineError> {
        fn scan(
            root: &Path,
            folder: &str,
            entries: &mut std::collections::VecDeque<(String, bool)>,
        ) -> Result<(), EngineError> {
            let absolute = root.join(folder);
            let mut children = std::fs::read_dir(&absolute)
                .map_err(|error| io(&absolute, error))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| io(&absolute, error))?;
            children.sort_by_key(|entry| entry.file_name());
            for entry in children {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') {
                    continue;
                }
                let kind = entry
                    .file_type()
                    .map_err(|error| io(&entry.path(), error))?;
                if !kind.is_file() && !kind.is_dir() {
                    continue;
                }
                let path = if folder.is_empty() {
                    name
                } else {
                    format!("{folder}/{name}")
                };
                entries.push_back((path.clone(), kind.is_dir()));
                if kind.is_dir() {
                    scan(root, &path, entries)?;
                }
            }
            Ok(())
        }
        let mut entries = std::collections::VecDeque::new();
        scan(root, "", &mut entries)?;
        Ok(entries)
    }
    pub fn open(root: &Path) -> Result<Self, EngineError> {
        let mut order = Self::default();
        order.children.insert(String::new(), Vec::new());
        order.read_folder(root, "")?;
        Ok(order)
    }
    fn read_folder(&mut self, root: &Path, folder: &str) -> Result<(), EngineError> {
        let absolute = root.join(folder);
        let mut entries: Vec<_> = std::fs::read_dir(&absolute)
            .map_err(|error| io(&absolute, error))?
            .collect::<Result<_, _>>()
            .map_err(|error| io(&absolute, error))?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let file_type = entry
                .file_type()
                .map_err(|error| io(&entry.path(), error))?;
            if !file_type.is_dir() && !file_type.is_file() {
                continue;
            }
            let path = if folder.is_empty() {
                name
            } else {
                format!("{folder}/{name}")
            };
            self.insert(&path, file_type.is_dir());
            if file_type.is_dir() {
                self.read_folder(root, &path)?;
            }
        }
        Ok(())
    }
    pub fn paths(&self) -> Vec<&str> {
        let mut stack: Vec<&str> = self
            .children
            .get("")
            .into_iter()
            .flatten()
            .map(String::as_str)
            .collect();
        let mut paths = Vec::new();
        while let Some(path) = stack.pop() {
            if let Some(children) = self.children.get(path) {
                stack.extend(children.iter().map(String::as_str));
            } else {
                paths.push(path);
            }
        }
        paths
    }
    pub fn folders(&self) -> Vec<&str> {
        let mut paths = vec!["/"];
        let mut stack: Vec<&str> = self
            .children
            .get("")
            .into_iter()
            .flatten()
            .map(String::as_str)
            .collect();
        while let Some(path) = stack.pop() {
            if let Some(children) = self.children.get(path) {
                paths.push(path);
                stack.extend(children.iter().map(String::as_str));
            }
        }
        paths
    }
    pub fn insert(&mut self, path: &str, folder: bool) {
        let parent = parent(path);
        if !self.children.contains_key(parent) {
            self.insert(parent, true);
        }
        let siblings = self.children.get_mut(parent).unwrap();
        if !siblings.iter().any(|child| child == path) {
            siblings.push(path.into());
        }
        if folder {
            self.children.entry(path.into()).or_default();
        }
    }
    pub fn remove(&mut self, path: &str) {
        if let Some(siblings) = self.children.get_mut(parent(path)) {
            siblings.retain(|child| child != path);
        }
        self.children
            .retain(|folder, _| folder != path && !is_descendant(folder, path));
    }
    pub fn rename(&mut self, from: &str, to: &str) {
        let subtree: Vec<_> = self
            .children
            .iter()
            .filter(|(path, _)| *path == from || is_descendant(path, from))
            .map(|(path, children)| (path.clone(), children.clone()))
            .collect();
        self.remove(from);
        self.insert(to, !subtree.is_empty());
        for (folder, children) in subtree {
            let key = format!("{to}{}", &folder[from.len()..]);
            let children = children
                .into_iter()
                .map(|child| format!("{to}{}", &child[from.len()..]))
                .collect();
            self.children.insert(key, children);
        }
    }
    /// Keep existing order through rescans. New children discovered together
    /// follow the sorted disk snapshot; watcher delivery order remains pending.
    pub fn reconcile(&mut self, disk: Self) {
        for (folder, children) in &mut self.children {
            if let Some(on_disk) = disk.children.get(folder) {
                children.retain(|path| on_disk.contains(path));
            }
        }
        self.children
            .retain(|folder, _| disk.children.contains_key(folder));
        for (folder, on_disk) in disk.children {
            let children = self.children.entry(folder).or_default();
            for path in on_disk {
                if !children.contains(&path) {
                    children.push(path);
                }
            }
        }
    }
}
fn parent(path: &str) -> &str {
    path.rsplit_once('/')
        .map(|(parent, _)| parent)
        .unwrap_or("")
}
fn is_descendant(path: &str, folder: &str) -> bool {
    path.strip_prefix(folder)
        .is_some_and(|suffix| suffix.starts_with('/'))
}
