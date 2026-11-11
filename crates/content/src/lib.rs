//! Managed content authority. Content is published only by atomic state replacement;
//! there is deliberately no editable vault tree or legacy filesystem write adapter.
pub mod api;
pub mod auth;
pub mod backup;
pub mod extraction;
pub mod extraction_api;
pub mod find;
mod fuzzy;
pub mod grant;
pub mod graph;
pub mod knowledge;
pub mod links;
pub mod log;
pub mod onboarding;
mod outline_text;
pub mod proposals;
pub mod ranks;
pub mod read_many;
pub mod render;
pub mod retention;
pub mod search;
pub mod stat;
mod storage;
#[cfg(unix)]
pub mod transport;

use auth::{ActorKind, Permission, Policy, TrustedIdentity};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContentError {
    pub code: String,
    pub message: String,
}
impl ContentError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}
impl std::fmt::Display for ContentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for ContentError {}
type Result<T> = std::result::Result<T, ContentError>;
fn io(e: impl std::fmt::Display) -> ContentError {
    ContentError::new("storage_error", e.to_string())
}
fn err(code: &str) -> ContentError {
    ContentError::new(code, code.replace('_', " "))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Change {
    Put {
        path: String,
        content: String,
    },
    PutBinary {
        path: String,
        bytes: Vec<u8>,
    },
    CreateFolder {
        path: String,
    },
    Delete {
        path: String,
    },
    Rename {
        path: String,
        to: String,
    },
    Restore {
        path: String,
        revision_id: String,
    },
    /// Sets the path's own `seen_from` and/or `colour`; an absent field is left as it is.
    SetMeta {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seen_from: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        colour: Option<String>,
    },
}
impl Change {
    fn path(&self) -> &str {
        match self {
            Self::Put { path, .. }
            | Self::PutBinary { path, .. }
            | Self::CreateFolder { path }
            | Self::Delete { path }
            | Self::Rename { path, .. }
            | Self::Restore { path, .. }
            | Self::SetMeta { path, .. } => path,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProposalState {
    Pending,
    Approved,
    Rejected,
    Applied,
    Withdrawn,
    /// Rebased onto a newer head; `supersedes` on the new proposal names this one.
    Superseded,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Proposal {
    pub id: String,
    pub tenant_id: String,
    pub proposer: TrustedIdentity,
    pub base_revision: String,
    pub changes: Vec<Change>,
    pub diff: Vec<FileDiff>,
    pub digest: String,
    pub state: ProposalState,
    pub approver: Option<TrustedIdentity>,
    pub decisions: Vec<ReviewDecision>,
    pub revision_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<Origin>,
    /// The proposal this one was rebased from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewDecision {
    pub actor: TrustedIdentity,
    pub approved: bool,
    pub policy_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}
/// Who drafted a change on a person's behalf. Only an assistant is recorded today.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Origin {
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_id: Option<String>,
}
/// Answers the current time as RFC 3339 UTC with millisecond precision.
pub type Clock = Box<dyn Fn() -> String + Send + Sync>;
fn system_clock() -> String {
    format_at(time::OffsetDateTime::now_utc())
}
/// Always `YYYY-MM-DDTHH:MM:SS.mmmZ` (24 characters), truncated to milliseconds, in UTC.
pub(crate) fn format_at(t: time::OffsetDateTime) -> String {
    let t = t.to_offset(time::UtcOffset::UTC);
    let t = t
        .replace_nanosecond(t.nanosecond() / 1_000_000 * 1_000_000)
        .expect("a truncated nanosecond is in range");
    t.format(time::macros::format_description!(
        "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
    ))
    .expect("a UTC timestamp formats as RFC 3339")
}
const MAX_MESSAGE: usize = 2048;
/// A commit message or decision reason: bounded, printable, newlines allowed.
fn validate_message(text: Option<&str>) -> Result<()> {
    match text {
        Some(t) if t.len() > MAX_MESSAGE || t.chars().any(|c| c.is_control() && c != '\n') => {
            Err(err("invalid_message"))
        }
        _ => Ok(()),
    }
}
fn validate_origin(origin: Option<&Origin>) -> Result<()> {
    match origin {
        Some(o)
            if o.source != "assistant"
                || o.model.as_ref().is_some_and(|m| m.len() > 128)
                || o.chat_id.as_ref().is_some_and(|c| c.len() > 256) =>
        {
            Err(err("invalid_origin"))
        }
        _ => Ok(()),
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileDiff {
    pub path: String,
    pub before: Option<String>,
    pub after: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_path: Option<String>,
    #[serde(default)]
    pub deleted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_blob: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_blob: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<EntryKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta_before: Option<ranks::PathMeta>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta_after: Option<ranks::PathMeta>,
    /// Text lines added and removed; `None` for binary, folder, rename-only and meta-only
    /// diffs and for records written before they were kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added_lines: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removed_lines: Option<u32>,
}
/// What a revision was, derived from its changes when it is published.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RevisionKind {
    Edit,
    Proposal,
    Restore,
    Rename,
    Meta,
    Import,
    NewVersion,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Revision {
    pub id: String,
    pub parent: Option<String>,
    pub actor: TrustedIdentity,
    pub proposal_id: Option<String>,
    pub changes: Vec<FileDiff>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<Origin>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<RevisionKind>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadResult {
    pub revision_id: String,
    pub path: String,
    pub content: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    Text,
    Binary,
    Folder,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub file_id: String,
    pub kind: EntryKind,
    pub blob: Option<String>,
    pub original_blob: Option<String>,
    #[serde(default, skip_serializing_if = "ranks::PathMeta::is_empty")]
    pub meta: ranks::PathMeta,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BinaryReadResult {
    pub revision_id: String,
    pub path: String,
    pub entry: Entry,
    pub bytes: Vec<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_base64: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Snapshot {
    revision: Revision,
    files: BTreeMap<String, Entry>,
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Tenant {
    sequence: u64,
    head: String,
    revisions: BTreeMap<String, Snapshot>,
    proposals: BTreeMap<String, Proposal>,
    #[serde(default)]
    identity_paths: BTreeMap<String, BTreeSet<String>>,
    #[serde(default, skip_serializing)]
    pruned_revisions: BTreeSet<String>,
}
impl Tenant {
    fn missing_revision(&self, id: &str) -> ContentError {
        err(if self.pruned_revisions.contains(id) {
            "history_pruned"
        } else {
            "not_found"
        })
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Replay {
    fingerprint: String,
    result: serde_json::Value,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct State {
    version: u32,
    policy: Policy,
    tenants: BTreeMap<String, Tenant>,
    replays: BTreeMap<String, Replay>,
    initialized: bool,
}
impl Default for State {
    fn default() -> Self {
        Self {
            version: 4,
            policy: Policy::default(),
            tenants: BTreeMap::new(),
            replays: BTreeMap::new(),
            initialized: false,
        }
    }
}
/// One process may retain this handle; each operation reloads state under the same
/// cross-process lock. The directory is private trusted-host data, never a vault.
pub struct Service {
    root: PathBuf,
    clock: Clock,
    graph_cache: graph::GraphCache,
}
impl Service {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_clock(root, Box::new(system_clock))
    }
    /// As `open`, with the time source that stamps revisions, proposals and decisions.
    pub fn open_with_clock(root: impl AsRef<Path>, clock: Clock) -> Result<Self> {
        fs::create_dir_all(root.as_ref()).map_err(io)?;
        let root = fs::canonicalize(root).map_err(io)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).map_err(io)?;
        }
        Ok(Self {
            root,
            clock,
            graph_cache: graph::GraphCache::new(),
        })
    }
    fn transaction<T>(&self, write: bool, f: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
        storage::transaction(
            &self.root,
            write,
            storage::Scope {
                tenant: None,
                revisions: vec![],
                history: true,
                proposal: None,
                replay: None,
                hydrate_text: true,
                metadata: vec![],
            },
            f,
        )
    }
    fn transaction_for<T>(
        &self,
        who: &TrustedIdentity,
        write: bool,
        revisions: Vec<String>,
        history: bool,
        proposal: Option<&str>,
        key: Option<&str>,
        f: impl FnOnce(&mut State) -> Result<T>,
    ) -> Result<T> {
        let none = Chain::default();
        self.transaction_scoped(who, write, revisions, history, proposal, key, &none, f)
    }
    /// `transaction_for` without history, also loading `chain`'s revision records as
    /// metadata for `touched_since`.
    #[allow(clippy::too_many_arguments)]
    fn transaction_chain<T>(
        &self,
        who: &TrustedIdentity,
        write: bool,
        revisions: Vec<String>,
        proposal: Option<&str>,
        key: Option<&str>,
        chain: &Chain,
        f: impl FnOnce(&mut State) -> Result<T>,
    ) -> Result<T> {
        self.transaction_scoped(who, write, revisions, false, proposal, key, chain, f)
    }
    #[allow(clippy::too_many_arguments)]
    fn transaction_scoped<T>(
        &self,
        who: &TrustedIdentity,
        write: bool,
        revisions: Vec<String>,
        history: bool,
        proposal: Option<&str>,
        key: Option<&str>,
        chain: &Chain,
        f: impl FnOnce(&mut State) -> Result<T>,
    ) -> Result<T> {
        let replay = key.map(|key| replay_key(who, key)).transpose()?;
        storage::transaction(
            &self.root,
            write,
            storage::Scope {
                tenant: Some(&who.tenant_id),
                revisions: revisions.iter().map(String::as_str).collect(),
                history,
                proposal: Some(proposal.unwrap_or("")),
                replay: replay.as_deref(),
                hydrate_text: true,
                metadata: chain.loaded.iter().map(String::as_str).collect(),
            },
            f,
        )
    }
    /// Runs `f` (a transaction over `chain`) until the staleness walk has every record
    /// it needs: a walk that meets an unloaded revision asks for the metadata chain
    /// from head back to the base it walks to, read without texts, and runs again.
    fn with_chain<T>(
        &self,
        who: &TrustedIdentity,
        mut f: impl FnMut(&Chain) -> Result<T>,
    ) -> Result<T> {
        let mut chain = Chain::default();
        for _ in 0..8 {
            match f(&chain) {
                Err(e) if e.code == CHAIN_NEEDED => {
                    let base = chain.base.take().unwrap_or_default();
                    chain = Chain {
                        loaded: self.chain_to(who, &base)?,
                        base: Default::default(),
                    };
                }
                result => return result,
            }
        }
        Err(err("conflict"))
    }
    /// Revision ids from head back to (not including) `base`, as metadata; the first
    /// missing record ends the walk and is named too, so a second walk knows it is gone.
    fn chain_to(&self, who: &TrustedIdentity, base: &str) -> Result<HashSet<String>> {
        self.transaction_metadata(who, vec![], |s| {
            let mut ids = HashSet::new();
            let Some(t) = s.tenants.get(&who.tenant_id) else {
                return Ok(ids);
            };
            let mut at = Some(t.head.clone()).filter(|h| !h.is_empty());
            while let Some(id) = at.filter(|id| id != base) {
                at = t.revisions.get(&id).and_then(|r| r.revision.parent.clone());
                ids.insert(id);
            }
            Ok(ids)
        })
    }
    /// A read over every revision record as metadata (no texts, proposals unhydrated),
    /// with manifests reconstructed only for `revisions` and the head.
    fn transaction_metadata<T>(
        &self,
        who: &TrustedIdentity,
        revisions: Vec<String>,
        f: impl FnOnce(&mut State) -> Result<T>,
    ) -> Result<T> {
        storage::transaction(
            &self.root,
            false,
            storage::Scope {
                tenant: Some(&who.tenant_id),
                revisions: revisions.iter().map(String::as_str).collect(),
                history: true,
                proposal: None,
                replay: None,
                hydrate_text: false,
                metadata: vec![],
            },
            f,
        )
    }
    /// Trusted operator initialization only; never exposed as a JSON operation.
    pub fn bootstrap_policy(&self, policy: Policy) -> Result<()> {
        validate_policy(&policy)?;
        self.transaction(true, |s| {
            if s.initialized {
                return Err(err("already_initialized"));
            }
            s.policy = policy;
            s.initialized = true;
            Ok(())
        })
    }
    /// Current tenant policy is visible only to a root human administrator.
    pub fn policy(&self, who: &TrustedIdentity) -> Result<Policy> {
        self.transaction(false, |s| {
            human(who)?;
            ranks::check(s, who, Permission::Admin, "")?;
            Ok(caller_policy(&s.policy, &who.tenant_id))
        })
    }
    pub fn policy_state(&self, who: &TrustedIdentity) -> Result<PolicyState> {
        let policy = self.policy(who)?;
        Ok(PolicyState {
            digest: fingerprint(&policy)?,
            policy,
        })
    }
    pub fn set_policy(&self, who: &TrustedIdentity, policy: Policy) -> Result<()> {
        self.replace_policy(who, policy, None)
    }
    pub fn replace_policy(
        &self,
        who: &TrustedIdentity,
        policy: Policy,
        expected_digest: Option<&str>,
    ) -> Result<()> {
        self.transaction(true, |s| {
            human(who)?;
            ranks::check(s, who, Permission::Admin, "")?;
            if let Some(expected) = expected_digest {
                let current = caller_policy(&s.policy, &who.tenant_id);
                if fingerprint(&current)? != expected {
                    return Err(err("policy_conflict"));
                }
            }
            validate_policy(&policy)?;
            if policy.grants.iter().any(|g| g.tenant_id != who.tenant_id)
                || policy.tenants.keys().any(|id| id != &who.tenant_id)
            {
                return Err(err("forbidden"));
            }
            let section = policy.tenants.into_iter().next().map(|(_, t)| t);
            // A ladder may be reordered but must keep every role an entry is pinned to.
            let roles = section
                .as_ref()
                .map_or_else(|| auth::TenantPolicy::default().roles, |t| t.roles.clone());
            if ranks::head_files(s, &who.tenant_id)
                .values()
                .filter_map(|e| e.meta.seen_from.as_ref())
                .any(|r| !roles.contains(r))
            {
                return Err(err("invalid_policy"));
            }
            match section {
                Some(t) if t != auth::TenantPolicy::default() => {
                    s.policy.tenants.insert(who.tenant_id.clone(), t);
                }
                _ => {
                    s.policy.tenants.remove(&who.tenant_id);
                }
            }
            s.policy.grants.retain(|g| g.tenant_id != who.tenant_id);
            s.policy.grants.extend(policy.grants);
            reopen_approvals(s, &who.tenant_id);
            Ok(())
        })
    }
    pub fn head(&self, who: &TrustedIdentity) -> Result<String> {
        self.transaction_for(who, false, vec![], false, None, None, |s| {
            ranks::check(s, who, Permission::Read, "")?;
            Ok(s.tenants
                .get(&who.tenant_id)
                .map(|t| t.head.clone())
                .filter(|h| !h.is_empty())
                .unwrap_or_else(|| "r0".into()))
        })
    }
    pub fn read(
        &self,
        who: &TrustedIdentity,
        path: &str,
        revision: Option<&str>,
    ) -> Result<ReadResult> {
        valid_path(path)?;
        self.transaction_for(
            who,
            false,
            revision.into_iter().map(str::to_owned).collect(),
            false,
            None,
            None,
            |s| {
                ranks::check(s, who, Permission::Read, path)?;
                let t = s
                    .tenants
                    .get(&who.tenant_id)
                    .ok_or_else(|| err("not_found"))?;
                let id = revision.unwrap_or(&t.head);
                let snap = t.revisions.get(id).ok_or_else(|| t.missing_revision(id))?;
                Ok(ReadResult {
                    revision_id: id.into(),
                    path: path.into(),
                    content: {
                        let entry = snap.files.get(path).ok_or_else(|| err("not_found"))?;
                        authorize_identity(s, who, entry, path)?;
                        ranks::check_at(s, who, Permission::Read, path, &snap.files)?;
                        if entry.kind != EntryKind::Text {
                            return Err(err("not_text"));
                        }
                        String::from_utf8(storage::read_blob(
                            &self.root,
                            entry.blob.as_deref().ok_or_else(|| err("not_found"))?,
                        )?)
                        .map_err(io)?
                    },
                })
            },
        )
    }
    pub fn list(&self, who: &TrustedIdentity, scope: &str) -> Result<Vec<String>> {
        self.transaction_for(who, false, vec![], false, None, None, |s| {
            ranks::check(s, who, Permission::Read, scope)?;
            let Some(t) = s.tenants.get(&who.tenant_id) else {
                return Ok(vec![]);
            };
            let Some(snap) = t.revisions.get(&t.head) else {
                return Ok(vec![]);
            };
            Ok(snap
                .files
                .keys()
                .filter(|p| {
                    (scope.is_empty() || *p == scope || p.starts_with(&format!("{scope}/")))
                        && ranks::check(s, who, Permission::Read, p).is_ok()
                        && snap
                            .files
                            .get(*p)
                            .is_some_and(|e| authorize_identity(s, who, e, p).is_ok())
                })
                .cloned()
                .collect())
        })
    }
    pub fn propose(
        &self,
        who: &TrustedIdentity,
        base: &str,
        changes: Vec<Change>,
        key: &str,
        reason: Option<&str>,
        origin: Option<Origin>,
    ) -> Result<Proposal> {
        validate_changes(&changes)?;
        validate_message(reason)?;
        validate_origin(origin.as_ref())?;
        if changes.iter().any(|c| matches!(c, Change::SetMeta { .. })) {
            return Err(err("set_meta_requires_edit"));
        }
        // The base's manifest is loaded: the proposal's diff is taken against `base`.
        let mut revisions = restore_revisions(&changes);
        revisions.push(base.to_owned());
        self.with_chain(who, |chain| {
            self.transaction_chain(who, true, revisions.clone(), None, Some(key), chain, |s| {
                check_changes(s, who, Permission::Propose, &changes)?;
                check_changes(s, who, Permission::Read, &changes)?;
                let fp = propose_fingerprint(base, &changes, reason, origin.as_ref())?;
                if let Some(old) = replay(s, who, key, &fp)? {
                    return Ok(old);
                };
                // Per-path rule: an old base is accepted (and recorded as given) when no
                // path the proposal touches changed since it.
                let touched = Touched::of_changes(&changes).with_head_ids(s, &who.tenant_id);
                check_fresh(s, who, base, &touched, chain)?;
                let p = self.create_proposal(
                    s,
                    who,
                    base,
                    changes.clone(),
                    reason,
                    origin.clone(),
                    None,
                )?;
                save_replay(s, who, key, fp, &p)?;
                Ok(p)
            })
        })
    }
    /// A new pending proposal in `proposer`'s name on `base`, stored in `s`.
    #[allow(clippy::too_many_arguments)]
    fn create_proposal(
        &self,
        s: &mut State,
        proposer: &TrustedIdentity,
        base: &str,
        changes: Vec<Change>,
        reason: Option<&str>,
        origin: Option<Origin>,
        supersedes: Option<String>,
    ) -> Result<Proposal> {
        let tenant = s.tenants.entry(proposer.tenant_id.clone()).or_default();
        tenant.sequence += 1;
        let id = format!(
            "p{}",
            fingerprint(&(proposer, base, &changes, tenant.sequence))?
        );
        let digest = fingerprint(&(&proposer.tenant_id, proposer, base, &changes))?;
        let proposed_diff = preview(&self.root, s, proposer, base, &changes)?.1;
        check_diffs(s, proposer, Permission::Propose, &proposed_diff)?;
        let p = Proposal {
            id: id.clone(),
            tenant_id: proposer.tenant_id.clone(),
            proposer: proposer.clone(),
            base_revision: base.into(),
            diff: proposed_diff,
            changes,
            digest,
            state: ProposalState::Pending,
            approver: None,
            decisions: vec![],
            revision_id: None,
            at: Some((self.clock)()),
            reason: reason.map(String::from),
            origin,
            supersedes,
        };
        s.tenants
            .entry(proposer.tenant_id.clone())
            .or_default()
            .proposals
            .insert(id, p.clone());
        Ok(p)
    }
    /// Carries a stale proposal onto today's head keeping what fits: a new proposal in
    /// the original proposer's name with every fitting paragraph, and the old one
    /// superseded, in one transaction under one replay record. Only `put` changes to text
    /// files take part. The caller is the proposer or someone who may decide it.
    pub fn rebase_proposal(
        &self,
        who: &TrustedIdentity,
        id: &str,
        digest: &str,
        keep: &str,
        key: &str,
    ) -> Result<Proposal> {
        if keep != "fitting" {
            return Err(ContentError::new(
                "invalid_request",
                "keep must be \"fitting\"",
            ));
        }
        self.with_chain(who, |chain| {
            self.transaction_chain(who, true, vec![], Some(id), Some(key), chain, |s| {
                let mut old = reviewable(s, who, id)?;
                let proposer = who.actor_id == old.proposer.actor_id
                    || Some(&who.actor_id) == old.proposer.on_behalf_of.as_ref();
                if !proposer && !decides(s, who, &old) {
                    return Err(err("forbidden"));
                }
                exact(&old, digest)?;
                let fp = fingerprint(&("rebase", id, digest, keep))?;
                if let Some(done) = replay(s, who, key, &fp)? {
                    return Ok(done);
                };
                if !matches!(old.state, ProposalState::Pending | ProposalState::Approved) {
                    return Err(err("invalid_review_state"));
                }
                // Each change with its diff, when it is a `put` on a text file.
                let puts: Vec<(&str, &FileDiff)> = old
                    .changes
                    .iter()
                    .map(|c| match c {
                        Change::Put { path, content } => old
                            .diff
                            .iter()
                            .find(|d| d.path == *path)
                            .filter(|d| {
                                d.previous_path.is_none()
                                    && !d.deleted
                                    && matches!(d.kind, Some(EntryKind::Text) | None)
                                    && (d.before.is_some() || d.before_blob.is_none())
                            })
                            .map(|d| (content.as_str(), d)),
                        _ => None,
                    })
                    .collect::<Option<_>>()
                    .ok_or_else(|| err("rebase_unsupported"))?;
                let head = head_of(s, &who.tenant_id).to_owned();
                // Stale only when a path it touches changed since its base.
                if !touched_since(
                    s,
                    &who.tenant_id,
                    &old.base_revision,
                    &Touched::of_proposal(&old),
                    chain,
                )? {
                    return Err(err("not_stale"));
                }
                let files = ranks::head_files(s, &who.tenant_id).into_owned();
                let mut changes = vec![];
                for (proposed, d) in puts {
                    if let Some((path, content)) = self.rebase_file(&files, d, proposed)? {
                        changes.push(Change::Put { path, content });
                    }
                }
                if changes.is_empty() {
                    return Err(err("nothing_fits"));
                }
                validate_changes(&changes)?;
                // The answer carries head's text at the paths the files have now.
                check_changes(s, who, Permission::Read, &changes).map_err(|_| err("not_found"))?;
                // A proposer who has lost Read is `forbidden` to the decider, who can read the path.
                check_changes(s, &old.proposer, Permission::Read, &changes)
                    .map_err(|_| err("forbidden"))?;
                // `create_proposal` checks the proposer as `propose` checks its caller
                // (Propose and Read on every path): a demoted proposer is not re-proposed for.
                let new = self.create_proposal(
                    s,
                    &old.proposer,
                    &head,
                    changes,
                    old.reason.as_deref(),
                    old.origin.clone(),
                    Some(old.id.clone()),
                )?;
                old.state = ProposalState::Superseded;
                old.approver = None;
                old.decisions.push(ReviewDecision {
                    actor: who.clone(),
                    approved: false,
                    policy_digest: fingerprint(&s.policy)?,
                    at: Some((self.clock)()),
                    reason: Some(format!("Superseded by {}", new.id)),
                });
                s.tenants
                    .get_mut(&who.tenant_id)
                    .unwrap()
                    .proposals
                    .insert(id.into(), old);
                save_replay(s, who, key, fp, &new)?;
                Ok(new)
            })
        })
    }
    pub fn review(&self, who: &TrustedIdentity, id: &str) -> Result<Proposal> {
        self.transaction_for(who, false, vec![], false, Some(id), None, |s| {
            reviewable(s, who, id)
        })
    }
    pub fn approve(
        &self,
        who: &TrustedIdentity,
        id: &str,
        digest: &str,
        key: &str,
        reason: Option<&str>,
    ) -> Result<Proposal> {
        self.decide(who, id, digest, key, true, reason)
    }
    pub fn reject(
        &self,
        who: &TrustedIdentity,
        id: &str,
        digest: &str,
        key: &str,
        reason: Option<&str>,
    ) -> Result<Proposal> {
        self.decide(who, id, digest, key, false, reason)
    }
    fn decide(
        &self,
        who: &TrustedIdentity,
        id: &str,
        digest: &str,
        key: &str,
        approve: bool,
        reason: Option<&str>,
    ) -> Result<Proposal> {
        validate_message(reason)?;
        self.with_chain(who, |chain| {
            self.transaction_chain(who, true, vec![], Some(id), Some(key), chain, |s| {
                human(who)?;
                read_tenant(s, who)?;
                let mut p = proposal(s, who, id)?;
                check_changes(s, who, Permission::Read, &p.changes)
                    .map_err(|_| err("not_found"))?;
                for c in &p.diff {
                    authorize_diff(s, who, c)?;
                }
                check_changes(s, who, Permission::Approve, &p.changes)?;
                check_diffs(s, who, Permission::Approve, &p.diff)?;
                if who.actor_id == p.proposer.actor_id
                    || Some(&who.actor_id) == p.proposer.on_behalf_of.as_ref()
                {
                    return Err(err("self_approval_denied"));
                }
                exact(&p, digest)?;
                let fp = decision_fingerprint(id, digest, approve, reason)?;
                if let Some(old) = replay(s, who, key, &fp)? {
                    return Ok(old);
                };
                if p.state != ProposalState::Pending {
                    return Err(err("invalid_review_state"));
                }
                // Only approve (and apply) write toward content, so only they need
                // the touched paths unchanged since the base; a decline never does.
                if approve {
                    check_fresh(s, who, &p.base_revision, &Touched::of_proposal(&p), chain)?;
                }
                p.state = if approve {
                    ProposalState::Approved
                } else {
                    ProposalState::Rejected
                };
                p.approver = Some(who.clone());
                p.decisions.push(ReviewDecision {
                    actor: who.clone(),
                    approved: approve,
                    policy_digest: fingerprint(&s.policy)?,
                    at: Some((self.clock)()),
                    reason: reason.map(String::from),
                });
                s.tenants
                    .get_mut(&who.tenant_id)
                    .unwrap()
                    .proposals
                    .insert(id.into(), p.clone());
                save_replay(s, who, key, fp, &p)?;
                Ok(p)
            })
        })
    }
    /// The proposer's own decline (or its `on_behalf_of` human's), before it is applied.
    pub fn withdraw(
        &self,
        who: &TrustedIdentity,
        id: &str,
        digest: &str,
        key: &str,
    ) -> Result<Proposal> {
        self.transaction_for(who, true, vec![], false, Some(id), Some(key), |s| {
            read_tenant(s, who)?;
            let mut p = proposal(s, who, id)?;
            check_changes(s, who, Permission::Read, &p.changes).map_err(|_| err("not_found"))?;
            for c in &p.diff {
                authorize_diff(s, who, c)?;
            }
            if who.actor_id != p.proposer.actor_id
                && Some(&who.actor_id) != p.proposer.on_behalf_of.as_ref()
            {
                return Err(err("not_proposer"));
            }
            exact(&p, digest)?;
            let fp = fingerprint(&("withdraw", id, digest))?;
            if let Some(old) = replay(s, who, key, &fp)? {
                return Ok(old);
            };
            if !matches!(p.state, ProposalState::Pending | ProposalState::Approved) {
                return Err(err("invalid_review_state"));
            }
            p.state = ProposalState::Withdrawn;
            p.approver = None;
            p.decisions.push(ReviewDecision {
                actor: who.clone(),
                approved: false,
                policy_digest: fingerprint(&s.policy)?,
                at: Some((self.clock)()),
                reason: Some("Withdrawn".into()),
            });
            s.tenants
                .get_mut(&who.tenant_id)
                .unwrap()
                .proposals
                .insert(id.into(), p.clone());
            save_replay(s, who, key, fp, &p)?;
            Ok(p)
        })
    }
    pub fn apply(
        &self,
        who: &TrustedIdentity,
        id: &str,
        digest: &str,
        key: &str,
    ) -> Result<Revision> {
        self.with_chain(who, |chain| {
            self.transaction_chain(who, true, vec![], Some(id), Some(key), chain, |s| {
                read_tenant(s, who)?;
                let mut p = proposal(s, who, id)?;
                check_changes(s, who, Permission::Read, &p.changes)
                    .map_err(|_| err("not_found"))?;
                for c in &p.diff {
                    authorize_diff(s, who, c)?;
                }
                check_changes(s, who, Permission::Propose, &p.changes)?;
                exact(&p, digest)?;
                let fp = fingerprint(&("apply", id, digest))?;
                if let Some(old) = replay(s, who, key, &fp)? {
                    return Ok(old);
                };
                if p.state == ProposalState::Applied {
                    let revision = s.tenants[&who.tenant_id].revisions
                        [p.revision_id.as_ref().unwrap()]
                    .revision
                    .clone();
                    save_replay(s, who, key, fp, &revision)?;
                    return Ok(revision);
                };
                if matches!(
                    p.state,
                    ProposalState::Withdrawn | ProposalState::Superseded
                ) {
                    return Err(err("invalid_review_state"));
                }
                if p.state != ProposalState::Approved {
                    return Err(err("approval_required"));
                }
                let approver = p
                    .approver
                    .as_ref()
                    .ok_or_else(|| err("approval_required"))?;
                human(approver)?;
                check_changes(s, approver, Permission::Approve, &p.changes)?;
                check_diffs(s, approver, Permission::Approve, &p.diff)?;
                check_diffs(s, &p.proposer, Permission::Propose, &p.diff)?;
                check_diffs(s, who, Permission::Propose, &p.diff)?;
                check_changes(s, approver, Permission::Read, &p.changes)?;
                check_changes(s, &p.proposer, Permission::Propose, &p.changes)?;
                check_changes(s, &p.proposer, Permission::Read, &p.changes)?;
                // Lands on today's head when no touched path changed since the base.
                let head = check_fresh(s, who, &p.base_revision, &Touched::of_proposal(&p), chain)?;
                let r = publish(
                    &self.root,
                    s,
                    who,
                    &head,
                    &p.changes,
                    Some(id.into()),
                    Stamp {
                        at: (self.clock)(),
                        message: None,
                        origin: p.origin.clone(),
                    },
                    None,
                )?;
                p.state = ProposalState::Applied;
                p.revision_id = Some(r.id.clone());
                s.tenants
                    .get_mut(&who.tenant_id)
                    .unwrap()
                    .proposals
                    .insert(id.into(), p);
                save_replay(s, who, key, fp, &r)?;
                Ok(r)
            })
        })
    }
    pub fn edit(
        &self,
        who: &TrustedIdentity,
        base: &str,
        changes: Vec<Change>,
        key: &str,
        message: Option<&str>,
        origin: Option<Origin>,
    ) -> Result<Revision> {
        self.edit_as(who, base, changes, key, message, origin, None)
    }
    /// `edit`, with the revision kind the caller already knows (an import).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn edit_as(
        &self,
        who: &TrustedIdentity,
        base: &str,
        changes: Vec<Change>,
        key: &str,
        message: Option<&str>,
        origin: Option<Origin>,
        kind_hint: Option<RevisionKind>,
    ) -> Result<Revision> {
        validate_changes(&changes)?;
        validate_message(message)?;
        validate_origin(origin.as_ref())?;
        self.with_chain(who, |chain| {
            let origin = origin.clone();
            self.transaction_chain(
                who,
                true,
                restore_revisions(&changes),
                None,
                Some(key),
                chain,
                |s| {
                    human(who)?;
                    check_changes(s, who, Permission::Edit, &changes)?;
                    check_changes(s, who, Permission::Read, &changes)?;
                    let fp = edit_fingerprint(base, &changes, message, origin.as_ref())?;
                    if let Some(old) = replay(s, who, key, &fp)? {
                        return Ok(old);
                    };
                    // Lands on today's head when no path it touches changed since `base`.
                    let touched = Touched::of_changes(&changes).with_head_ids(s, &who.tenant_id);
                    let base = check_fresh(s, who, base, &touched, chain)?;
                    let base = base.as_str();
                    let mut diffs = preview(&self.root, s, who, base, &changes)?.1;
                    // `preview` gates set_meta itself: the visibility rung replaces Edit.
                    diffs.retain(|d| {
                        d.previous_path.is_some()
                            || !changes.iter().any(
                                |c| matches!(c, Change::SetMeta { path, .. } if *path == d.path),
                            )
                    });
                    check_diffs(s, who, Permission::Edit, &diffs)?;
                    let stamp = Stamp {
                        at: (self.clock)(),
                        message: message.map(String::from),
                        origin,
                    };
                    let r = publish(&self.root, s, who, base, &changes, None, stamp, kind_hint)?;
                    save_replay(s, who, key, fp, &r)?;
                    Ok(r)
                },
            )
        })
    }
    pub fn history(&self, who: &TrustedIdentity, path: &str) -> Result<Vec<Revision>> {
        valid_path(path)?;
        self.history_by(who, path, None, false)
    }
    /// `history`, for the file identity `file_id` when known (a `stat` at a past revision).
    fn history_by(
        &self,
        who: &TrustedIdentity,
        path: &str,
        file_id: Option<&str>,
        metadata_only: bool,
    ) -> Result<Vec<Revision>> {
        // Revision-time visibility needs the manifests of the revisions that touch the
        // path and of their parents. A first pass over revision metadata names them; a
        // pass that finds a revision it did not hydrate (the head moved) asks again.
        let mut hydrated = HashSet::new();
        for _ in 0..8 {
            match self.history_over(who, path, file_id, metadata_only, &hydrated)? {
                Ok(result) => return Ok(result),
                Err(needed) => hydrated = needed,
            }
        }
        Err(err("conflict"))
    }
    /// The history, or the revisions whose manifests it needs when one is not hydrated.
    fn history_over(
        &self,
        who: &TrustedIdentity,
        path: &str,
        file_id: Option<&str>,
        metadata_only: bool,
        hydrated: &HashSet<String>,
    ) -> Result<std::result::Result<Vec<Revision>, HashSet<String>>> {
        let revisions = hydrated.iter().cloned().collect();
        let body = |s: &mut State| {
            let head = ranks::head_files(s, &who.tenant_id);
            if ranks::check(s, who, Permission::Read, path).is_err() {
                // As if absent: empty where the nearest existing ancestor is readable.
                ranks::check_at(
                    s,
                    who,
                    Permission::Read,
                    path,
                    &ranks::as_absent(&head, path),
                )?;
                return Ok(Ok(vec![]));
            }
            let Some(t) = s.tenants.get(&who.tenant_id) else {
                return Ok(Ok(vec![]));
            };
            let mut identity = file_id
                .map(str::to_owned)
                .or_else(|| head.get(path).map(|e| e.file_id.clone()));
            let mut at = Some(t.head.as_str());
            while identity.is_none() {
                let Some(snap) = at.and_then(|id| t.revisions.get(id)) else {
                    break;
                };
                identity = snap
                    .revision
                    .changes
                    .iter()
                    .rev()
                    .find(|c| c.path == path || c.previous_path.as_deref() == Some(path))
                    .and_then(|c| c.file_id.clone());
                at = snap.revision.parent.as_deref();
            }
            let touches = |c: &FileDiff| {
                identity
                    .as_ref()
                    .is_some_and(|fid| c.file_id.as_ref() == Some(fid))
                    || (identity.is_none() && c.path == path)
            };
            let mut needed = HashSet::new();
            for id in chain(t, &t.head) {
                let snapshot = &t.revisions[&id];
                if snapshot.revision.changes.iter().any(touches) {
                    needed.extend(snapshot.revision.parent.clone());
                    needed.insert(id);
                }
            }
            if !needed.is_subset(hydrated) {
                return Ok(Err(needed));
            }
            let mut result = vec![];
            let mut current = Some(t.head.as_str());
            while let Some(id) = current {
                let Some(snapshot) = t.revisions.get(id) else {
                    break;
                };
                let mut revision = snapshot.revision.clone();
                revision.changes.retain(|c| touches(c));
                for c in &revision.changes {
                    authorize_diff(s, who, c)?;
                }
                if !revision.changes.is_empty()
                    && readable_then(s, who, t, &snapshot.revision, &revision.changes).is_ok()
                {
                    result.push(revision);
                }
                current = snapshot.revision.parent.as_deref();
            }
            Ok(Ok(result))
        };
        if metadata_only {
            self.transaction_metadata(who, revisions, body)
        } else {
            self.transaction_for(who, false, revisions, true, None, None, body)
        }
    }
    pub fn diff(&self, who: &TrustedIdentity, id: &str) -> Result<Vec<FileDiff>> {
        // The parent manifest holds the `before` side; revisions never change parents.
        let parent =
            self.transaction_for(who, false, vec![id.to_owned()], false, None, None, |s| {
                Ok(s.tenants
                    .get(&who.tenant_id)
                    .and_then(|t| t.revisions.get(id))
                    .and_then(|snap| snap.revision.parent.clone()))
            })?;
        let revisions = std::iter::once(id.to_owned()).chain(parent).collect();
        self.transaction_for(who, false, revisions, false, None, None, |s| {
            read_tenant(s, who)?;
            let t = s
                .tenants
                .get(&who.tenant_id)
                .ok_or_else(|| err("not_found"))?;
            let r = &t
                .revisions
                .get(id)
                .ok_or_else(|| t.missing_revision(id))?
                .revision;
            for c in &r.changes {
                authorize_diff(s, who, c)?;
            }
            readable_then(s, who, t, r, &r.changes)?;
            Ok(r.changes.clone())
        })
    }
}
fn human(who: &TrustedIdentity) -> Result<()> {
    if who.kind != ActorKind::Human {
        Err(err("human_required"))
    } else {
        Ok(())
    }
}
fn valid_path(path: &str) -> Result<()> {
    if path.is_empty()
        || path.len() > 1024
        || path.starts_with('/')
        || path.contains('\\')
        || path.chars().any(|c| c.is_control())
        || path
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == "..")
    {
        Err(err("invalid_path"))
    } else {
        Ok(())
    }
}
fn validate_changes(changes: &[Change]) -> Result<()> {
    if changes.is_empty() || changes.len() > 100 {
        return Err(err("changes_limit"));
    }
    let mut paths = BTreeSet::new();
    for c in changes {
        valid_path(c.path())?;
        if !paths.insert(c.path()) {
            return Err(err("duplicate_path"));
        }
        if let Change::Rename { to, .. } = c {
            valid_path(to)?;
        }
        let size = match c {
            Change::Put { content, .. } => content.len(),
            Change::PutBinary { bytes, .. } => bytes.len(),
            _ => 0,
        };
        if size > 16 * 1024 * 1024 {
            return Err(err("content_limit"));
        }
    }
    Ok(())
}
fn check_changes(
    s: &State,
    who: &TrustedIdentity,
    permission: Permission,
    changes: &[Change],
) -> Result<()> {
    for c in changes {
        // `preview` gates set_meta: visibility rung for seen_from, Edit for colour.
        if permission == Permission::Edit && matches!(c, Change::SetMeta { .. }) {
            continue;
        }
        ranks::check(s, who, permission, c.path())?;
        if let Change::Rename { to, .. } = c {
            ranks::check(s, who, permission, to)?;
        }
    }
    Ok(())
}
/// `review`'s checks: the caller reads the tenant and every path the proposal touches.
fn reviewable(s: &State, who: &TrustedIdentity, id: &str) -> Result<Proposal> {
    read_tenant(s, who)?;
    let p = proposal(s, who, id)?;
    check_changes(s, who, Permission::Read, &p.changes).map_err(|_| err("not_found"))?;
    for c in &p.diff {
        authorize_diff(s, who, c)?;
    }
    Ok(p)
}
/// Who may decide `p` (`deciders`), whatever its state, and whether only the
/// administrator fallback qualifies.
fn proposal_deciders(s: &State, p: &Proposal) -> (Vec<String>, bool) {
    let mut paths: Vec<&str> = Vec::new();
    for c in &p.changes {
        paths.push(c.path());
        if let Change::Rename { to, .. } = c {
            paths.push(to);
        }
    }
    for d in &p.diff {
        paths.push(&d.path);
        if let Some(prev) = &d.previous_path {
            paths.push(prev);
        }
    }
    let mut excluded = vec![p.proposer.actor_id.as_str()];
    excluded.extend(p.proposer.on_behalf_of.as_deref());
    ranks::deciders(s, &p.tenant_id, &paths, &excluded)
}
/// The caller is one of `p`'s deciders.
fn decides(s: &State, who: &TrustedIdentity, p: &Proposal) -> bool {
    who.kind == ActorKind::Human && proposal_deciders(s, p).0.contains(&who.actor_id)
}
/// The tenant's head revision; `r0` before its first.
fn head_of<'a>(s: &'a State, tenant_id: &str) -> &'a str {
    s.tenants
        .get(tenant_id)
        .map(|t| t.head.as_str())
        .filter(|h| !h.is_empty())
        .unwrap_or("r0")
}
/// Exact base rule (onboarding): `base` must be today's head.
fn check_base(s: &State, who: &TrustedIdentity, base: &str) -> Result<()> {
    if base != head_of(s, &who.tenant_id) {
        Err(err("stale_base"))
    } else {
        Ok(())
    }
}
/// Internal: the staleness walk met a revision record not loaded; never answered.
const CHAIN_NEEDED: &str = "chain_needed";
/// Revision records loaded as metadata for `touched_since`, and the base a walk that
/// ran short wanted.
#[derive(Default)]
struct Chain {
    loaded: HashSet<String>,
    base: std::cell::RefCell<Option<String>>,
}
/// The paths and file identities a write touches.
#[derive(Default)]
struct Touched {
    names: Vec<String>,
    ids: HashSet<String>,
}
impl Touched {
    /// A proposal's: every changed name (both sides of a rename) and identity.
    fn of_proposal(p: &Proposal) -> Self {
        let mut t = Self::of_changes(&p.changes);
        for d in &p.diff {
            t.names.push(d.path.clone());
            t.names.extend(d.previous_path.clone());
            t.ids.extend(d.file_id.clone());
        }
        t
    }
    /// A direct edit's: every changed name and the identities those names hold at head.
    fn of_changes(changes: &[Change]) -> Self {
        let mut t = Self::default();
        for c in changes {
            t.names.push(c.path().to_owned());
            if let Change::Rename { to, .. } = c {
                t.names.push(to.clone());
            }
        }
        t
    }
    fn with_head_ids(mut self, s: &State, tenant: &str) -> Self {
        let files = ranks::head_files(s, tenant);
        for n in &self.names {
            if let Some(e) = files.get(n) {
                self.ids.insert(e.file_id.clone());
            }
        }
        self
    }
    /// `d` changed the same file, the same name, a folder holding it or a file it holds.
    fn hit(&self, d: &FileDiff) -> bool {
        let related = |a: &str, b: &str| {
            a == b
                || (a.len() > b.len() && a.starts_with(b) && a.as_bytes()[b.len()] == b'/')
                || (b.len() > a.len() && b.starts_with(a) && b.as_bytes()[a.len()] == b'/')
        };
        d.file_id.as_ref().is_some_and(|f| self.ids.contains(f))
            || std::iter::once(&d.path)
                .chain(d.previous_path.as_ref())
                .any(|p| self.names.iter().any(|n| related(n, p)))
    }
}
/// Whether a revision after `base` changed what `touched` names (by identity, by name,
/// renames included). Also true when `base` is not an ancestor of head or the chain
/// between them is gone. A head that is `base` is never stale.
fn touched_since(
    s: &State,
    tenant: &str,
    base: &str,
    touched: &Touched,
    chain: &Chain,
) -> Result<bool> {
    let head = head_of(s, tenant);
    if base == head {
        return Ok(false);
    }
    let Some(t) = s.tenants.get(tenant) else {
        return Ok(true);
    };
    let mut at = head.to_owned();
    loop {
        if at == base {
            return Ok(false);
        }
        let Some(snap) = t.revisions.get(&at) else {
            if chain.loaded.contains(&at) || t.pruned_revisions.contains(&at) {
                return Ok(true);
            }
            *chain.base.borrow_mut() = Some(base.to_owned());
            return Err(err(CHAIN_NEEDED));
        };
        if snap.revision.changes.iter().any(|d| touched.hit(d)) {
            return Ok(true);
        }
        match &snap.revision.parent {
            Some(p) => at = p.clone(),
            None => return Ok(base != "r0"),
        }
    }
}
/// The per-path base rule (propose, approve, apply, edit): `stale_base` only when a path the
/// write touches changed after `base`; otherwise the write lands on today's head,
/// which is returned.
fn check_fresh(
    s: &State,
    who: &TrustedIdentity,
    base: &str,
    touched: &Touched,
    chain: &Chain,
) -> Result<String> {
    if touched_since(s, &who.tenant_id, base, touched, chain)? {
        Err(err("stale_base"))
    } else {
        Ok(head_of(s, &who.tenant_id).to_owned())
    }
}
fn proposal(s: &State, who: &TrustedIdentity, id: &str) -> Result<Proposal> {
    s.tenants
        .get(&who.tenant_id)
        .and_then(|t| t.proposals.get(id))
        .cloned()
        .ok_or_else(|| err("not_found"))
}
fn exact(p: &Proposal, digest: &str) -> Result<()> {
    if p.digest != digest {
        Err(err("proposal_mismatch"))
    } else {
        Ok(())
    }
}
fn fingerprint(value: &impl Serialize) -> Result<String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(value).map_err(io)?)
    ))
}
// Without the message and origin fields the older tuples are kept exactly, so retries that span
// the upgrade still replay instead of answering `idempotency_conflict`.
fn edit_fingerprint(
    base: &str,
    changes: &[Change],
    message: Option<&str>,
    origin: Option<&Origin>,
) -> Result<String> {
    if message.is_none() && origin.is_none() {
        fingerprint(&("edit", base, changes))
    } else {
        fingerprint(&("edit", base, changes, message, origin))
    }
}
fn propose_fingerprint(
    base: &str,
    changes: &[Change],
    reason: Option<&str>,
    origin: Option<&Origin>,
) -> Result<String> {
    if reason.is_none() && origin.is_none() {
        fingerprint(&("propose", base, changes))
    } else {
        fingerprint(&("propose", base, changes, reason, origin))
    }
}
fn decision_fingerprint(
    id: &str,
    digest: &str,
    approve: bool,
    reason: Option<&str>,
) -> Result<String> {
    match reason {
        None => fingerprint(&("decision", id, digest, approve)),
        Some(reason) => fingerprint(&("decision", id, digest, approve, reason)),
    }
}
fn replay_key(who: &TrustedIdentity, key: &str) -> Result<String> {
    if key.is_empty() || key.len() > 200 {
        return Err(err("invalid_idempotency_key"));
    }
    serde_json::to_string(&(who, key)).map_err(io)
}
fn replay<T: for<'de> Deserialize<'de>>(
    s: &State,
    who: &TrustedIdentity,
    key: &str,
    fp: &str,
) -> Result<Option<T>> {
    match s.replays.get(&replay_key(who, key)?) {
        None => Ok(None),
        Some(r) if r.fingerprint == fp => {
            if r.result
                .get("$mnemosyne_replay_expired")
                .and_then(serde_json::Value::as_bool)
                == Some(true)
            {
                return Err(err("replay_expired"));
            }
            authorize_replay(s, who, &r.result)?;
            Ok(Some(serde_json::from_value(r.result.clone()).map_err(io)?))
        }
        Some(_) => Err(err("idempotency_conflict")),
    }
}
fn save_replay(
    s: &mut State,
    who: &TrustedIdentity,
    key: &str,
    fp: String,
    result: &impl Serialize,
) -> Result<()> {
    s.replays.insert(
        replay_key(who, key)?,
        Replay {
            fingerprint: fp,
            result: serde_json::to_value(result).map_err(io)?,
        },
    );
    Ok(())
}
/// What the changes amount to: an import or proposal as the caller says, otherwise the
/// one kind every change shares, otherwise a plain edit.
fn classify(
    s: &State,
    who: &TrustedIdentity,
    base: &str,
    changes: &[Change],
    from_proposal: bool,
    hint: Option<RevisionKind>,
) -> RevisionKind {
    if let Some(kind) = hint {
        return kind;
    }
    if from_proposal {
        return RevisionKind::Proposal;
    }
    let all = |f: fn(&Change) -> bool| !changes.is_empty() && changes.iter().all(f);
    if all(|c| matches!(c, Change::SetMeta { .. })) {
        RevisionKind::Meta
    } else if all(|c| matches!(c, Change::Rename { .. })) {
        RevisionKind::Rename
    } else if all(|c| matches!(c, Change::Restore { .. })) {
        RevisionKind::Restore
    } else if all(|c| matches!(c, Change::PutBinary { .. })) && {
        let files = s
            .tenants
            .get(&who.tenant_id)
            .and_then(|t| t.revisions.get(base))
            .map(|snap| &snap.files);
        changes.iter().all(|c| {
            files
                .and_then(|f| f.get(c.path()))
                .is_some_and(|e| e.kind == EntryKind::Binary)
        })
    } {
        RevisionKind::NewVersion
    } else {
        RevisionKind::Edit
    }
}
/// When, why and through whom a revision is published.
struct Stamp {
    at: String,
    message: Option<String>,
    origin: Option<Origin>,
}
#[allow(clippy::too_many_arguments)]
fn publish(
    root: &Path,
    s: &mut State,
    who: &TrustedIdentity,
    base: &str,
    changes: &[Change],
    proposal_id: Option<String>,
    stamp: Stamp,
    kind_hint: Option<RevisionKind>,
) -> Result<Revision> {
    let (files, diffs) = preview(root, s, who, base, changes)?;
    let kind = classify(s, who, base, changes, proposal_id.is_some(), kind_hint);
    let t = s.tenants.entry(who.tenant_id.clone()).or_default();
    t.sequence += 1;
    let id = format!("r{}", fingerprint(&(who, base, changes, t.sequence))?);
    let revision = Revision {
        id: id.clone(),
        parent: (base != "r0").then(|| base.into()),
        actor: who.clone(),
        proposal_id,
        changes: diffs,
        at: Some(stamp.at),
        message: stamp.message,
        origin: stamp.origin,
        kind: Some(kind),
    };
    record_identity_paths(&mut t.identity_paths, &revision.changes);
    t.revisions.insert(
        id.clone(),
        Snapshot {
            revision: revision.clone(),
            files,
        },
    );
    t.head = id;
    Ok(revision)
}

/// Ok when the caller holds a Read grant somewhere in its tenant or is a member of it.
fn read_tenant(s: &State, who: &TrustedIdentity) -> Result<()> {
    let tp = ranks::tenant_policy(s, &who.tenant_id);
    if let Some(m) = tp.member_role(&who.actor_id) {
        if m.kind != who.kind {
            return Err(err("policy_mismatch"));
        }
        return Ok(());
    }
    if s.policy.grants.iter().any(|g| {
        g.tenant_id == who.tenant_id
            && g.permission == Permission::Read
            && ranks::check(s, who, Permission::Read, &g.scope).is_ok()
    }) || ranks::check(s, who, Permission::Read, "").is_ok()
    {
        Ok(())
    } else {
        Err(err("forbidden"))
    }
}

// All names an identity has occupied remain authorization boundaries. A move or
// deletion cannot make its earlier bytes visible to a narrower current grant.
fn authorize_identity(s: &State, who: &TrustedIdentity, entry: &Entry, path: &str) -> Result<()> {
    ranks::check(s, who, Permission::Read, path)?;
    if let Some(t) = s.tenants.get(&who.tenant_id) {
        if let Some(paths) = t.identity_paths.get(&entry.file_id) {
            for path in paths {
                ranks::check(s, who, Permission::Read, path).map_err(|_| err("not_found"))?;
            }
        }
    }
    Ok(())
}
fn authorize_diff(s: &State, who: &TrustedIdentity, c: &FileDiff) -> Result<()> {
    ranks::check(s, who, Permission::Read, &c.path).map_err(|_| err("not_found"))?;
    if let Some(path) = &c.previous_path {
        ranks::check(s, who, Permission::Read, path).map_err(|_| err("not_found"))?;
    }
    if let Some(id) = &c.file_id {
        authorize_identity(
            s,
            who,
            &Entry {
                file_id: id.clone(),
                kind: EntryKind::Text,
                blob: None,
                original_blob: None,
                meta: Default::default(),
            },
            &c.path,
        )
        .map_err(|_| err("not_found"))?;
    }
    if c.file_id.is_none() {
        authorize_legacy_path(s, who, &c.path)?;
    }
    Ok(())
}
/// Revision ids from `head` back to the oldest retained ancestor.
fn chain(t: &Tenant, head: &str) -> Vec<String> {
    let mut ids = vec![];
    let mut at = Some(head);
    while let Some(id) = at.filter(|id| !id.is_empty()) {
        let Some(snap) = t.revisions.get(id) else {
            break;
        };
        ids.push(id.to_owned());
        at = snap.revision.parent.as_deref();
    }
    ids
}
/// The past is as hidden as it was then: each changed name must be readable under the
/// revision's own manifest and, for the `before` side, its parent's.
fn readable_then(
    s: &State,
    who: &TrustedIdentity,
    t: &Tenant,
    revision: &Revision,
    changes: &[FileDiff],
) -> Result<()> {
    let manifests = std::iter::once(revision.id.as_str())
        .chain(revision.parent.as_deref())
        .filter_map(|id| t.revisions.get(id).map(|snap| &snap.files));
    for files in manifests {
        for c in changes {
            for name in std::iter::once(&c.path).chain(c.previous_path.as_ref()) {
                ranks::check_at(s, who, Permission::Read, name, files)
                    .map_err(|_| err("not_found"))?;
            }
        }
    }
    Ok(())
}
fn preview(
    root: &Path,
    s: &State,
    who: &TrustedIdentity,
    base: &str,
    changes: &[Change],
) -> Result<(BTreeMap<String, Entry>, Vec<FileDiff>)> {
    let tenant = s.tenants.get(&who.tenant_id);
    let mut files = tenant
        .and_then(|t| t.revisions.get(base))
        .map(|s| s.files.clone())
        .unwrap_or_default();
    let mut diffs = vec![];
    for change in changes {
        let path = change.path();
        let before = files.get(path).cloned();
        if let Some(entry) = &before {
            authorize_identity(s, who, entry, path)?;
        }
        match change {
            Change::Put { .. } | Change::PutBinary { .. } => {
                check_entry_destination(&files, path, &EntryKind::Text)?;
                let (bytes, kind) = match change {
                    Change::Put { content, .. } => (content.as_bytes(), EntryKind::Text),
                    Change::PutBinary { bytes, .. } => (bytes.as_slice(), EntryKind::Binary),
                    _ => unreachable!(),
                };
                let blob = storage::put_blob(root, bytes)?;
                let entry = Entry {
                    file_id: before
                        .as_ref()
                        .map(|e| e.file_id.clone())
                        .unwrap_or(format!("f{}", fingerprint(&(&who.tenant_id, base, path))?)),
                    kind,
                    original_blob: before
                        .as_ref()
                        .and_then(|e| e.original_blob.clone())
                        .or(Some(blob.clone())),
                    blob: Some(blob),
                    meta: before.as_ref().map(|e| e.meta.clone()).unwrap_or_default(),
                };
                diffs.push(make_diff(root, path, None, before.as_ref(), Some(&entry))?);
                files.insert(path.into(), entry);
            }
            Change::CreateFolder { .. } => {
                if before.is_some() {
                    return Err(err("already_exists"));
                }
                check_parents(&files, path)?;
                let entry = Entry {
                    file_id: format!("f{}", fingerprint(&(&who.tenant_id, base, path))?),
                    kind: EntryKind::Folder,
                    blob: None,
                    original_blob: None,
                    meta: Default::default(),
                };
                diffs.push(make_diff(root, path, None, None, Some(&entry))?);
                files.insert(path.into(), entry);
            }
            Change::Delete { .. } | Change::Rename { .. } => {
                if before.is_none() {
                    return Err(err("not_found"));
                }
                let entries: Vec<_> = files
                    .iter()
                    .filter(|(p, _)| *p == path || p.starts_with(&format!("{path}/")))
                    .map(|(p, e)| (p.clone(), e.clone()))
                    .collect();
                // A delete or move carries every descendant: one the caller cannot read
                // (rank- or lineage-hidden) refuses the whole change. `forbidden`, not
                // `not_found`: the caller holds the folder, so this bounded disclosure is
                // already earned, and "absent" would be false for a folder it can list.
                for (p, e) in &entries {
                    if p != path {
                        authorize_identity(s, who, e, p).map_err(|e| match e.code.as_str() {
                            "not_found" => err("forbidden"),
                            _ => e,
                        })?;
                    }
                }
                if let Change::Rename { to, .. } = change {
                    if to == path || to.starts_with(&format!("{path}/")) {
                        return Err(err("path_conflict"));
                    }
                    check_parents(&files, to)?;
                    let mut remaining = files.clone();
                    for (old, _) in &entries {
                        remaining.remove(old);
                    }
                    for (old, entry) in &entries {
                        let new = format!("{to}{}", &old[path.len()..]);
                        if files.contains_key(&new) {
                            return Err(err("already_exists"));
                        }
                        check_entry_destination(&remaining, &new, &entry.kind)?;
                    }
                }
                for (old, entry) in entries {
                    authorize_identity(s, who, &entry, &old)?;

                    files.remove(&old);
                    if let Change::Rename { to, .. } = change {
                        let new = format!("{to}{}", &old[path.len()..]);
                        ranks::check(s, who, Permission::Read, &new)?;
                        diffs.push(make_diff(
                            root,
                            &new,
                            Some(old),
                            Some(&entry),
                            Some(&entry),
                        )?);
                        files.insert(new, entry);
                    } else {
                        diffs.push(make_diff(root, &old, None, Some(&entry), None)?);
                    }
                }
                if let Change::Rename { to, .. } = change {
                    check_visibility_subtree(s, who, &files, to)?;
                }
            }
            Change::SetMeta {
                seen_from, colour, ..
            } => {
                let before = before.ok_or_else(|| err("not_found"))?;
                if seen_from.is_none() && colour.is_none() {
                    return Err(err("invalid_request"));
                }
                let tp = ranks::tenant_policy(s, &who.tenant_id);
                let mut entry = before.clone();
                if let Some(role) = seen_from {
                    let admin = s.policy.check(who, Permission::Admin, path).is_ok();
                    let own = ranks::role_rank(s, who);
                    let rung = tp.rank(&tp.rungs.visibility_from);
                    if !admin && !matches!((own, rung), (Some(o), Some(r)) if o >= r) {
                        return Err(err("forbidden"));
                    }
                    let wanted = tp.rank(role).ok_or_else(|| err("invalid_policy"))?;
                    if !admin && own.is_none_or(|o| wanted > o) {
                        return Err(err("visibility_above_own_rank"));
                    }
                    entry.meta.seen_from = Some(role.clone());
                }
                if let Some(name) = colour {
                    ranks::check(s, who, Permission::Edit, path)?;
                    if !ranks::COLOURS.contains(&name.as_str()) {
                        return Err(err("invalid_policy"));
                    }
                    entry.meta.colour = Some(name.clone());
                }
                diffs.push(make_diff(root, path, None, Some(&before), Some(&entry))?);
                files.insert(path.into(), entry);
                check_visibility_subtree(s, who, &files, path)?;
            }
            Change::Restore { revision_id, .. } => {
                if let Some(t) = tenant {
                    if t.pruned_revisions.contains(revision_id) {
                        return Err(err("history_pruned"));
                    }
                }
                let source = tenant
                    .and_then(|t| t.revisions.get(revision_id))
                    .ok_or_else(|| err("not_found"))?;
                let mut old = source
                    .files
                    .get(path)
                    .ok_or_else(|| err("not_found"))?
                    .clone();
                authorize_identity(s, who, &old, path)?;
                ranks::check_at(s, who, Permission::Read, path, &source.files)?;
                if files
                    .iter()
                    .any(|(p, e)| p != path && e.file_id == old.file_id)
                {
                    return Err(err("identity_conflict"));
                }
                if old.kind == EntryKind::Folder {
                    return Err(err("restore_folder_unsupported"));
                }
                for blob in [&old.blob, &old.original_blob].into_iter().flatten() {
                    storage::read_blob(root, blob)?;
                }
                if before.as_ref().is_some_and(|e| e.file_id != old.file_id) {
                    return Err(err("identity_conflict"));
                }
                check_entry_destination(&files, path, &old.kind)?;
                // Restore brings back content and identity only; historical meta is never
                // written back, so visibility changes only through set_meta.
                old.meta = before.as_ref().map(|e| e.meta.clone()).unwrap_or_default();
                diffs.push(make_diff(root, path, None, before.as_ref(), Some(&old))?);
                files.insert(path.into(), old);
                check_visibility_subtree(s, who, &files, path)?;
            }
        }
    }
    Ok((files, diffs))
}
fn check_entry_destination(
    files: &BTreeMap<String, Entry>,
    path: &str,
    kind: &EntryKind,
) -> Result<()> {
    check_parents(files, path)?;
    if *kind != EntryKind::Folder
        && (files.get(path).is_some_and(|e| e.kind == EntryKind::Folder)
            || files.keys().any(|p| p.starts_with(&format!("{path}/"))))
    {
        return Err(err("path_conflict"));
    }
    Ok(())
}
fn check_parents(files: &BTreeMap<String, Entry>, path: &str) -> Result<()> {
    let mut parent = path;
    while let Some((p, _)) = parent.rsplit_once('/') {
        if files.get(p).is_some_and(|e| e.kind != EntryKind::Folder) {
            return Err(err("path_conflict"));
        }
        parent = p;
    }
    Ok(())
}
/// No entry is visible below its folder: `path` and every entry under it that carries an
/// explicit `seen_from` must rank at least the effective `seen_from` of its parent (the
/// tenant root for a top-level entry). A role missing from the ladder fails closed
/// (`policy_set` keeps every pinned role, so this is a guard).
fn check_visibility_subtree(
    s: &State,
    who: &TrustedIdentity,
    files: &BTreeMap<String, Entry>,
    path: &str,
) -> Result<()> {
    let tp = ranks::tenant_policy(s, &who.tenant_id);
    let prefix = format!("{path}/");
    for (p, e) in files
        .iter()
        .filter(|(p, _)| *p == path || p.starts_with(&prefix))
    {
        let Some(own) = e.meta.seen_from.as_deref() else {
            continue;
        };
        let own = tp.rank(own).ok_or_else(|| err("invalid_policy"))?;
        let parent = p.rsplit_once('/').map_or("", |(parent, _)| parent);
        let floor = ranks::effective_seen_from(files, &tp, parent);
        if own < tp.rank(&floor).ok_or_else(|| err("invalid_policy"))? {
            return Err(err("visibility_below_folder"));
        }
    }
    Ok(())
}
fn make_diff(
    root: &Path,
    path: &str,
    previous_path: Option<String>,
    before: Option<&Entry>,
    after: Option<&Entry>,
) -> Result<FileDiff> {
    let text = |e: Option<&Entry>| -> Result<Option<String>> {
        match e {
            Some(e) if e.kind == EntryKind::Text => Ok(Some(
                String::from_utf8(storage::read_blob(
                    root,
                    e.blob.as_deref().ok_or_else(|| err("corrupt_blob"))?,
                )?)
                .map_err(io)?,
            )),
            _ => Ok(None),
        }
    };
    let has_meta = before.into_iter().chain(after).any(|e| !e.meta.is_empty());
    let is_text = |e: Option<&Entry>| e.is_none_or(|e| e.kind == EntryKind::Text);
    let unchanged = match (before, after) {
        (Some(b), Some(a)) => b.blob.is_some() && b.blob == a.blob,
        _ => false,
    };
    let (added_lines, removed_lines) = if unchanged
        || !is_text(before)
        || !is_text(after)
        || (before.is_none() && after.is_none())
    {
        (None, None)
    } else {
        let (added, removed) =
            render::line_counts(text(before)?.as_deref(), &text(after)?.unwrap_or_default());
        (Some(added), Some(removed))
    };
    Ok(FileDiff {
        path: path.into(),
        before: text(before)?,
        after: text(after)?.unwrap_or_default(),
        file_id: after.or(before).map(|e| e.file_id.clone()),
        previous_path,
        deleted: after.is_none(),
        before_blob: before.and_then(|e| e.blob.clone()),
        after_blob: after.and_then(|e| e.blob.clone()),
        kind: after.or(before).map(|e| e.kind.clone()),
        meta_before: has_meta.then(|| before.map(|e| e.meta.clone())).flatten(),
        meta_after: has_meta.then(|| after.map(|e| e.meta.clone())).flatten(),
        added_lines,
        removed_lines,
    })
}
impl Service {
    /// The caller's place in its tenant: any member or any grant holder in the tenant.
    pub fn me(&self, who: &TrustedIdentity) -> Result<ranks::MeResult> {
        self.transaction_for(who, false, vec![], false, None, None, |s| {
            let tp = ranks::tenant_policy(s, &who.tenant_id);
            let member = tp.member_role(&who.actor_id);
            if member.is_some_and(|m| m.kind != who.kind) {
                return Err(err("policy_mismatch"));
            }
            let holds_grant = s
                .policy
                .grants
                .iter()
                .any(|g| g.tenant_id == who.tenant_id && g.actor_id == who.actor_id);
            if who.tenant_id.is_empty()
                || who.actor_id.is_empty()
                || (member.is_none() && !holds_grant)
            {
                return Err(err("forbidden"));
            }
            Ok(ranks::MeResult {
                actor_id: who.actor_id.clone(),
                kind: who.kind.clone(),
                role: member.map(|m| m.role.clone()),
                roles: tp.roles.clone(),
                rungs: tp.rungs.clone(),
                root_seen_from: tp.root_seen_from.clone(),
            })
        })
    }
    /// Effective meta and the caller's permissions at a path it can read.
    pub fn permissions(
        &self,
        who: &TrustedIdentity,
        path: &str,
    ) -> Result<ranks::PermissionsResult> {
        self.transaction_for(who, false, vec![], false, None, None, |s| {
            let head = ranks::head_files(s, &who.tenant_id);
            // A path the caller may not read answers what a new file there would get. An
            // entry readable by rank whose earlier name is hidden (moved out of a hidden
            // folder) answers the same meta, and nothing is possible on it: `read` and
            // every write there answer `not_found`.
            let rank_hidden = ranks::check(s, who, Permission::Read, path).is_err();
            let lineage_hidden = !rank_hidden
                && head
                    .get(path)
                    .is_some_and(|e| authorize_identity(s, who, e, path).is_err());
            let files = if rank_hidden || lineage_hidden {
                std::borrow::Cow::Owned(ranks::as_absent(&head, path))
            } else {
                head
            };
            ranks::check_at(s, who, Permission::Read, path, &files)?;
            let tp = ranks::tenant_policy(s, &who.tenant_id);
            let (seen_from, inherited_from) = if path.is_empty() {
                (tp.root_seen_from.clone(), None)
            } else {
                ranks::seen_from_source(&files, &tp, path)
            };
            let (colour, colour_source) =
                ranks::effective_colour(&files, &tp, &who.tenant_id, path);
            Ok(ranks::PermissionsResult {
                path: path.into(),
                seen_from,
                inherited_from,
                colour,
                colour_source,
                can: if lineage_hidden {
                    ranks::Can {
                        read: false,
                        propose: false,
                        edit: false,
                        approve: false,
                        admin: false,
                    }
                } else {
                    ranks::can_at(s, who, path, &files)
                },
            })
        })
    }
    /// Who may answer a proposal the caller can review.
    pub fn deciders(&self, who: &TrustedIdentity, id: &str) -> Result<ranks::DecidersResult> {
        self.transaction_for(who, false, vec![], false, Some(id), None, |s| {
            let p = reviewable(s, who, id)?;
            // Nobody can still decide a proposal that has ended.
            if !matches!(p.state, ProposalState::Pending | ProposalState::Approved) {
                return Ok(ranks::DecidersResult {
                    proposal_id: p.id.clone(),
                    deciders: vec![],
                    fallback: false,
                });
            }
            let (deciders, fallback) = proposal_deciders(s, &p);
            Ok(ranks::DecidersResult {
                proposal_id: p.id.clone(),
                deciders,
                fallback,
            })
        })
    }
    pub fn read_binary(
        &self,
        who: &TrustedIdentity,
        path: &str,
        revision: Option<&str>,
        original: bool,
        encoding: Option<&str>,
    ) -> Result<BinaryReadResult> {
        let base64 = match encoding {
            None | Some("array") => false,
            Some("base64") => true,
            Some(_) => {
                return Err(ContentError::new(
                    "invalid_request",
                    "encoding must be \"array\" or \"base64\"",
                ))
            }
        };
        valid_path(path)?;
        self.transaction_for(
            who,
            false,
            revision.into_iter().map(str::to_owned).collect(),
            false,
            None,
            None,
            |s| {
                ranks::check(s, who, Permission::Read, path)?;
                let t = s
                    .tenants
                    .get(&who.tenant_id)
                    .ok_or_else(|| err("not_found"))?;
                let id = revision.unwrap_or(&t.head);
                if t.pruned_revisions.contains(id) {
                    return Err(err("history_pruned"));
                }
                let snap = t.revisions.get(id).ok_or_else(|| err("not_found"))?;
                let entry = snap.files.get(path).ok_or_else(|| err("not_found"))?;
                authorize_identity(s, who, entry, path)?;
                ranks::check_at(s, who, Permission::Read, path, &snap.files)?;
                let blob = if original {
                    &entry.original_blob
                } else {
                    &entry.blob
                };
                let bytes = match blob {
                    Some(blob) => storage::read_blob(&self.root, blob)?,
                    None => vec![],
                };
                let (bytes, bytes_base64) = if base64 {
                    use base64::Engine;
                    let text = base64::engine::general_purpose::STANDARD.encode(&bytes);
                    (vec![], Some(text))
                } else {
                    (bytes, None)
                };
                Ok(BinaryReadResult {
                    revision_id: id.into(),
                    path: path.into(),
                    entry: entry.clone(),
                    bytes,
                    bytes_base64,
                })
            },
        )
    }
}
pub(crate) fn migrate_v1(root: &Path, mut value: serde_json::Value) -> Result<State> {
    if value["version"] != 1 {
        return Err(err("unsupported_store_version"));
    }
    if let Some(tenants) = value["tenants"].as_object_mut() {
        for (tenant, t) in tenants {
            let mut ids = BTreeMap::new();
            if let Some(revisions) = t["revisions"].as_object_mut() {
                for snap in revisions.values_mut() {
                    if let Some(files) = snap["files"].as_object_mut() {
                        for (path, content) in files {
                            let text = content
                                .as_str()
                                .ok_or_else(|| err("invalid_legacy_store"))?;
                            let blob = storage::put_blob(root, text.as_bytes())?;
                            let id =
                                format!("f{}", fingerprint(&(tenant.as_str(), path.as_str()))?);
                            ids.insert(path.clone(), id.clone());
                            *content = serde_json::to_value(Entry {
                                file_id: id,
                                kind: EntryKind::Text,
                                blob: Some(blob.clone()),
                                original_blob: Some(blob),
                                meta: Default::default(),
                            })
                            .map_err(io)?;
                        }
                    }
                    if let Some(changes) = snap["revision"]["changes"].as_array_mut() {
                        for c in changes {
                            if let Some(id) = c["path"].as_str().and_then(|p| ids.get(p)) {
                                c["file_id"] = id.clone().into();
                            }
                        }
                    }
                }
            }
        }
    }
    value["version"] = 2.into();
    let mut state: State = serde_json::from_value(value).map_err(io)?;
    for tenant in state.tenants.values_mut() {
        for snap in tenant.revisions.values() {
            record_identity_paths(&mut tenant.identity_paths, &snap.revision.changes);
        }
        let mut chain = vec![];
        let mut seen = BTreeSet::new();
        let mut at = (!tenant.head.is_empty()).then(|| tenant.head.clone());
        while let Some(id) = at {
            if !seen.insert(id.clone()) {
                return Err(err("invalid_legacy_store"));
            }
            let snap = tenant
                .revisions
                .get(&id)
                .ok_or_else(|| err("invalid_legacy_store"))?;
            at = snap.revision.parent.clone();
            chain.push(id);
        }
        let mut originals = BTreeMap::new();
        for id in chain.iter().rev() {
            for entry in tenant.revisions.get_mut(id).unwrap().files.values_mut() {
                if let Some(blob) = &entry.blob {
                    let original = originals
                        .entry(entry.file_id.clone())
                        .or_insert_with(|| blob.clone());
                    entry.original_blob = Some(original.clone());
                }
            }
        }
    }
    Ok(state)
}

fn check_diffs(
    s: &State,
    who: &TrustedIdentity,
    permission: Permission,
    diffs: &[FileDiff],
) -> Result<()> {
    for c in diffs {
        ranks::check(s, who, permission, &c.path)?;
        ranks::check(s, who, Permission::Read, &c.path)?;
        if let Some(path) = &c.previous_path {
            ranks::check(s, who, permission, path)?;
            ranks::check(s, who, Permission::Read, path)?;
        }
    }
    Ok(())
}

fn restore_revisions(changes: &[Change]) -> Vec<String> {
    changes
        .iter()
        .filter_map(|c| match c {
            Change::Restore { revision_id, .. } => Some(revision_id.clone()),
            _ => None,
        })
        .collect()
}

fn authorize_replay(s: &State, who: &TrustedIdentity, value: &serde_json::Value) -> Result<()> {
    match value {
        serde_json::Value::Object(fields) => {
            if let Some(path) = fields.get("path").and_then(serde_json::Value::as_str) {
                ranks::check(s, who, Permission::Read, path).map_err(|_| err("not_found"))?;
                if let Some(id) = fields.get("file_id").and_then(serde_json::Value::as_str) {
                    authorize_identity(
                        s,
                        who,
                        &Entry {
                            file_id: id.into(),
                            kind: EntryKind::Text,
                            blob: None,
                            original_blob: None,
                            meta: Default::default(),
                        },
                        path,
                    )
                    .map_err(|_| err("not_found"))?;
                }
            }
            if fields
                .get("file_id")
                .and_then(serde_json::Value::as_str)
                .is_none()
            {
                if let Some(path) = fields.get("path").and_then(serde_json::Value::as_str) {
                    authorize_legacy_path(s, who, path)?;
                }
            }
            for value in fields.values() {
                authorize_replay(s, who, value)?;
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                authorize_replay(s, who, value)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn record_identity_paths(paths: &mut BTreeMap<String, BTreeSet<String>>, changes: &[FileDiff]) {
    for c in changes {
        if let Some(id) = &c.file_id {
            let names = paths.entry(id.clone()).or_default();
            names.insert(c.path.clone());
            if let Some(old) = &c.previous_path {
                names.insert(old.clone());
            }
        }
    }
}

fn authorize_legacy_path(s: &State, who: &TrustedIdentity, path: &str) -> Result<()> {
    if let Some(t) = s.tenants.get(&who.tenant_id) {
        for names in t
            .identity_paths
            .values()
            .filter(|names| names.contains(path))
        {
            for name in names {
                ranks::check(s, who, Permission::Read, name).map_err(|_| err("not_found"))?;
            }
        }
    }
    Ok(())
}

/// A policy change returns the tenant's outstanding approvals to pending.
fn reopen_approvals(s: &mut State, tenant_id: &str) {
    if let Some(tenant) = s.tenants.get_mut(tenant_id) {
        for p in tenant.proposals.values_mut() {
            if p.state == ProposalState::Approved {
                p.state = ProposalState::Pending;
                p.approver = None;
            }
        }
    }
}

/// The caller's grants plus its tenant section (default when none is stored).
fn caller_policy(stored: &Policy, tenant_id: &str) -> Policy {
    Policy {
        grants: stored
            .grants
            .iter()
            .filter(|g| g.tenant_id == tenant_id)
            .cloned()
            .collect(),
        tenants: std::collections::BTreeMap::from([(
            tenant_id.to_string(),
            stored.tenants.get(tenant_id).cloned().unwrap_or_default(),
        )]),
    }
}

fn valid_tenant_policy(t: &auth::TenantPolicy) -> bool {
    let known = |r: &String| t.roles.contains(r);
    (1..=16).contains(&t.roles.len())
        && t.roles.iter().enumerate().all(|(i, r)| {
            (1..=64).contains(&r.chars().count())
                && !r.chars().any(char::is_control)
                && !t.roles[..i].contains(r)
        })
        && t.members.iter().all(|(id, m)| {
            !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control) && known(&m.role)
        })
        && known(&t.rungs.propose_from)
        && known(&t.rungs.publish_from)
        && known(&t.rungs.visibility_from)
        && known(&t.root_seen_from)
}

fn validate_policy(policy: &Policy) -> Result<()> {
    if policy.grants.len() > 10_000
        || policy.grants.iter().any(|g| {
            [&g.tenant_id, &g.actor_id]
                .iter()
                .any(|s| s.is_empty() || s.len() > 256 || s.chars().any(char::is_control))
                || !auth::valid_path(&g.scope, true)
        })
        || policy
            .tenants
            .keys()
            .any(|id| id.is_empty() || id.len() > 256)
        || !policy.tenants.values().all(valid_tenant_policy)
    {
        return Err(err("invalid_policy"));
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PolicyState {
    pub policy: Policy,
    pub digest: String,
}

#[cfg(test)]
mod history_records {
    use super::*;

    #[test]
    fn timestamps_are_fixed_width_milliseconds() {
        let base = time::macros::datetime!(2026-10-28 22:00:00 UTC);
        assert_eq!(format_at(base), "2026-10-28T22:00:00.000Z");
        let t = base.replace_nanosecond(120_999_999).unwrap();
        assert_eq!(format_at(t), "2026-10-28T22:00:00.120Z");
        let t = base.replace_nanosecond(7_000_000).unwrap();
        assert_eq!(format_at(t), "2026-10-28T22:00:00.007Z");
    }

    fn put() -> Vec<Change> {
        vec![Change::Put {
            path: "a.md".into(),
            content: "a".into(),
        }]
    }
    fn origin() -> Origin {
        Origin {
            source: "assistant".into(),
            model: None,
            chat_id: None,
        }
    }

    #[test]
    fn records_without_new_fields_keep_legacy_replay_fingerprints() {
        // SHA-256 of `["edit","r0",[{"type":"put","path":"a.md","content":"a"}]]` etc.
        assert_eq!(
            edit_fingerprint("r0", &put(), None, None).unwrap(),
            "19570146fc652231b01b0f307ad45483647358822d70fac1890dc7757f725011"
        );
        assert_eq!(
            propose_fingerprint("r0", &put(), None, None).unwrap(),
            "be75836512acfbc0bb9bb77672836faba319a8f4c009f2b79830b9aff5abff57"
        );
        assert_eq!(
            decision_fingerprint("p1", "d1", true, None).unwrap(),
            "e64760be73cc7836e3a33551f945c5afe0131811ac2276e54955bff3e13b9724"
        );
        let legacy = edit_fingerprint("r0", &put(), None, None).unwrap();
        assert_ne!(
            edit_fingerprint("r0", &put(), Some("m"), None).unwrap(),
            legacy
        );
        assert_ne!(
            edit_fingerprint("r0", &put(), None, Some(&origin())).unwrap(),
            legacy
        );
        assert_ne!(
            propose_fingerprint("r0", &put(), Some("r"), None).unwrap(),
            propose_fingerprint("r0", &put(), None, None).unwrap()
        );
        assert_ne!(
            decision_fingerprint("p1", "d1", true, Some("r")).unwrap(),
            decision_fingerprint("p1", "d1", true, None).unwrap()
        );
    }
}
