//! Explicit, trusted local operator retention. No tenant API or automatic policy.
//! Metadata publication precedes orphan deletion under the authority lock.
use crate::{err, fingerprint, io, storage, Result, TrustedIdentity};
use fs2::FileExt;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::Read,
    path::Path,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RetentionPolicy {
    pub tenant_id: String,
    pub keep_revisions: usize,
    #[serde(default)]
    pub expire_pruned_edit_replays: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetentionReport {
    pub version: u32,
    pub policy: RetentionPolicy,
    pub plan_digest: String,
    pub applied: bool,
    pub revisions_before: u64,
    pub retained_revisions: u64,
    pub pruned_revision_ids: Vec<String>,
    pub pinned_revision_ids: Vec<String>,
    pub checkpoint_revision_ids: Vec<String>,
    pub expired_replay_count: u64,
    pub retained_blob_bytes: u64,
    pub orphan_blob_count: u64,
    pub orphan_blob_bytes: u64,
    pub deleted_blob_count: u64,
    pub deleted_blob_bytes: u64,
}
struct Plan {
    report: RetentionReport,
    checkpoints: BTreeMap<String, Value>,
    expire: BTreeMap<String, Value>,
    orphans: BTreeMap<String, u64>,
}
fn blob_name(id: &str) -> bool {
    id.len() == 64
        && id
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
fn database_hash(path: &Path) -> Result<String> {
    let mut file = File::open(path).map_err(io)?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let n = file.read(&mut buffer).map_err(io)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
fn references(value: &Value, ids: &mut BTreeSet<String>) -> Result<()> {
    match value {
        Value::Object(fields) => {
            for (key, value) in fields {
                if [
                    "blob",
                    "original_blob",
                    "before_blob",
                    "after_blob",
                    "$mnemosyne_blob",
                    "$mnemosyne_binary",
                ]
                .contains(&key.as_str())
                {
                    if let Some(id) = value.as_str() {
                        if !blob_name(id) {
                            return Err(err("invalid_blob_id"));
                        }
                        ids.insert(id.to_owned());
                    }
                }
                references(value, ids)?;
            }
        }
        Value::Array(values) => {
            for value in values {
                references(value, ids)?;
            }
        }
        _ => (),
    }
    Ok(())
}
fn inventory(root: &Path) -> Result<BTreeMap<String, u64>> {
    let dir = root.join("blobs");
    let mut result = BTreeMap::new();
    match fs::symlink_metadata(&dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(result),
        Err(e) => return Err(io(e)),
        Ok(m) if !m.is_dir() || m.file_type().is_symlink() => {
            return Err(err("invalid_retention_file"))
        }
        _ => (),
    }
    for file in fs::read_dir(dir).map_err(io)? {
        let file = file.map_err(io)?;
        let name = file.file_name();
        let Some(name) = name.to_str().filter(|s| blob_name(s)) else {
            continue;
        };
        let metadata = fs::symlink_metadata(file.path()).map_err(io)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(err("invalid_retention_file"));
        }
        // Check content, including unreachable candidates, before authorizing deletion.
        let bytes = storage::read_blob(root, name)?;
        result.insert(name.into(), bytes.len() as u64);
    }
    Ok(result)
}
fn plan(root: &Path, c: &Connection, policy: RetentionPolicy) -> Result<Plan> {
    if policy.keep_revisions == 0
        || policy.tenant_id.is_empty()
        || policy.tenant_id.len() > 256
        || policy.tenant_id.chars().any(char::is_control)
    {
        return Err(err("invalid_retention_policy"));
    }
    let version: Option<String> = c
        .query_row("SELECT value FROM meta WHERE key='version'", [], |r| {
            r.get(0)
        })
        .optional()
        .map_err(io)?;
    if !matches!(version.as_deref(), Some("2" | "3" | "4")) {
        return Err(err("unsupported_store_version"));
    }
    let head: String = c
        .query_row(
            "SELECT head FROM tenants WHERE id=?1",
            [&policy.tenant_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(io)?
        .ok_or_else(|| err("not_found"))?;
    let mut revisions = BTreeMap::<String, Option<String>>::new();
    let mut q = c
        .prepare("SELECT id,parent FROM revisions WHERE tenant=?1")
        .map_err(io)?;
    for row in q
        .query_map([&policy.tenant_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        })
        .map_err(io)?
    {
        let (id, parent) = row.map_err(io)?;
        revisions.insert(id, parent);
    }
    let mut keep = BTreeSet::new();
    let mut at = (!head.is_empty()).then_some(head.clone());
    while let Some(id) = at {
        if keep.len() >= policy.keep_revisions {
            break;
        }
        let Some(parent) = revisions.get(&id) else {
            break;
        };
        if !keep.insert(id.clone()) {
            return Err(err("corrupt_revision_chain"));
        }
        at = parent.clone();
    }
    if !head.is_empty() && !keep.contains(&head) {
        return Err(err("corrupt_revision_chain"));
    }
    let mut pins = BTreeSet::new();
    let mut q = c
        .prepare("SELECT data FROM proposals WHERE tenant=?1")
        .map_err(io)?;
    for row in q
        .query_map([&policy.tenant_id], |r| r.get::<_, String>(0))
        .map_err(io)?
    {
        // Payload fields relevant to revision roots are short and never packed.
        let value: Value = serde_json::from_str(&row.map_err(io)?).map_err(io)?;
        if let Some(id) = value["base_revision"].as_str().filter(|id| *id != "r0") {
            pins.insert(id.to_owned());
        }
        if let Some(id) = value["revision_id"].as_str() {
            pins.insert(id.to_owned());
        }
        if let Some(changes) = value["changes"].as_array() {
            for change in changes {
                if change["type"] == "restore" {
                    if let Some(id) = change["revision_id"].as_str() {
                        pins.insert(id.to_owned());
                    }
                }
            }
        }
    }
    if pins.iter().any(|id| !revisions.contains_key(id)) {
        return Err(err("corrupt_revision_chain"));
    }
    keep.extend(pins.iter().cloned());
    let prune: BTreeSet<String> = revisions
        .keys()
        .filter(|id| !keep.contains(*id))
        .cloned()
        .collect();
    let mut checkpoints = BTreeMap::new();
    for id in &keep {
        if revisions[id]
            .as_ref()
            .is_some_and(|parent| !keep.contains(parent))
        {
            checkpoints.insert(id.clone(), storage::manifest(c, &policy.tenant_id, id)?);
        }
    }
    let objects = inventory(root)?;
    // Validate the entire original authority before touching it, including other tenants.
    storage::validate_backup(root, &objects.keys().cloned().collect())?;
    let mut expire = BTreeMap::new();
    if policy.expire_pruned_edit_replays {
        let mut expired_revisions = prune.clone();
        if storage::has_table(c, "pruned_revisions")? {
            let mut q = c
                .prepare("SELECT revision FROM pruned_revisions WHERE tenant=?1")
                .map_err(io)?;
            for row in q
                .query_map([&policy.tenant_id], |r| r.get::<_, String>(0))
                .map_err(io)?
            {
                expired_revisions.insert(row.map_err(io)?);
            }
        }
        let mut q = c.prepare("SELECT id,data FROM replays").map_err(io)?;
        for row in q
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(io)?
        {
            let (id, data) = row.map_err(io)?;
            let (who, _): (TrustedIdentity, String) = serde_json::from_str(&id).map_err(io)?;
            let mut value: Value = serde_json::from_str(&data).map_err(io)?;
            let result = &value["result"];
            if who.tenant_id == policy.tenant_id
                && result.get("proposal_id") == Some(&Value::Null)
                && result.get("actor").is_some()
                && result.get("changes").is_some()
                && result["id"]
                    .as_str()
                    .is_some_and(|id| expired_revisions.contains(id))
            {
                value["result"] = json!({"$mnemosyne_replay_expired":true});
                expire.insert(id, value);
            }
        }
    }
    let mut live = BTreeSet::new();
    for table in ["revisions", "entries", "current_entries", "proposals"] {
        let rev_column = if table == "revisions" {
            "id"
        } else if table == "entries" {
            "revision"
        } else {
            "''"
        };
        let mut q = c
            .prepare(&format!(
                "SELECT tenant,{rev_column},data FROM {table} WHERE data IS NOT NULL"
            ))
            .map_err(io)?;
        for row in q
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .map_err(io)?
        {
            let (tenant, revision, data) = row.map_err(io)?;
            if tenant == policy.tenant_id && prune.contains(&revision) {
                continue;
            }
            references(
                &serde_json::from_str::<Value>(&data).map_err(io)?,
                &mut live,
            )?;
        }
    }
    let mut q = c.prepare("SELECT id,data FROM replays").map_err(io)?;
    for row in q
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .map_err(io)?
    {
        let (id, data) = row.map_err(io)?;
        let value = match expire.get(&id) {
            Some(v) => v.clone(),
            None => serde_json::from_str(&data).map_err(io)?,
        };
        references(&value, &mut live)?;
    }
    let mut q = c.prepare("SELECT value FROM meta").map_err(io)?;
    for row in q.query_map([], |r| r.get::<_, String>(0)).map_err(io)? {
        if let Ok(v) = serde_json::from_str::<Value>(&row.map_err(io)?) {
            references(&v, &mut live)?;
        }
    }
    if storage::has_table(c, "revision_checkpoints")? {
        let mut q = c
            .prepare("SELECT tenant,revision,data FROM revision_checkpoints")
            .map_err(io)?;
        for row in q
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .map_err(io)?
        {
            let (tenant, id, data) = row.map_err(io)?;
            if tenant == policy.tenant_id && (prune.contains(&id) || checkpoints.contains_key(&id))
            {
                continue;
            }
            references(
                &serde_json::from_str::<Value>(&data).map_err(io)?,
                &mut live,
            )?;
        }
    }
    for files in checkpoints.values() {
        references(files, &mut live)?;
    }
    if live.iter().any(|id| !objects.contains_key(id)) {
        return Err(err("backup_missing_blob"));
    }
    let orphans: BTreeMap<String, u64> = objects
        .iter()
        .filter(|(id, _)| !live.contains(*id))
        .map(|(id, size)| (id.clone(), *size))
        .collect();
    let database_hash = database_hash(&root.join("authority.sqlite3"))?;
    let digest = fingerprint(&(&policy, database_hash, &objects))?;
    let report = RetentionReport {
        version: 1,
        policy,
        plan_digest: digest,
        applied: false,
        revisions_before: revisions.len() as u64,
        retained_revisions: keep.len() as u64,
        pruned_revision_ids: prune.into_iter().collect(),
        pinned_revision_ids: pins.into_iter().collect(),
        checkpoint_revision_ids: checkpoints.keys().cloned().collect(),
        expired_replay_count: expire.len() as u64,
        retained_blob_bytes: objects
            .iter()
            .filter(|(id, _)| live.contains(*id))
            .map(|(_, size)| size)
            .sum(),
        orphan_blob_count: orphans.len() as u64,
        orphan_blob_bytes: orphans.values().sum(),
        deleted_blob_count: 0,
        deleted_blob_bytes: 0,
    };
    Ok(Plan {
        report,
        checkpoints,
        expire,
        orphans,
    })
}
fn run(root: &Path, policy: RetentionPolicy, expected: Option<&str>) -> Result<RetentionReport> {
    let root = fs::canonicalize(root).map_err(io)?;
    for name in ["authority.sqlite3", "authority.lock"] {
        let metadata = fs::symlink_metadata(root.join(name)).map_err(io)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(err("invalid_retention_file"));
        }
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(root.join("authority.lock"))
        .map_err(io)?;
    lock.lock_exclusive().map_err(io)?;
    let flags = if expected.is_some() {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    } else {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    };
    let mut c = Connection::open_with_flags(root.join("authority.sqlite3"), flags).map_err(io)?;
    let journal_mode: String = c
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .map_err(io)?;
    if journal_mode != "delete" {
        return Err(err("unsupported_retention_journal_mode"));
    }
    c.execute_batch("PRAGMA trusted_schema=OFF; PRAGMA synchronous=FULL;")
        .map_err(io)?;
    let mut plan = plan(&root, &c, policy)?;
    let Some(expected) = expected else {
        return Ok(plan.report);
    };
    if expected != plan.report.plan_digest {
        return Err(err("retention_plan_conflict"));
    }
    let tx = c.transaction().map_err(io)?;
    tx.execute_batch("CREATE TABLE IF NOT EXISTS revision_checkpoints(tenant TEXT NOT NULL,revision TEXT NOT NULL,data TEXT NOT NULL,PRIMARY KEY(tenant,revision)); CREATE TABLE IF NOT EXISTS pruned_revisions(tenant TEXT NOT NULL,revision TEXT NOT NULL,PRIMARY KEY(tenant,revision));").map_err(io)?;
    tx.execute(
        "UPDATE meta SET value='3' WHERE key='version' AND value='2'",
        [],
    )
    .map_err(io)?;
    for (id, files) in &plan.checkpoints {
        tx.execute("INSERT INTO revision_checkpoints(tenant,revision,data) VALUES(?1,?2,?3) ON CONFLICT(tenant,revision) DO UPDATE SET data=excluded.data",params![plan.report.policy.tenant_id,id,serde_json::to_string(files).map_err(io)?]).map_err(io)?;
    }
    for id in &plan.report.pruned_revision_ids {
        for sql in [
            "DELETE FROM entries WHERE tenant=?1 AND revision=?2",
            "DELETE FROM revisions WHERE tenant=?1 AND id=?2",
            "DELETE FROM revision_checkpoints WHERE tenant=?1 AND revision=?2",
        ] {
            tx.execute(sql, params![plan.report.policy.tenant_id, id])
                .map_err(io)?;
        }
        tx.execute(
            "INSERT OR IGNORE INTO pruned_revisions(tenant,revision) VALUES(?1,?2)",
            params![plan.report.policy.tenant_id, id],
        )
        .map_err(io)?;
    }
    for (id, value) in &plan.expire {
        tx.execute(
            "UPDATE replays SET data=?2 WHERE id=?1",
            params![id, serde_json::to_string(value).map_err(io)?],
        )
        .map_err(io)?;
    }
    failpoint("retention_before_commit");
    tx.commit().map_err(io)?;
    failpoint("retention_after_metadata_commit");
    for (id, size) in &plan.orphans {
        fs::remove_file(root.join("blobs").join(id)).map_err(io)?;
        plan.report.deleted_blob_count += 1;
        plan.report.deleted_blob_bytes += size;
        failpoint("retention_during_gc");
    }
    if root.join("blobs").exists() {
        File::open(root.join("blobs"))
            .and_then(|f| f.sync_all())
            .map_err(io)?;
    }
    File::open(&root).and_then(|f| f.sync_all()).map_err(io)?;
    plan.report.applied = true;
    Ok(plan.report)
}
fn failpoint(name: &str) {
    if std::env::var("MNEMOSYNE_CONTENT_FAILPOINT").as_deref() == Ok(name) {
        std::process::exit(86);
    }
}
/// Plan explicitly, without creating schema, pruning metadata or deleting objects.
pub fn dry_run(root: impl AsRef<Path>, policy: RetentionPolicy) -> Result<RetentionReport> {
    run(root.as_ref(), policy, None)
}
/// Apply only a matching reviewed plan. Concurrent mutations require another dry run.
pub fn apply(
    root: impl AsRef<Path>,
    policy: RetentionPolicy,
    expected_plan: &str,
) -> Result<RetentionReport> {
    run(root.as_ref(), policy, Some(expected_plan))
}
