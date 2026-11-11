//! SQLite publication metadata and immutable content objects. Callers retain the
//! authority lock until the FULL synchronous database commit has completed.
use super::*;
use fs2::FileExt;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::fs::{File, OpenOptions};
use std::io::Write;

pub(crate) struct Scope<'a> {
    pub tenant: Option<&'a str>,
    pub revisions: Vec<&'a str>,
    pub history: bool,
    pub proposal: Option<&'a str>,
    pub replay: Option<&'a str>,
    /// False loads revision and proposal records as metadata only: no `before`/`after`
    /// texts, and proposals name no revisions to reconstruct (`log`).
    pub hydrate_text: bool,
    /// Further revision records to load as metadata only (no texts, no manifests): the
    /// chain from head back to a base, for the touched-path staleness walk.
    pub metadata: Vec<&'a str>,
}

pub(crate) fn put_blob(root: &Path, bytes: &[u8]) -> Result<String> {
    let id = format!("{:x}", Sha256::digest(bytes));
    let dir = root.join("blobs");
    if !dir.exists() {
        fs::create_dir(&dir).map_err(io)?;
        // Persist the directory's entry in its parent before a SQL transaction
        // can publish references to objects inside it.
        File::open(root).and_then(|f| f.sync_all()).map_err(io)?;
    }
    let path = dir.join(&id);
    if path.exists() {
        if read_blob(root, &id)? != bytes {
            return Err(err("corrupt_blob"));
        }
        return Ok(id);
    }
    // All domain writes hold authority.lock. A stale temp is never authoritative.
    let temp = dir.join(format!(".{id}.next"));
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temp)
        .map_err(io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(io)?;
    }
    file.write_all(bytes).map_err(io)?;
    file.sync_all().map_err(io)?;
    fs::rename(temp, path).map_err(io)?;
    File::open(&dir).and_then(|f| f.sync_all()).map_err(io)?;
    Ok(id)
}
fn check_blob_id(id: &str) -> Result<()> {
    if id.len() != 64
        || !id
            .bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    {
        return Err(err("invalid_blob_id"));
    }
    Ok(())
}
/// Stored size of a blob, without reading or verifying its bytes.
pub(crate) fn blob_len(root: &Path, id: &str) -> Result<u64> {
    check_blob_id(id)?;
    Ok(fs::metadata(root.join("blobs").join(id)).map_err(io)?.len())
}
pub(crate) fn read_blob(root: &Path, id: &str) -> Result<Vec<u8>> {
    check_blob_id(id)?;
    let bytes = fs::read(root.join("blobs").join(id)).map_err(io)?;
    if format!("{:x}", Sha256::digest(&bytes)) != id {
        return Err(err("corrupt_blob"));
    }
    Ok(bytes)
}
// Large strings in exact review/replay records are shared objects as well. The
// tagged representation is private storage and never escapes through the API.
fn pack(root: &Path, v: &mut Value) -> Result<()> {
    match v {
        Value::String(s) if s.len() > 1024 => {
            *v = json!({"$mnemosyne_blob":put_blob(root,s.as_bytes())?});
        }
        Value::Array(a)
            if a.len() > 1024 && a.iter().all(|x| x.as_u64().is_some_and(|n| n <= 255)) =>
        {
            let bytes = a
                .iter()
                .map(|x| x.as_u64().unwrap() as u8)
                .collect::<Vec<_>>();
            *v = json!({"$mnemosyne_binary":put_blob(root,&bytes)?});
        }
        Value::Array(a) => {
            for x in a {
                pack(root, x)?;
            }
        }
        Value::Object(o) => {
            for x in o.values_mut() {
                pack(root, x)?;
            }
        }
        _ => {}
    }
    Ok(())
}
fn unpack(root: &Path, v: &mut Value, hydrate: bool) -> Result<()> {
    if let Some(id) = v
        .as_object()
        .filter(|o| o.len() == 1)
        .and_then(|o| o.get("$mnemosyne_binary"))
        .and_then(Value::as_str)
    {
        *v = if hydrate {
            serde_json::to_value(read_blob(root, id)?).map_err(io)?
        } else {
            json!([])
        };
        return Ok(());
    }
    if let Some(id) = v
        .as_object()
        .filter(|o| o.len() == 1)
        .and_then(|o| o.get("$mnemosyne_blob"))
        .and_then(Value::as_str)
    {
        *v = Value::String(if hydrate {
            String::from_utf8(read_blob(root, id)?).map_err(io)?
        } else {
            String::new()
        });
    } else {
        match v {
            Value::Array(a) => {
                for x in a {
                    unpack(root, x, hydrate)?
                }
            }
            Value::Object(o) => {
                for x in o.values_mut() {
                    unpack(root, x, hydrate)?
                }
            }
            _ => {}
        }
    }
    Ok(())
}
fn encode(root: &Path, value: &impl Serialize) -> Result<String> {
    let mut v = serde_json::to_value(value).map_err(io)?;
    pack(root, &mut v)?;
    serde_json::to_string(&v).map_err(io)
}
fn decode<T: for<'de> Deserialize<'de>>(root: &Path, text: &str, hydrate: bool) -> Result<T> {
    let mut v: Value = serde_json::from_str(text).map_err(io)?;
    unpack(root, &mut v, hydrate)?;
    serde_json::from_value(v).map_err(io)
}

fn schema(c: &Connection) -> Result<()> {
    c.execute_batch("PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;
 CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);
 CREATE TABLE IF NOT EXISTS tenants(id TEXT PRIMARY KEY,sequence INTEGER NOT NULL,head TEXT NOT NULL);
 CREATE TABLE IF NOT EXISTS revisions(tenant TEXT NOT NULL,id TEXT NOT NULL,parent TEXT,data TEXT NOT NULL,PRIMARY KEY(tenant,id));
 CREATE TABLE IF NOT EXISTS entries(tenant TEXT NOT NULL,revision TEXT NOT NULL,path TEXT NOT NULL,data TEXT,PRIMARY KEY(tenant,revision,path));
 CREATE TABLE IF NOT EXISTS current_entries(tenant TEXT NOT NULL,path TEXT NOT NULL,data TEXT NOT NULL,PRIMARY KEY(tenant,path));
 CREATE TABLE IF NOT EXISTS proposals(tenant TEXT NOT NULL,id TEXT NOT NULL,data TEXT NOT NULL,PRIMARY KEY(tenant,id));
 CREATE TABLE IF NOT EXISTS replays(id TEXT PRIMARY KEY,data TEXT NOT NULL);
 CREATE TABLE IF NOT EXISTS identity_paths(tenant TEXT NOT NULL,file_id TEXT NOT NULL,path TEXT NOT NULL,PRIMARY KEY(tenant,file_id,path));").map_err(io)
}
pub(crate) fn has_table(c: &Connection, table: &str) -> Result<bool> {
    c.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [table],
        |r| r.get(0),
    )
    .map_err(io)
}
pub(crate) fn manifest(c: &Connection, tenant: &str, id: &str) -> Result<Value> {
    let mut chain = Vec::new();
    let mut cur = Some(id.to_string());
    let mut seen = BTreeSet::new();
    let checkpoints = has_table(c, "revision_checkpoints")?;
    let mut base = serde_json::Map::new();
    while let Some(r) = cur {
        if !seen.insert(r.clone()) {
            return Err(err("corrupt_revision_chain"));
        }
        if checkpoints {
            let checkpoint: Option<String> = c
                .query_row(
                    "SELECT data FROM revision_checkpoints WHERE tenant=?1 AND revision=?2",
                    params![tenant, r],
                    |row| row.get(0),
                )
                .optional()
                .map_err(io)?;
            if let Some(data) = checkpoint {
                base = serde_json::from_str::<Value>(&data)
                    .map_err(io)?
                    .as_object()
                    .cloned()
                    .ok_or_else(|| err("corrupt_revision_chain"))?;
                break;
            }
        }
        let parent = c
            .query_row(
                "SELECT parent FROM revisions WHERE tenant=?1 AND id=?2",
                params![tenant, r],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(io)?
            .ok_or_else(|| err("not_found"))?;
        chain.push(r);
        cur = parent;
    }
    let mut files = base;
    let mut q = c
        .prepare("SELECT path,data FROM entries WHERE tenant=?1 AND revision=?2 ORDER BY path")
        .map_err(io)?;
    for r in chain.iter().rev() {
        let rows = q
            .query_map(params![tenant, r], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })
            .map_err(io)?;
        for row in rows {
            let (p, d) = row.map_err(io)?;
            if let Some(d) = d {
                files.insert(p, serde_json::from_str(&d).map_err(io)?);
            } else {
                files.remove(&p);
            }
        }
    }
    Ok(Value::Object(files))
}
fn load(root: &Path, c: &Connection, scope: &Scope<'_>) -> Result<State> {
    let version: Option<String> = c
        .query_row("SELECT value FROM meta WHERE key='version'", [], |r| {
            r.get(0)
        })
        .optional()
        .map_err(io)?;
    let version = match version.as_deref() {
        Some("2") => 2,
        Some("3") => 3,
        Some("4") => 4,
        _ => return Err(err("unsupported_store_version")),
    };
    if version == 3
        && (!has_table(c, "revision_checkpoints")? || !has_table(c, "pruned_revisions")?)
    {
        return Err(err("unsupported_store_version"));
    }
    let policy: String = c
        .query_row("SELECT value FROM meta WHERE key='policy'", [], |r| {
            r.get(0)
        })
        .map_err(io)?;
    let initialized: String = c
        .query_row("SELECT value FROM meta WHERE key='initialized'", [], |r| {
            r.get(0)
        })
        .map_err(io)?;
    let mut state = State {
        version,
        policy: decode(root, &policy, true)?,
        initialized: initialized == "true",
        tenants: BTreeMap::new(),
        replays: BTreeMap::new(),
    };
    let mut tq = c
        .prepare("SELECT id,sequence,head FROM tenants WHERE (?1 IS NULL OR id=?1)")
        .map_err(io)?;
    let rows = tq
        .query_map([scope.tenant], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, u64>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
        .map_err(io)?;
    for row in rows {
        let (id, sequence, head) = row.map_err(io)?;
        let mut t = Tenant {
            sequence,
            head: head.clone(),
            ..Default::default()
        };
        if has_table(c, "pruned_revisions")? {
            let mut q = c
                .prepare("SELECT revision FROM pruned_revisions WHERE tenant=?1")
                .map_err(io)?;
            t.pruned_revisions = q
                .query_map([&id], |r| r.get::<_, String>(0))
                .map_err(io)?
                .collect::<std::result::Result<_, _>>()
                .map_err(io)?;
        }
        let mut pq = c
            .prepare("SELECT id,data FROM proposals WHERE tenant=?1 AND (?2 IS NULL OR id=?2)")
            .map_err(io)?;
        for row in pq
            .query_map(params![id, scope.proposal], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(io)?
        {
            let (pid, data) = row.map_err(io)?;
            t.proposals
                .insert(pid, decode(root, &data, scope.hydrate_text)?);
        }
        let mut lineage = c
            .prepare("SELECT file_id,path FROM identity_paths WHERE tenant=?1")
            .map_err(io)?;
        for row in lineage
            .query_map([&id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(io)?
        {
            let (file_id, path) = row.map_err(io)?;
            t.identity_paths.entry(file_id).or_default().insert(path);
        }
        let mut wanted = scope.revisions.clone();
        for p in t.proposals.values().filter(|_| scope.hydrate_text) {
            if let Some(revision_id) = &p.revision_id {
                wanted.push(revision_id);
            }
            for change in &p.changes {
                if let Change::Restore { revision_id, .. } = change {
                    wanted.push(revision_id);
                }
            }
        }
        wanted.push(&head);
        let revision_query = if scope.history {
            "SELECT id,data FROM revisions WHERE tenant=?1 AND ?2 IS NOT NULL"
        } else {
            "SELECT id,data FROM revisions WHERE tenant=?1 AND id IN (SELECT value FROM json_each(?2))"
        };
        let mut rq = c.prepare(revision_query).map_err(io)?;
        let listed: Vec<&str> = wanted.iter().chain(&scope.metadata).copied().collect();
        for row in rq
            .query_map(
                params![id, serde_json::to_string(&listed).map_err(io)?],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .map_err(io)?
        {
            let (rid, data) = row.map_err(io)?;
            let hydrate = scope.hydrate_text
                && (scope.history
                    || scope.revisions.contains(&rid.as_str())
                    || t.proposals
                        .values()
                        .any(|p| p.revision_id.as_deref() == Some(&rid)));
            // Authorization needs identity/path lineage, never historical text.
            // Only explicit history/diff requests hydrate historical payloads.
            let revision = if hydrate {
                decode(root, &data, true)?
            } else {
                let mut metadata: Value = serde_json::from_str(&data).map_err(io)?;
                if let Some(changes) = metadata["changes"].as_array_mut() {
                    for change in changes {
                        change["before"] = Value::Null;
                        change["after"] = Value::String(String::new());
                    }
                }
                // The message is metadata people read, kept even when packed as a blob.
                if let Some(message) = metadata.get_mut("message") {
                    unpack(root, message, true)?;
                }
                serde_json::from_value(metadata).map_err(io)?
            };
            let files = if rid == head {
                let mut map = serde_json::Map::new();
                let mut eq = c
                    .prepare("SELECT path,data FROM current_entries WHERE tenant=?1")
                    .map_err(io)?;
                for e in eq
                    .query_map([&id], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                    })
                    .map_err(io)?
                {
                    let (p, d) = e.map_err(io)?;
                    map.insert(p, serde_json::from_str(&d).map_err(io)?);
                }
                serde_json::from_value(Value::Object(map)).map_err(io)?
            } else if wanted.contains(&rid.as_str()) {
                serde_json::from_value(manifest(c, &id, &rid)?).map_err(io)?
            } else {
                BTreeMap::new()
            };
            t.revisions.insert(rid, Snapshot { revision, files });
        }
        state.tenants.insert(id, t);
    }
    if let Some(key) = scope.replay {
        if let Some(data) = c
            .query_row("SELECT data FROM replays WHERE id=?1", [key], |r| {
                r.get::<_, String>(0)
            })
            .optional()
            .map_err(io)?
        {
            state.replays.insert(key.into(), decode(root, &data, true)?);
        }
    }
    if scope.tenant.is_none() && scope.replay.is_none() {
        let mut q = c.prepare("SELECT id,data FROM replays").map_err(io)?;
        for row in q
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(io)?
        {
            let (k, d) = row.map_err(io)?;
            state.replays.insert(k, decode(root, &d, true)?);
        }
    }
    Ok(state)
}
fn save(root: &Path, c: &Connection, state: &State) -> Result<()> {
    for (key, value) in [
        ("version", "4".to_string()),
        ("policy", encode(root, &state.policy)?),
        ("initialized", state.initialized.to_string()),
    ] {
        c.execute("INSERT INTO meta(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value WHERE value<>excluded.value",params![key,value]).map_err(io)?;
    }
    for (tid, t) in &state.tenants {
        let old_head: Option<String> = c
            .query_row("SELECT head FROM tenants WHERE id=?1", [tid], |r| r.get(0))
            .optional()
            .map_err(io)?;
        c.execute("INSERT INTO tenants(id,sequence,head) VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET sequence=excluded.sequence,head=excluded.head WHERE sequence<>excluded.sequence OR head<>excluded.head",params![tid,t.sequence,t.head]).map_err(io)?;
        // Parent order matters when migration imports many immutable revisions.
        let mut pending: BTreeSet<String> = t.revisions.keys().cloned().collect();
        while !pending.is_empty() {
            let mut progress = false;
            for rid in pending.clone() {
                let snap = &t.revisions[&rid];
                let exists: bool = c
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM revisions WHERE tenant=?1 AND id=?2)",
                        params![tid, rid],
                        |r| r.get(0),
                    )
                    .map_err(io)?;
                if exists {
                    pending.remove(&rid);
                    progress = true;
                    continue;
                }
                if let Some(parent) = &snap.revision.parent {
                    if pending.contains(parent) {
                        continue;
                    }
                }
                let parent_files = match &snap.revision.parent {
                    Some(p) if old_head.as_ref() == Some(p) => {
                        let mut map = serde_json::Map::new();
                        let mut q = c
                            .prepare("SELECT path,data FROM current_entries WHERE tenant=?1")
                            .map_err(io)?;
                        for row in q
                            .query_map([tid], |r| {
                                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                            })
                            .map_err(io)?
                        {
                            let (path, data) = row.map_err(io)?;
                            map.insert(path, serde_json::from_str(&data).map_err(io)?);
                        }
                        Value::Object(map)
                    }
                    Some(p) => manifest(c, tid, p)?,
                    None => json!({}),
                };
                let files = serde_json::to_value(&snap.files).map_err(io)?;
                let before = parent_files.as_object().unwrap();
                let after = files.as_object().unwrap();
                c.execute(
                    "INSERT INTO revisions(tenant,id,parent,data) VALUES(?1,?2,?3,?4)",
                    params![
                        tid,
                        rid,
                        snap.revision.parent,
                        encode(root, &snap.revision)?
                    ],
                )
                .map_err(io)?;
                for change in &snap.revision.changes {
                    if let Some(file_id) = &change.file_id {
                        for path in std::iter::once(&change.path).chain(change.previous_path.iter())
                        {
                            c.execute("INSERT OR IGNORE INTO identity_paths(tenant,file_id,path) VALUES(?1,?2,?3)",params![tid,file_id,path]).map_err(io)?;
                        }
                    }
                }
                let paths: BTreeSet<_> = before.keys().chain(after.keys()).collect();
                for path in paths {
                    if before.get(path) != after.get(path) {
                        let data = after
                            .get(path)
                            .map(serde_json::to_string)
                            .transpose()
                            .map_err(io)?;
                        c.execute(
                            "INSERT INTO entries(tenant,revision,path,data) VALUES(?1,?2,?3,?4)",
                            params![tid, rid, path, data],
                        )
                        .map_err(io)?;
                    }
                }
                pending.remove(&rid);
                progress = true;
            }
            if !progress {
                return Err(err("corrupt_revision_chain"));
            }
        }
        // Only changed current entries are written; no full manifest rewrite.
        if let Some(head) = t.revisions.get(&t.head) {
            let files = serde_json::to_value(&head.files).map_err(io)?;
            let files = files.as_object().unwrap();
            let mut q = c
                .prepare("SELECT path FROM current_entries WHERE tenant=?1")
                .map_err(io)?;
            let paths = q
                .query_map([tid], |r| r.get::<_, String>(0))
                .map_err(io)?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(io)?;
            for p in paths {
                if !files.contains_key(&p) {
                    c.execute(
                        "DELETE FROM current_entries WHERE tenant=?1 AND path=?2",
                        params![tid, p],
                    )
                    .map_err(io)?;
                }
            }
            for (p, v) in files {
                let d = serde_json::to_string(v).map_err(io)?;
                c.execute("INSERT INTO current_entries(tenant,path,data) VALUES(?1,?2,?3) ON CONFLICT(tenant,path) DO UPDATE SET data=excluded.data WHERE data<>excluded.data",params![tid,p,d]).map_err(io)?;
            }
        }
        for (pid, p) in &t.proposals {
            let data = encode(root, p)?;
            c.execute("INSERT INTO proposals(tenant,id,data) VALUES(?1,?2,?3) ON CONFLICT(tenant,id) DO UPDATE SET data=excluded.data WHERE data<>excluded.data",params![tid,pid,data]).map_err(io)?;
        }
    }
    for (key, r) in &state.replays {
        let data = encode(root, r)?;
        c.execute("INSERT INTO replays(id,data) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET data=excluded.data WHERE data<>excluded.data",params![key,data]).map_err(io)?;
    }
    Ok(())
}
pub(crate) fn transaction<T>(
    root: &Path,
    write: bool,
    scope: Scope<'_>,
    f: impl FnOnce(&mut State) -> Result<T>,
) -> Result<T> {
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join("authority.lock"))
        .map_err(io)?;
    lock.lock_exclusive().map_err(io)?;
    let mut c = Connection::open(root.join("authority.sqlite3")).map_err(io)?;
    schema(&c)?;
    let exists: bool = c
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM meta WHERE key='version')",
            [],
            |r| r.get(0),
        )
        .map_err(io)?;
    if !exists {
        let state = match fs::read(root.join("authority.json")) {
            Ok(bytes) => {
                let v = serde_json::from_slice(&bytes).map_err(io)?;
                super::migrate_v1(root, v)?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(e) => return Err(io(e)),
        };
        let tx = c.transaction().map_err(io)?;
        save(root, &tx, &state)?;
        // Verify migrated complete logical state, including stable review and replay IDs.
        let check = load(
            root,
            &tx,
            &Scope {
                tenant: None,
                revisions: vec![],
                history: true,
                proposal: None,
                replay: None,
                hydrate_text: true,
                metadata: vec![],
            },
        )?;
        if serde_json::to_value(&check.policy).map_err(io)?
            != serde_json::to_value(&state.policy).map_err(io)?
            || check.tenants.len() != state.tenants.len()
        {
            return Err(err("migration_verification_failed"));
        }
        if serde_json::to_value(&check.replays).map_err(io)?
            != serde_json::to_value(&state.replays).map_err(io)?
            || check.initialized != state.initialized
        {
            return Err(err("migration_verification_failed"));
        }
        for (tid, t) in &state.tenants {
            let got = &check.tenants[tid];
            if got.identity_paths != t.identity_paths
                || got.head != t.head
                || got.sequence != t.sequence
                || serde_json::to_value(&got.proposals).map_err(io)?
                    != serde_json::to_value(&t.proposals).map_err(io)?
                || got.revisions.len() != t.revisions.len()
            {
                return Err(err("migration_verification_failed"));
            }
            for (rid, snap) in &t.revisions {
                if serde_json::to_value(&snap.files).map_err(io)? != manifest(&tx, tid, rid)?
                    || serde_json::to_value(&snap.revision).map_err(io)?
                        != serde_json::to_value(&got.revisions[rid].revision).map_err(io)?
                {
                    return Err(err("migration_verification_failed"));
                }
            }
        }
        failpoint("migration_before_commit");
        tx.commit().map_err(io)?;
        failpoint("migration_after_commit");
        File::open(root).and_then(|f| f.sync_all()).map_err(io)?;
    }
    let tx = c.transaction().map_err(io)?;
    let mut state = load(root, &tx, &scope)?;
    let result = f(&mut state)?;
    if write {
        save(root, &tx, &state)?;
        failpoint("before_sql_commit");
        tx.commit().map_err(io)?;
        failpoint("after_sql_commit");
    }
    Ok(result)
}
fn failpoint(name: &str) {
    if std::env::var("MNEMOSYNE_CONTENT_FAILPOINT").as_deref() == Ok(name) {
        std::process::exit(86)
    }
}

/// Read-only validation for operator snapshots, including payloads not selected
/// by ordinary scoped requests. Never create, migrate or repair an input here.
pub(crate) fn validate_backup(root: &Path, blobs: &BTreeSet<String>) -> Result<()> {
    let c = Connection::open_with_flags(
        root.join("authority.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(io)?;
    c.execute_batch("PRAGMA trusted_schema=OFF; PRAGMA query_only=ON;")
        .map_err(io)?;
    let integrity: String = c
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .map_err(io)?;
    if integrity != "ok" {
        return Err(err("backup_integrity_error"));
    }
    fn refs(v: &Value, blobs: &BTreeSet<String>) -> Result<()> {
        match v {
            Value::Object(o) => {
                for (key, value) in o {
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
                            if !blobs.contains(id) {
                                return Err(err("backup_missing_blob"));
                            }
                        }
                    }
                    refs(value, blobs)?;
                }
            }
            Value::Array(a) => {
                for value in a {
                    refs(value, blobs)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    // Both packed review/replay payloads and original/current/historical blob
    // references are required, even when no current read can reach them.
    for table in [
        "revisions",
        "entries",
        "current_entries",
        "proposals",
        "replays",
        "meta",
    ] {
        let column = if table == "meta" { "value" } else { "data" };
        let mut q = c
            .prepare(&format!(
                "SELECT {column} FROM {table} WHERE {column} IS NOT NULL"
            ))
            .map_err(io)?;
        for row in q.query_map([], |r| r.get::<_, String>(0)).map_err(io)? {
            let text = row.map_err(io)?;
            if table == "meta" {
                if let Ok(value) = serde_json::from_str::<Value>(&text) {
                    refs(&value, blobs)?;
                }
            } else {
                refs(&serde_json::from_str::<Value>(&text).map_err(io)?, blobs)?;
            }
        }
    }
    if has_table(&c, "revision_checkpoints")? {
        let mut q = c
            .prepare("SELECT data FROM revision_checkpoints")
            .map_err(io)?;
        for row in q.query_map([], |r| r.get::<_, String>(0)).map_err(io)? {
            refs(
                &serde_json::from_str::<Value>(&row.map_err(io)?).map_err(io)?,
                blobs,
            )?;
        }
    }
    let state = load(
        root,
        &c,
        &Scope {
            tenant: None,
            revisions: vec![],
            history: true,
            proposal: None,
            replay: None,
            hydrate_text: true,
            metadata: vec![],
        },
    )?;
    for (tid, tenant) in &state.tenants {
        // A tenant with pending proposals but no publications has an empty
        // persisted head; r0 is the API-only initial revision sentinel.
        if !(tenant.head.is_empty() && tenant.revisions.is_empty())
            && !tenant.revisions.contains_key(&tenant.head)
        {
            return Err(err("corrupt_revision_chain"));
        }
        for rid in tenant.revisions.keys() {
            // Reconstruct every historical manifest, detecting cycles and missing parents.
            let files = manifest(&c, tid, rid)?;
            for path in files
                .as_object()
                .ok_or_else(|| err("backup_integrity_error"))?
                .keys()
            {
                valid_path(path)?;
            }
            if rid == &tenant.head
                && files != serde_json::to_value(&tenant.revisions[rid].files).map_err(io)?
            {
                return Err(err("backup_integrity_error"));
            }
        }
    }
    Ok(())
}
