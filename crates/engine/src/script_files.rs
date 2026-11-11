//! Retained SDK file objects, independent of JavaScript wrapper allocation.
//!
//! Actual instance evidence: plugin_api_instances.json. Data descriptors are
//! data properties, not fabricated accessors. Lifecycle notifications are an
//! explicit input; engine-wide notification and mutable JS property writes must
//! be wired before this can claim installed Vault/SDK compatibility.
use mnemosyne_runtime_protocol::{
    HostDescriptor, HostDescriptorKind, HostError, HostKey, HostObjectId, HostResult, HostValue,
};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stat {
    pub ctime: f64,
    pub mtime: f64,
    pub size: f64,
}
#[derive(Debug)]
enum Object {
    File {
        path: String,
        parent: Option<HostObjectId>,
        deleted: bool,
        stat: HostObjectId,
        saving: f64,
    },
    Folder {
        path: String,
        parent: Option<HostObjectId>,
        deleted: bool,
        children: HostObjectId,
    },
    Children(Vec<HostObjectId>),
    Stat(Stat),
}

/// IDs are never reused, including after deletion or replacement of stat data.
#[derive(Debug)]
pub struct FileObjects {
    vault: HostObjectId,
    root: HostObjectId,
    next: u64,
    paths: BTreeMap<String, HostObjectId>,
    objects: BTreeMap<u64, Object>,
}
impl FileObjects {
    /// `first_id` belongs to the caller's object registry, after service IDs.
    pub fn new(vault: HostObjectId, first_id: u64) -> Self {
        assert!(first_id > vault.0);
        let mut result = Self {
            vault,
            root: HostObjectId(first_id),
            next: first_id,
            paths: BTreeMap::new(),
            objects: BTreeMap::new(),
        };
        let children = result.allocate(Object::Children(Vec::new()));
        result.root = result.allocate(Object::Folder {
            path: "/".into(),
            parent: None,
            deleted: false,
            children,
        });
        result.paths.insert("/".into(), result.root);
        result
    }
    fn allocate(&mut self, object: Object) -> HostObjectId {
        let id = HostObjectId(self.next);
        self.next = self.next.checked_add(1).expect("SDK object ID exhausted");
        self.objects.insert(id.0, object);
        id
    }
    pub fn root(&self) -> HostObjectId {
        self.root
    }
    /// Literal lookup: the native SDK does not normalize './', empty, or '/x'.
    pub fn lookup(&self, path: &str) -> Option<HostObjectId> {
        self.paths.get(path).copied()
    }
    pub fn lookup_file(&self, path: &str) -> Option<HostObjectId> {
        self.lookup(path)
            .filter(|id| matches!(self.objects[&id.0], Object::File { .. }))
    }
    pub fn lookup_folder(&self, path: &str) -> Option<HostObjectId> {
        self.lookup(path)
            .filter(|id| matches!(self.objects[&id.0], Object::Folder { .. }))
    }
    fn children_id(&self, folder: HostObjectId) -> Result<HostObjectId, HostError> {
        match self.objects.get(&folder.0) {
            Some(Object::Folder { children, .. }) => Ok(*children),
            _ => Err(HostError::unsupported("SDK object is not a folder")),
        }
    }
    fn add_child(&mut self, folder: HostObjectId, child: HostObjectId) -> Result<(), HostError> {
        let id = self.children_id(folder)?;
        if let Some(Object::Children(items)) = self.objects.get_mut(&id.0) {
            if !items.contains(&child) {
                items.push(child);
            }
        }
        Ok(())
    }
    fn remove_child(&mut self, folder: HostObjectId, child: HostObjectId) -> Result<(), HostError> {
        let id = self.children_id(folder)?;
        if let Some(Object::Children(items)) = self.objects.get_mut(&id.0) {
            items.retain(|id| *id != child);
        }
        Ok(())
    }
    fn parent_for(&mut self, path: &str) -> Result<HostObjectId, HostError> {
        let parent = mnemosyne_core::parent(path);
        if parent.is_empty() {
            Ok(self.root)
        } else {
            self.insert_folder(parent)
        }
    }
    pub fn insert_folder(&mut self, path: &str) -> Result<HostObjectId, HostError> {
        if let Some(id) = self.lookup_folder(path) {
            return Ok(id);
        }
        self.validate_new_path(path)?;
        let parent = self.parent_for(path)?;
        let children = self.allocate(Object::Children(Vec::new()));
        let id = self.allocate(Object::Folder {
            path: path.into(),
            parent: Some(parent),
            deleted: false,
            children,
        });
        self.paths.insert(path.into(), id);
        self.add_child(parent, id)?;
        Ok(id)
    }
    fn validate_new_path(&self, path: &str) -> Result<(), HostError> {
        if path.is_empty()
            || path.starts_with('/')
            || path.ends_with('/')
            || self.lookup(path).is_some()
        {
            return Err(HostError::unsupported(format!(
                "Invalid or occupied SDK path {path:?}"
            )));
        }
        Ok(())
    }
    pub fn insert_file(&mut self, path: &str, stat: Stat) -> Result<HostObjectId, HostError> {
        self.validate_new_path(path)?;
        let parent = self.parent_for(path)?;
        let stat = self.allocate(Object::Stat(stat));
        let id = self.allocate(Object::File {
            path: path.into(),
            parent: Some(parent),
            deleted: false,
            stat,
            saving: 0.0,
        });
        self.paths.insert(path.into(), id);
        self.add_child(parent, id)?;
        Ok(id)
    }
    /// Stat replacement leaves the previous object unchanged and retained.
    pub fn modified(&mut self, file: HostObjectId, stat: Stat) -> Result<(), HostError> {
        if !matches!(self.objects.get(&file.0), Some(Object::File { .. })) {
            return Err(HostError::unsupported("SDK object is not a file"));
        }
        let next_stat = self.allocate(Object::Stat(stat));
        if let Some(Object::File { stat, .. }) = self.objects.get_mut(&file.0) {
            *stat = next_stat;
        }
        Ok(())
    }
    /// File-rename notification. Folder subtree renames require their own
    /// ordered transaction and are not represented by this method.
    pub fn renamed_file(&mut self, file: HostObjectId, to: &str) -> Result<(), HostError> {
        self.validate_new_path(to)?;
        let (from, old_parent) = match self.objects.get(&file.0) {
            Some(Object::File {
                path,
                parent,
                deleted: false,
                ..
            }) => (path.clone(), *parent),
            _ => return Err(HostError::unsupported("SDK object is not a live file")),
        };
        let parent = self.parent_for(to)?;
        if let Some(old_parent) = old_parent {
            self.remove_child(old_parent, file)?;
        }
        self.add_child(parent, file)?;
        self.paths.remove(&from);
        self.paths.insert(to.into(), file);
        if let Some(Object::File {
            path, parent: slot, ..
        }) = self.objects.get_mut(&file.0)
        {
            *path = to.into();
            *slot = Some(parent);
        }
        Ok(())
    }
    pub fn deleted_file(&mut self, file: HostObjectId) -> Result<(), HostError> {
        let (path, parent) = match self.objects.get(&file.0) {
            Some(Object::File { path, parent, .. }) => (path.clone(), *parent),
            _ => return Err(HostError::unsupported("SDK object is not a file")),
        };
        if let Some(parent) = parent {
            self.remove_child(parent, file)?;
        }
        if self.paths.get(&path) == Some(&file) {
            self.paths.remove(&path);
        }
        if let Some(Object::File {
            parent, deleted, ..
        }) = self.objects.get_mut(&file.0)
        {
            *parent = None;
            *deleted = true;
        }
        Ok(())
    }
    pub fn value(&self, id: HostObjectId) -> Result<HostValue, HostError> {
        let class = match self.objects.get(&id.0) {
            Some(Object::File { .. }) => "TFile",
            Some(Object::Folder { .. }) => "TFolder",
            Some(Object::Children(_)) => "Array",
            Some(Object::Stat(_)) => "Object",
            None => return Err(HostError::unsupported("Unknown SDK object ID")),
        };
        Ok(HostValue::HostObject {
            id,
            class: class.encode_utf16().collect(),
        })
    }
    /// Pure file/folder methods over the retained Rust graph. Arbitrary writes
    /// to JS own fields must be inspected before this graph can serve a live
    /// plugin; these methods currently use explicit lifecycle notifications.
    pub fn call(&self, id: HostObjectId, method: &str) -> HostResult {
        match (self.objects.get(&id.0), method) {
            (Some(Object::File { path, .. }), "toString") => Ok(HostValue::from(path.as_str())),
            (Some(Object::File { path, .. }), "getShortName") => {
                let name = mnemosyne_core::basename(path);
                let text = if mnemosyne_core::extension(name) == "md" {
                    mnemosyne_core::strip_extension(name)
                } else {
                    name.to_owned()
                };
                Ok(HostValue::from(text.as_str()))
            }
            (Some(Object::Folder { path, .. }), "isRoot") => Ok(HostValue::Boolean(path == "/")),
            (Some(Object::Folder { .. }), "getFileCount") => {
                Ok(HostValue::Number(self.count(id, false)? as f64))
            }
            (Some(Object::Folder { .. }), "getFolderCount") => {
                Ok(HostValue::Number(self.count(id, true)? as f64))
            }
            _ => Err(HostError::unsupported(format!(
                "SDK file method remains uninstalled: {method}"
            ))),
        }
    }
    fn count(&self, folder: HostObjectId, folders: bool) -> Result<usize, HostError> {
        let children = self.children_id(folder)?;
        let Some(Object::Children(items)) = self.objects.get(&children.0) else {
            unreachable!()
        };
        let mut count = 0;
        for id in items {
            match self.objects.get(&id.0) {
                Some(Object::Folder { .. }) => {
                    count += usize::from(folders) + self.count(*id, folders)?
                }
                Some(Object::File { .. }) if !folders => count += 1,
                _ => {}
            }
        }
        Ok(count)
    }
    fn reference(&self, id: Option<HostObjectId>) -> HostValue {
        id.map(|id| self.value(id).unwrap())
            .unwrap_or(HostValue::Null)
    }
    pub fn own_descriptors(&self, id: HostObjectId) -> Result<Vec<HostDescriptor>, HostError> {
        let mut fields: Vec<(&str, HostValue)> = Vec::new();
        match self.objects.get(&id.0) {
            Some(Object::File {
                path,
                parent,
                deleted,
                stat,
                saving,
            }) => {
                fields.extend(self.abstract_fields(path, *parent, *deleted));
                let name = mnemosyne_core::basename(path);
                fields.extend([
                    (
                        "basename",
                        HostValue::from(mnemosyne_core::strip_extension(name).as_str()),
                    ),
                    (
                        "extension",
                        HostValue::from(mnemosyne_core::extension(name).as_str()),
                    ),
                    ("saving", HostValue::Number(*saving)),
                    ("stat", self.value(*stat)?),
                ]);
            }
            Some(Object::Folder {
                path,
                parent,
                deleted,
                children,
            }) => {
                fields.extend(self.abstract_fields(path, *parent, *deleted));
                fields.push(("children", self.value(*children)?));
            }
            Some(Object::Stat(stat)) => fields.extend([
                ("ctime", HostValue::Number(stat.ctime)),
                ("mtime", HostValue::Number(stat.mtime)),
                ("size", HostValue::Number(stat.size)),
            ]),
            Some(Object::Children(items)) => {
                let mut descriptors = items
                    .iter()
                    .enumerate()
                    .map(|(i, id)| data(&i.to_string(), self.value(*id).unwrap()))
                    .collect::<Vec<_>>();
                let mut length = data("length", HostValue::Number(items.len() as f64));
                length.enumerable = false;
                length.configurable = false;
                descriptors.push(length);
                return Ok(descriptors);
            }
            None => return Err(HostError::unsupported("Unknown SDK object ID")),
        }
        Ok(fields
            .into_iter()
            .map(|(key, value)| data(key, value))
            .collect())
    }
    fn abstract_fields(
        &self,
        path: &str,
        parent: Option<HostObjectId>,
        deleted: bool,
    ) -> Vec<(&'static str, HostValue)> {
        vec![
            ("parent", self.reference(parent)),
            ("deleted", HostValue::Boolean(deleted)),
            (
                "vault",
                HostValue::HostObject {
                    id: self.vault,
                    class: "Vault".encode_utf16().collect(),
                },
            ),
            ("path", HostValue::from(path)),
            ("name", HostValue::from(mnemosyne_core::basename(path))),
        ]
    }
}
fn data(key: &str, value: HostValue) -> HostDescriptor {
    HostDescriptor {
        key: HostKey::from(key),
        kind: HostDescriptorKind::Data {
            value,
            writable: true,
        },
        enumerable: true,
        configurable: true,
    }
}
