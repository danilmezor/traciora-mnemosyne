//! Operator grants for an initialised store, and the lock that marks a
//! running `content-serve`.
//!
//! `serve.lock` is advisory and exists only so operator commands can tell that a
//! service is running. Correctness of concurrent writes never depends on it: every
//! domain write is one transaction under `authority.lock`, and the service re-reads
//! the policy inside each request's transaction, holding none in memory.
use crate::{
    auth::{Grant, Permission, Policy},
    caller_policy, err, fingerprint, io, reopen_approvals, validate_policy, ContentError, Result,
    Service,
};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::Path,
};

const LOCK_FILE: &str = "serve.lock";

/// An exclusive `flock` on `STORE/serve.lock`, held for a service's lifetime. The
/// kernel releases it when the holder exits, however it exits, so a lock left by a
/// dead process is free for the next one; the pid inside is only for messages.
pub struct ServeLock(File);

#[derive(Serialize, Deserialize)]
struct Holder {
    pid: u32,
    started_at: String,
}

fn open_lock(store: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(store.join(LOCK_FILE)).map_err(io)
}

/// `Ok(None)` when another process holds the lock.
fn try_lock(file: &File) -> Result<Option<()>> {
    match file.try_lock_exclusive() {
        Ok(()) => Ok(Some(())),
        Err(e) if e.raw_os_error() == fs2::lock_contended_error().raw_os_error() => Ok(None),
        Err(e) => Err(io(e)),
    }
}

/// `store_in_use`, naming the holder recorded in the lock file when it can be read.
fn in_use(store: &Path) -> ContentError {
    let mut text = String::new();
    let holder = File::open(store.join(LOCK_FILE))
        .and_then(|f| f.take(4096).read_to_string(&mut text))
        .ok()
        .and_then(|_| serde_json::from_str::<Holder>(&text).ok());
    ContentError::new(
        "store_in_use",
        match holder {
            Some(h) => format!(
                "A content service is running on this store (pid {}, started {})",
                h.pid, h.started_at
            ),
            None => "A content service is running on this store".into(),
        },
    )
}

impl ServeLock {
    /// Takes the lock without waiting, creating the store directory as `Service::open`
    /// does; `None` while a live process holds it.
    pub fn try_acquire(store: &Path) -> Result<Option<ServeLock>> {
        Service::open(store)?;
        let mut file = open_lock(store)?;
        if try_lock(&file)?.is_none() {
            return Ok(None);
        }
        let holder = Holder {
            pid: std::process::id(),
            started_at: crate::system_clock(),
        };
        file.set_len(0).map_err(io)?;
        file.write_all(&serde_json::to_vec(&holder).map_err(io)?)
            .map_err(io)?;
        Ok(Some(ServeLock(file)))
    }
    /// As `try_acquire`, but `store_in_use` (naming the holder's pid) when held. An
    /// operator's momentary probe (`add_grant`) is waited out briefly.
    pub fn acquire(store: &Path) -> Result<ServeLock> {
        for attempt in 0..5 {
            if let Some(lock) = Self::try_acquire(store)? {
                return Ok(lock);
            }
            if attempt < 4 {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
        Err(in_use(store))
    }
}
impl Drop for ServeLock {
    fn drop(&mut self) {
        // Still held here: clear the stale pid, then closing the file releases the lock.
        let _ = self.0.set_len(0);
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GrantReport {
    pub tenant_id: String,
    pub actor_id: String,
    pub permission: Permission,
    pub scope: String,
    pub added: bool,
    pub policy_digest: String,
}

fn initialised_files(store: &Path) -> bool {
    store.join("authority.sqlite3").is_file() || store.join("authority.json").is_file()
}

/// Adds one grant. Refuses `store_in_use` while a service holds
/// `serve.lock` unless `while_serving`; either way the write is one `authority.lock`
/// transaction, as `policy_set`'s, so a running service sees it on its next request.
pub fn add_grant(store: &Path, grant: Grant, while_serving: bool) -> Result<GrantReport> {
    // Never create a store here: a missing or empty directory is not initialised.
    if !initialised_files(store) {
        return Err(err("not_initialized"));
    }
    if !while_serving {
        // A momentary probe: released at once, so it never keeps a service from starting.
        let file = open_lock(store)?;
        if try_lock(&file)?.is_none() {
            return Err(in_use(store));
        }
    }
    let service = Service::open(store)?;
    service.transaction(true, |s| {
        if !s.initialized {
            return Err(err("not_initialized"));
        }
        validate_policy(&Policy {
            grants: vec![grant.clone()],
            tenants: Default::default(),
        })?;
        let added = !s.policy.grants.iter().any(|g| {
            g.tenant_id == grant.tenant_id
                && g.actor_id == grant.actor_id
                && g.permission == grant.permission
                && g.scope == grant.scope
        });
        if added {
            s.policy.grants.push(grant.clone());
            validate_policy(&s.policy)?;
            reopen_approvals(s, &grant.tenant_id);
        }
        Ok(GrantReport {
            policy_digest: fingerprint(&caller_policy(&s.policy, &grant.tenant_id))?,
            tenant_id: grant.tenant_id.clone(),
            actor_id: grant.actor_id.clone(),
            permission: grant.permission,
            scope: grant.scope.clone(),
            added,
        })
    })
}
