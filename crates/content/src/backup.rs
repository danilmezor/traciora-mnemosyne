//! Trusted local operator snapshots. These operations include every tenant and
//! are deliberately outside the tenant request API and authentication transport.
use crate::{err, io, storage, ContentError};
use fs2::FileExt;
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

type Result<T> = std::result::Result<T, ContentError>;
static NEXT: AtomicU64 = AtomicU64::new(0);
const MAX_MANIFEST_BYTES: usize = 64 * 1024 * 1024;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackupReport {
    pub version: u32,
    pub blob_count: u64,
    pub blob_bytes: u64,
    pub database_sha256: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    authority_version: u32,
    database_sha256: String,
    blobs: BTreeMap<String, u64>,
}
fn regular(path: &Path) -> Result<()> {
    if !fs::symlink_metadata(path)
        .map_err(io)?
        .file_type()
        .is_file()
    {
        return Err(err("invalid_backup_file"));
    }
    Ok(())
}
fn hash(path: &Path) -> Result<(String, u64)> {
    regular(path)?;
    let mut input = File::open(path).map_err(io)?;
    let mut hash = Sha256::new();
    let mut bytes = 0;
    let mut buf = [0u8; 65536];
    loop {
        let n = input.read(&mut buf).map_err(io)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
        bytes += n as u64;
    }
    Ok((format!("{:x}", hash.finalize()), bytes))
}
fn blob_name(id: &str) -> bool {
    id.len() == 64
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn sync_dir(path: &Path) -> Result<()> {
    File::open(path).and_then(|f| f.sync_all()).map_err(io)
}
fn private_dir(path: &Path) -> Result<()> {
    fs::create_dir(path).map_err(io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(io)?;
    }
    Ok(())
}
fn copy(from: &Path, to: &Path) -> Result<()> {
    regular(from)?;
    let mut input = File::open(from).map_err(io)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut output = options.open(to).map_err(io)?;
    std::io::copy(&mut input, &mut output).map_err(io)?;
    output.sync_all().map_err(io)
}
struct Stage(PathBuf);
impl Drop for Stage {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn destination(source: &Path, destination: &Path) -> Result<(PathBuf, Stage)> {
    let name = destination
        .file_name()
        .ok_or_else(|| err("invalid_backup_destination"))?;
    if name == "." || name == ".." {
        return Err(err("invalid_backup_destination"));
    }
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let parent = fs::canonicalize(parent).map_err(io)?;
    let dest = parent.join(name);
    if dest.starts_with(source) || source.starts_with(&dest) {
        return Err(err("invalid_backup_destination"));
    }
    if let Ok(meta) = fs::symlink_metadata(&dest) {
        if !meta.is_dir()
            || meta.file_type().is_symlink()
            || fs::read_dir(&dest).map_err(io)?.next().is_some()
        {
            return Err(err("destination_not_empty"));
        }
    }
    let stage = parent.join(format!(
        ".mnemosyne-stage-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    private_dir(&stage)?;
    Ok((dest, Stage(stage)))
}
fn publish(stage: &Stage, destination: &Path) -> Result<()> {
    sync_dir(&stage.0)?;
    failpoint("backup_before_publish");
    // Never remove a nonempty directory, file or symlink, even on a retry.
    if fs::symlink_metadata(destination).is_ok() {
        let meta = fs::symlink_metadata(destination).map_err(io)?;
        if !meta.is_dir() || meta.file_type().is_symlink() {
            return Err(err("destination_not_empty"));
        }
        fs::remove_dir(destination).map_err(|_| err("destination_not_empty"))?;
    }
    fs::rename(&stage.0, destination).map_err(io)?;
    sync_dir(destination.parent().unwrap())?;
    failpoint("backup_after_publish");
    Ok(())
}
fn failpoint(name: &str) {
    if std::env::var("MNEMOSYNE_CONTENT_FAILPOINT").as_deref() == Ok(name) {
        std::process::exit(86);
    }
}
fn report(m: &Manifest) -> BackupReport {
    BackupReport {
        version: m.version,
        blob_count: m.blobs.len() as u64,
        blob_bytes: m.blobs.values().sum(),
        database_sha256: m.database_sha256.clone(),
    }
}
fn validate(root: &Path, m: &Manifest) -> Result<()> {
    if m.version != 1 || ![2, 3, 4].contains(&m.authority_version) {
        return Err(err("unsupported_backup_version"));
    }
    if hash(&root.join("authority.sqlite3"))?.0 != m.database_sha256 {
        return Err(err("backup_integrity_error"));
    }
    let c = Connection::open_with_flags(
        root.join("authority.sqlite3"),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(io)?;
    let version: String = c
        .query_row("SELECT value FROM meta WHERE key='version'", [], |r| {
            r.get(0)
        })
        .map_err(io)?;
    if version != m.authority_version.to_string() {
        return Err(err("backup_integrity_error"));
    }
    for (id, size) in &m.blobs {
        if !blob_name(id) {
            return Err(err("invalid_blob_id"));
        }
        let (digest, actual) = hash(&root.join("blobs").join(id))?;
        if digest != *id || actual != *size {
            return Err(err("backup_integrity_error"));
        }
    }
    storage::validate_backup(root, &m.blobs.keys().cloned().collect())
}
/// Create a coherent whole-store backup from an initialized current SQLite store.
/// Source and output must be privately owned by the trusted operator.
pub fn create(source: impl AsRef<Path>, output: impl AsRef<Path>) -> Result<BackupReport> {
    let source = fs::canonicalize(source).map_err(io)?;
    let (output, stage) = destination(&source, output.as_ref())?;
    regular(&source.join("authority.sqlite3"))?;
    regular(&source.join("authority.lock"))?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(source.join("authority.lock"))
        .map_err(io)?;
    lock.lock_exclusive().map_err(io)?;
    let c = Connection::open_with_flags(
        source.join("authority.sqlite3"),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(io)?;
    let authority_version: String = c
        .query_row("SELECT value FROM meta WHERE key='version'", [], |r| {
            r.get(0)
        })
        .map_err(io)?;
    let authority_version = authority_version.parse::<u32>().map_err(io)?;
    if ![2, 3, 4].contains(&authority_version) {
        return Err(err("unsupported_store_version"));
    }
    c.execute(
        "VACUUM INTO ?1",
        [stage
            .0
            .join("authority.sqlite3")
            .to_str()
            .ok_or_else(|| err("invalid_backup_destination"))?],
    )
    .map_err(io)?;
    File::open(stage.0.join("authority.sqlite3"))
        .and_then(|f| f.sync_all())
        .map_err(io)?;
    private_dir(&stage.0.join("blobs"))?;
    let mut blobs = BTreeMap::new();
    if source.join("blobs").exists() {
        if fs::symlink_metadata(source.join("blobs"))
            .map_err(io)?
            .file_type()
            .is_symlink()
        {
            return Err(err("invalid_backup_file"));
        }
        for entry in fs::read_dir(source.join("blobs")).map_err(io)? {
            let entry = entry.map_err(io)?;
            let id = entry
                .file_name()
                .into_string()
                .map_err(|_| err("invalid_blob_id"))?;
            // Private CAS staging files are nonauthoritative and excluded.
            if id.starts_with('.') && id.ends_with(".next") {
                continue;
            }
            if !blob_name(&id) {
                return Err(err("invalid_blob_id"));
            }
            copy(&entry.path(), &stage.0.join("blobs").join(&id))?;
            let (digest, size) = hash(&stage.0.join("blobs").join(&id))?;
            if digest != id {
                return Err(err("backup_integrity_error"));
            }
            blobs.insert(id, size);
        }
    }
    let m = Manifest {
        version: 1,
        authority_version,
        database_sha256: hash(&stage.0.join("authority.sqlite3"))?.0,
        blobs,
    };
    validate(&stage.0, &m)?;
    let manifest = serde_json::to_vec_pretty(&m).map_err(io)?;
    if manifest.len() > MAX_MANIFEST_BYTES {
        return Err(err("backup_manifest_too_large"));
    }
    let mut f = File::create(stage.0.join("manifest.json")).map_err(io)?;
    f.write_all(&manifest).map_err(io)?;
    f.sync_all().map_err(io)?;
    sync_dir(&stage.0.join("blobs"))?;
    publish(&stage, &output)?;
    Ok(report(&m))
}
/// Validate and restore into a new or empty destination, publishing atomically.
/// Credentials must be provisioned separately by the trusted host.
pub fn restore(input: impl AsRef<Path>, output: impl AsRef<Path>) -> Result<BackupReport> {
    let input = fs::canonicalize(input).map_err(io)?;
    let (output, stage) = destination(&input, output.as_ref())?;
    regular(&input.join("manifest.json"))?;
    let blobs_meta = fs::symlink_metadata(input.join("blobs")).map_err(io)?;
    if !blobs_meta.is_dir() || blobs_meta.file_type().is_symlink() {
        return Err(err("invalid_backup_file"));
    }
    let mut bytes = Vec::new();
    File::open(input.join("manifest.json"))
        .map_err(io)?
        .take(MAX_MANIFEST_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(io)?;
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(err("backup_manifest_too_large"));
    }
    let m: Manifest = serde_json::from_slice(&bytes).map_err(io)?;
    if m.version != 1 || ![2, 3, 4].contains(&m.authority_version) {
        return Err(err("unsupported_backup_version"));
    }
    private_dir(&stage.0.join("blobs"))?;
    copy(
        &input.join("authority.sqlite3"),
        &stage.0.join("authority.sqlite3"),
    )?;
    for id in m.blobs.keys() {
        if !blob_name(id) {
            return Err(err("invalid_blob_id"));
        }
        copy(
            &input.join("blobs").join(id),
            &stage.0.join("blobs").join(id),
        )?;
    }
    validate(&stage.0, &m)?;
    sync_dir(&stage.0.join("blobs"))?;
    publish(&stage, &output)?;
    Ok(report(&m))
}
