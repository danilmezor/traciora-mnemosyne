//! Explicit directory snapshots. Content authority stays in the existing edit/HITL API.
use super::*;
use std::borrow::Cow;
use std::io::{Read, Write};

pub const MAX_FILES: usize = 32;
pub const MAX_SOURCE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_EXPORT_BYTES: usize = 32 * 1024 * 1024;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportFile {
    pub path: String,
    #[serde(default)]
    pub bytes: Vec<u8>,
    #[serde(default)]
    pub directory: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ConflictPolicy {
    #[default]
    Error,
    Skip,
    Overwrite,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportMode {
    Edit,
    Propose,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportOutcome {
    pub path: String,
    pub status: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportPreview {
    pub base_revision: String,
    pub offset: usize,
    pub next_offset: Option<usize>,
    pub source_digest: String,
    pub outcomes: Vec<ImportOutcome>,
    pub changes: Vec<Change>,
    pub digest: String,
}
impl ImportPreview {
    fn digest(&self) -> Result<String> {
        fingerprint(&(
            "import-v1",
            &self.base_revision,
            self.offset,
            self.next_offset,
            &self.source_digest,
            &self.outcomes,
            &self.changes,
        ))
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportFile {
    pub path: String,
    pub directory: bool,
    /// Empty when the page is base64-encoded.
    pub bytes: Vec<u8>,
    pub original_bytes: Vec<u8>,
    /// `encoding: "base64"`: the bytes, standard alphabet, padded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_base64: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_bytes_base64: Option<String>,
}
/// A file's current and original bytes.
type Bytes<'a> = (Cow<'a, [u8]>, Cow<'a, [u8]>);
impl ExportFile {
    /// The current and original bytes, from whichever form the page carries; an array
    /// page is borrowed as it is.
    fn decoded(&self) -> Result<Bytes<'_>> {
        fn decode<'a>(array: &'a [u8], text: &Option<String>) -> Result<Cow<'a, [u8]>> {
            use base64::Engine;
            match text {
                None => Ok(Cow::Borrowed(array)),
                Some(text) if array.is_empty() => base64::engine::general_purpose::STANDARD
                    .decode(text)
                    .map(Cow::Owned)
                    .map_err(|_| ContentError::new("invalid_request", "Invalid base64 in export")),
                Some(_) => Err(ContentError::new(
                    "invalid_request",
                    "An export file carries bytes in both forms",
                )),
            }
        }
        Ok((
            decode(&self.bytes, &self.bytes_base64)?,
            decode(&self.original_bytes, &self.original_bytes_base64)?,
        ))
    }
}
/// `export_snapshot`'s `encoding`: `true` for `"base64"`, `false` for `"array"` (the
/// default); anything else is `invalid_request`.
pub fn export_base64(encoding: Option<&str>) -> Result<bool> {
    match encoding {
        None | Some("array") => Ok(false),
        Some("base64") => Ok(true),
        Some(_) => Err(ContentError::new(
            "invalid_request",
            "encoding must be \"array\" or \"base64\"",
        )),
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExportSnapshot {
    pub revision_id: String,
    pub policy_digest: String,
    pub files: Vec<ExportFile>,
    pub next_offset: Option<usize>,
}
impl ExportSnapshot {
    /// The same page with every file's bytes moved to its base64 fields.
    pub fn into_base64(mut self) -> Self {
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD;
        for f in &mut self.files {
            f.bytes_base64 = Some(b64.encode(std::mem::take(&mut f.bytes)));
            f.original_bytes_base64 = Some(b64.encode(std::mem::take(&mut f.original_bytes)));
        }
        self
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImportSkipped {
    pub revision_id: String,
    pub next_offset: Option<usize>,
    pub skipped: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExportReport {
    pub revision_id: String,
    pub entries: usize,
    pub destination: String,
}

fn validate_files(files: &[ImportFile]) -> Result<()> {
    if files.len() > MAX_FILES {
        return Err(err("import_limit"));
    }
    let mut size = 0usize;
    let mut names = BTreeSet::new();
    for f in files {
        valid_path(&f.path)?;
        size = size
            .checked_add(f.bytes.len())
            .ok_or_else(|| err("import_limit"))?;
        if size > MAX_SOURCE_BYTES {
            return Err(err("import_limit"));
        }
        if !names.insert(&f.path) || (f.directory && !f.bytes.is_empty()) {
            return Err(err("invalid_import"));
        }
    }
    for file in files {
        if !file.directory
            && files
                .iter()
                .any(|f| f.path.starts_with(&format!("{}/", file.path)))
        {
            return Err(err("invalid_import"));
        }
    }
    Ok(())
}
impl Service {
    pub fn import_preview(
        &self,
        who: &TrustedIdentity,
        base: &str,
        mut files: Vec<ImportFile>,
        offset: usize,
        next_offset: Option<usize>,
        conflict: ConflictPolicy,
    ) -> Result<ImportPreview> {
        validate_files(&files)?;
        if next_offset.is_some_and(|n| n <= offset || n != offset.saturating_add(files.len())) {
            return Err(err("invalid_import_cursor"));
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));
        let source_digest = fingerprint(&files)?;
        self.transaction_for(who, false, vec![], false, None, None, |s| {
            check_base(s, who, base)?;
            let existing = s
                .tenants
                .get(&who.tenant_id)
                .and_then(|t| t.revisions.get(&t.head));
            let mut result = ImportPreview {
                base_revision: base.into(),
                offset,
                next_offset,
                source_digest,
                outcomes: vec![],
                changes: vec![],
                digest: String::new(),
            };
            for file in &files {
                crate::ranks::check(s, who, Permission::Read, &file.path)?;
                let prior = existing.and_then(|snap| snap.files.get(&file.path));
                if let Some(entry) = prior {
                    authorize_identity(s, who, entry, &file.path)?;
                }
                let structural_conflict = existing.is_some_and(|snap| {
                    let mut parent = file.path.as_str();
                    while let Some((p, _)) = parent.rsplit_once('/') {
                        if snap
                            .files
                            .get(p)
                            .is_some_and(|e| e.kind != EntryKind::Folder)
                        {
                            return true;
                        }
                        parent = p;
                    }
                    !file.directory
                        && snap
                            .files
                            .keys()
                            .any(|p| p.starts_with(&format!("{}/", file.path)))
                });
                let incompatible =
                    prior.is_some_and(|e| (e.kind == EntryKind::Folder) != file.directory);
                let status = if structural_conflict || incompatible {
                    "conflict"
                } else if prior.is_none() {
                    "create"
                } else if file.directory || conflict == ConflictPolicy::Skip {
                    "skip"
                } else if conflict == ConflictPolicy::Overwrite {
                    "overwrite"
                } else {
                    "conflict"
                };
                result.outcomes.push(ImportOutcome {
                    path: file.path.clone(),
                    status: status.into(),
                });
                if status == "create" || status == "overwrite" {
                    result.changes.push(if file.directory {
                        Change::CreateFolder {
                            path: file.path.clone(),
                        }
                    } else if let Some(content) =
                        std::str::from_utf8(&file.bytes).ok().filter(|_| {
                            !file.path.to_ascii_lowercase().ends_with(".pdf")
                                && !file.path.to_ascii_lowercase().ends_with(".docx")
                        })
                    {
                        Change::Put {
                            path: file.path.clone(),
                            content: content.into(),
                        }
                    } else {
                        Change::PutBinary {
                            path: file.path.clone(),
                            bytes: file.bytes.clone(),
                        }
                    });
                }
            }
            result.digest = result.digest()?;
            Ok(result)
        })
    }
    /// One previewed batch, as `who`'s edit or proposal. `message` and `origin` are the edit's (`log` shows them on the import revision) or, in
    /// `propose` mode, the proposal's `reason` and origin; both are part of the replay
    /// fingerprint, and a batch without them keeps the older fingerprint. A skipped
    /// batch publishes nothing and records neither.
    pub fn import_commit(
        &self,
        who: &TrustedIdentity,
        p: ImportPreview,
        mode: ImportMode,
        key: &str,
        message: Option<&str>,
        origin: Option<Origin>,
    ) -> Result<api::ApiResult> {
        validate_message(message)?;
        validate_origin(origin.as_ref())?;
        if p.digest != p.digest()?
            || p.outcomes.len() > 32
            || p.changes.len() > 32
            || p.changes.iter().any(|c| {
                !matches!(
                    c,
                    Change::Put { .. } | Change::PutBinary { .. } | Change::CreateFolder { .. }
                )
            })
        {
            return Err(err("invalid_import_preview"));
        }
        let mut size = 0usize;
        for c in &p.changes {
            size = size
                .checked_add(match c {
                    Change::Put { content, .. } => content.len(),
                    Change::PutBinary { bytes, .. } => bytes.len(),
                    _ => 0,
                })
                .ok_or_else(|| err("import_limit"))?;
        }
        if size > MAX_SOURCE_BYTES {
            return Err(err("import_limit"));
        }
        if p.outcomes.iter().any(|o| o.status == "conflict") {
            return Err(err("import_conflict"));
        }
        if p.changes.is_empty() {
            return self.transaction_for(who, false, vec![], false, None, None, |s| {
                if matches!(mode, ImportMode::Edit) {
                    human(who)?;
                }
                check_base(s, who, &p.base_revision)?;
                for o in &p.outcomes {
                    crate::ranks::check(s, who, Permission::Read, &o.path)?;
                }
                Ok(api::ApiResult::ImportSkipped(ImportSkipped {
                    revision_id: p.base_revision.clone(),
                    next_offset: p.next_offset,
                    skipped: true,
                }))
            });
        }
        match mode {
            ImportMode::Edit => Ok(api::ApiResult::Revision(self.edit_as(
                who,
                &p.base_revision,
                p.changes,
                key,
                message,
                origin,
                Some(crate::RevisionKind::Import),
            )?)),
            ImportMode::Propose => Ok(api::ApiResult::Proposal(self.propose(
                who,
                &p.base_revision,
                p.changes,
                key,
                message,
                origin,
            )?)),
        }
    }
    pub fn export_snapshot(
        &self,
        who: &TrustedIdentity,
        scope: &str,
        revision: Option<&str>,
        offset: usize,
        limit: usize,
        expected_policy_digest: Option<&str>,
    ) -> Result<ExportSnapshot> {
        if !auth::valid_path(scope, true) {
            return Err(err("invalid_path"));
        }
        if !(1..=32).contains(&limit) {
            return Err(err("export_limit"));
        }
        self.transaction_for(
            who,
            false,
            revision.into_iter().map(str::to_owned).collect(),
            false,
            None,
            None,
            |s| {
                crate::ranks::check(s, who, Permission::Read, scope)?;
                let tenant = s.tenants.get(&who.tenant_id);
                let head = revision.unwrap_or_else(|| {
                    tenant
                        .map(|t| t.head.as_str())
                        .filter(|h| !h.is_empty())
                        .unwrap_or("r0")
                });
                // Hidden as it is now and as it was then.
                if let Some(snap) = tenant.and_then(|t| t.revisions.get(head)) {
                    crate::ranks::check_at(s, who, Permission::Read, scope, &snap.files)?;
                }
                if tenant.is_some_and(|t| t.pruned_revisions.contains(head)) {
                    return Err(err("history_pruned"));
                }
                let policy_digest = fingerprint(&(&s.policy, tenant.map(|t| &t.identity_paths)))?;
                if expected_policy_digest.is_some_and(|d| d != policy_digest) {
                    return Err(err("export_policy_changed"));
                }
                let mut result = ExportSnapshot {
                    revision_id: head.into(),
                    policy_digest,
                    files: vec![],
                    next_offset: None,
                };
                let mut size = 0usize;
                if let Some(snap) = tenant.and_then(|t| t.revisions.get(head)) {
                    // Only authorized entries contribute to pagination or cursor values.
                    for (index, (path, entry)) in snap
                        .files
                        .iter()
                        .filter(|(p, e)| {
                            (scope.is_empty() || *p == scope || p.starts_with(&format!("{scope}/")))
                                && crate::ranks::check(s, who, Permission::Read, p).is_ok()
                                && crate::ranks::check_at(s, who, Permission::Read, p, &snap.files)
                                    .is_ok()
                                && authorize_identity(s, who, e, p).is_ok()
                        })
                        .enumerate()
                        .skip(offset)
                    {
                        let object_size = |blob: &Option<String>| -> Result<usize> {
                            match blob {
                                None => Ok(0),
                                Some(id) => usize::try_from(
                                    fs::metadata(self.root.join("blobs").join(id))
                                        .map_err(io)?
                                        .len(),
                                )
                                .map_err(io),
                            }
                        };
                        let needed = object_size(&entry.blob)?
                            .checked_add(object_size(&entry.original_blob)?)
                            .ok_or_else(|| err("export_limit"))?;
                        if needed > MAX_EXPORT_BYTES {
                            return Err(err("export_file_limit"));
                        }
                        if result.files.len() >= limit || needed > MAX_EXPORT_BYTES - size {
                            result.next_offset = Some(index);
                            break;
                        }
                        let read = |blob: &Option<String>| -> Result<Vec<u8>> {
                            match blob {
                                None => Ok(vec![]),
                                Some(id) => storage::read_blob(&self.root, id),
                            }
                        };
                        let bytes = read(&entry.blob)?;
                        let original_bytes = read(&entry.original_blob)?;
                        size += needed;
                        result.files.push(ExportFile {
                            path: path.clone(),
                            directory: entry.kind == EntryKind::Folder,
                            bytes,
                            original_bytes,
                            bytes_base64: None,
                            original_bytes_base64: None,
                        });
                    }
                } else if head != "r0" {
                    return Err(err("not_found"));
                }
                Ok(result)
            },
        )
    }
}

/// Capture regular files only; symlinks are rejected, including source ancestors.
/// A concurrently changing source is not a filesystem snapshot: the captured bytes
/// themselves are frozen in the preview/session and never reread during commit.
pub fn scan_directory(source: &Path) -> Result<Vec<ImportFile>> {
    let (files, next) = scan_directory_batch(source, 0, 32)?;
    if next.is_some() {
        return Err(err("import_limit"));
    }
    Ok(files)
}
/// Inventory paths without reading file bodies; capture only a bounded page.
pub fn scan_directory_batch(
    source: &Path,
    offset: usize,
    batch_size: usize,
) -> Result<(Vec<ImportFile>, Option<usize>)> {
    reject_symlink_ancestors(source)?;
    if !fs::symlink_metadata(source).map_err(io)?.is_dir() {
        return Err(err("invalid_source"));
    }
    if !(1..=32).contains(&batch_size) {
        return Err(err("import_limit"));
    }
    fn walk(root: &Path, path: &Path, out: &mut Vec<(String, bool)>) -> Result<()> {
        for child in fs::read_dir(path).map_err(io)? {
            let child = child.map_err(io)?;
            let path = child.path();
            let meta = fs::symlink_metadata(&path).map_err(io)?;
            if meta.file_type().is_symlink() {
                return Err(err("unsafe_source"));
            }
            let relative = path
                .strip_prefix(root)
                .map_err(io)?
                .to_str()
                .ok_or_else(|| err("invalid_path"))?
                .to_string();
            valid_path(&relative)?;
            if !meta.is_dir() && !meta.is_file() {
                return Err(err("unsafe_source"));
            }
            out.push((relative, meta.is_dir()));
            if meta.is_dir() {
                walk(root, &path, out)?;
            }
        }
        Ok(())
    }
    let mut inventory = vec![];
    walk(source, source, &mut inventory)?;
    inventory.sort();
    if offset > inventory.len() {
        return Err(err("invalid_import_cursor"));
    }
    let mut files = vec![];
    let mut size = 0;
    for (name, directory) in inventory.iter().skip(offset).take(batch_size) {
        let path = source.join(name);
        reject_symlink_ancestors(&path)?;
        let bytes = if *directory {
            vec![]
        } else {
            let mut opts = fs::OpenOptions::new();
            opts.read(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.custom_flags(libc::O_NOFOLLOW);
            }
            let f = opts.open(&path).map_err(io)?;
            if !f.metadata().map_err(io)?.is_file() {
                return Err(err("unsafe_source"));
            }
            let len = f.metadata().map_err(io)?.len();
            if len > MAX_SOURCE_BYTES as u64 {
                return Err(err("content_limit"));
            }
            if len > (MAX_SOURCE_BYTES - size) as u64 {
                break;
            }
            let mut bytes = vec![];
            f.take((MAX_SOURCE_BYTES - size) as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(io)?;
            if bytes.len() > MAX_SOURCE_BYTES - size {
                return Err(err("source_changed"));
            }
            bytes
        };
        size += bytes.len();
        files.push(ImportFile {
            path: name.clone(),
            directory: *directory,
            bytes,
        });
    }
    let end = offset + files.len();
    validate_files(&files)?;
    Ok((files, (end < inventory.len()).then_some(end)))
}
fn reject_symlink_ancestors(path: &Path) -> Result<()> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map_err(io)?.join(path)
    };
    let mut at = PathBuf::new();
    for part in absolute.components() {
        if matches!(part, std::path::Component::ParentDir) {
            return Err(err("invalid_path"));
        }
        at.push(part);
        if fs::symlink_metadata(&at)
            .map_err(io)?
            .file_type()
            .is_symlink()
        {
            return Err(err("unsafe_path"));
        }
    }
    Ok(())
}

/// Private staged export; publish only after all pages from one revision succeed.
/// Current permissions must be rechecked by the caller for every snapshot page.
pub struct ExportWriter {
    staging: PathBuf,
    destination: PathBuf,
    revision_id: String,
    entries: usize,
    published: bool,
}
impl Drop for ExportWriter {
    fn drop(&mut self) {
        if !self.published {
            let _ = fs::remove_dir_all(&self.staging);
        }
    }
}
impl ExportWriter {
    pub fn new(destination: &Path, revision_id: &str) -> Result<Self> {
        let parent = destination
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        reject_symlink_ancestors(parent)?;
        if fs::symlink_metadata(destination).is_ok() {
            return Err(err("export_destination_exists"));
        }
        static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let staging = loop {
            let candidate = parent.join(format!(
                ".mnemosyne-export-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            match fs::create_dir(&candidate) {
                Ok(()) => break candidate,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(io(e)),
            }
        };
        let writer = Self {
            staging,
            destination: destination.into(),
            revision_id: revision_id.into(),
            entries: 0,
            published: false,
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&writer.staging, fs::Permissions::from_mode(0o700)).map_err(io)?;
        }
        for tree in ["current", "original"] {
            fs::create_dir(writer.staging.join(tree)).map_err(io)?;
        }
        Ok(writer)
    }
    pub fn append(&mut self, page: &ExportSnapshot) -> Result<()> {
        if page.revision_id != self.revision_id {
            return Err(err("export_revision_mismatch"));
        }
        for f in &page.files {
            valid_path(&f.path)?;
            let (bytes, original_bytes) = f.decoded()?;
            for tree in ["current", "original"] {
                let path = self.staging.join(tree).join(&f.path);
                let dir = if f.directory {
                    path.as_path()
                } else {
                    path.parent().ok_or_else(|| err("invalid_path"))?
                };
                fs::create_dir_all(dir).map_err(io)?;
                reject_symlink_ancestors(dir)?;
                if !f.directory {
                    let mut options = fs::OpenOptions::new();
                    options.write(true).create_new(true);
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::OpenOptionsExt;
                        options.custom_flags(libc::O_NOFOLLOW).mode(0o600);
                    }
                    let mut out = options.open(path).map_err(io)?;
                    out.write_all(if tree == "current" {
                        &bytes
                    } else {
                        &original_bytes
                    })
                    .map_err(io)?;
                    out.sync_all().map_err(io)?;
                }
            }
        }
        self.entries += page.files.len();
        Ok(())
    }
    pub fn finish(mut self) -> Result<ExportReport> {
        exclusive_rename(&self.staging, &self.destination)?;
        self.published = true;
        Ok(ExportReport {
            revision_id: self.revision_id.clone(),
            entries: self.entries,
            destination: self.destination.display().to_string(),
        })
    }
}
fn exclusive_rename(from: &Path, to: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let from = std::ffi::CString::new(from.as_os_str().as_bytes()).map_err(io)?;
        let to = std::ffi::CString::new(to.as_os_str().as_bytes()).map_err(io)?;
        #[cfg(target_os = "macos")]
        let status = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) };
        #[cfg(target_os = "linux")]
        let status = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                libc::AT_FDCWD,
                from.as_ptr(),
                libc::AT_FDCWD,
                to.as_ptr(),
                1,
            )
        } as i32;
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        return Err(err("unsupported_platform"));
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        if status != 0 {
            let e = std::io::Error::last_os_error();
            return Err(if e.kind() == std::io::ErrorKind::AlreadyExists {
                err("export_destination_exists")
            } else {
                io(e)
            });
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (from, to);
        Err(err("unsupported_platform"))
    }
}
pub fn write_export(snapshot: &ExportSnapshot, destination: &Path) -> Result<ExportReport> {
    if snapshot.next_offset.is_some() {
        return Err(err("incomplete_export"));
    }
    let mut writer = ExportWriter::new(destination, &snapshot.revision_id)?;
    writer.append(snapshot)?;
    writer.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The replay fingerprint stored for an import without words, and with them.
    fn import_fingerprints(message: Option<&str>) -> String {
        let root = std::env::temp_dir().join(format!(
            "mnemosyne-import-fp-{}-{}",
            std::process::id(),
            message.is_some()
        ));
        let _ = fs::remove_dir_all(&root);
        let s = Service::open(&root).unwrap();
        let who = TrustedIdentity {
            tenant_id: "t".into(),
            actor_id: "h".into(),
            kind: auth::ActorKind::Human,
            on_behalf_of: None,
        };
        s.bootstrap_policy(Policy {
            tenants: Default::default(),
            grants: [Permission::Read, Permission::Edit]
                .into_iter()
                .map(|permission| auth::Grant {
                    tenant_id: "t".into(),
                    actor_id: "h".into(),
                    permission,
                    scope: "".into(),
                })
                .collect(),
        })
        .unwrap();
        let files = vec![ImportFile {
            path: "a.md".into(),
            bytes: b"a".to_vec(),
            directory: false,
        }];
        let p = s
            .import_preview(&who, "r0", files, 0, None, ConflictPolicy::Error)
            .unwrap();
        s.import_commit(&who, p, ImportMode::Edit, "k", message, None)
            .unwrap();
        let fp = s
            .transaction_for(&who, false, vec![], false, None, Some("k"), |st| {
                Ok(st.replays[&replay_key(&who, "k")?].fingerprint.clone())
            })
            .unwrap();
        let _ = fs::remove_dir_all(&root);
        fp
    }

    #[test]
    fn import_without_message_keeps_the_legacy_replay_fingerprint() {
        // SHA-256 of `["edit","r0",[{"type":"put","path":"a.md","content":"a"}]]`, the
        // legacy import fingerprint.
        assert_eq!(
            import_fingerprints(None),
            "19570146fc652231b01b0f307ad45483647358822d70fac1890dc7757f725011"
        );
        assert_ne!(
            import_fingerprints(Some("Imported")),
            "19570146fc652231b01b0f307ad45483647358822d70fac1890dc7757f725011"
        );
    }
}
