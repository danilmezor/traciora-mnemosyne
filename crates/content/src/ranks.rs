//! Rank arithmetic and per-path meta: effective visibility and colour.
use crate::{
    auth::{valid_path, ActorKind, Permission, Rungs, TenantPolicy, TrustedIdentity},
    ContentError, Entry, EntryKind, State,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{borrow::Cow, collections::BTreeMap};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seen_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub colour: Option<String>,
}
impl PathMeta {
    pub fn is_empty(&self) -> bool {
        self.seen_from.is_none() && self.colour.is_none()
    }
}

pub const COLOURS: [&str; 7] = [
    "blue", "magenta", "green", "lavender", "orange", "cyan", "other",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColourSource {
    Explicit,
    Inherited,
    Default,
}

/// The path itself, then each ancestor, nearest first.
fn lineage(path: &str) -> impl Iterator<Item = &str> {
    std::iter::successors(Some(path), |p| p.rsplit_once('/').map(|(parent, _)| parent))
}

/// Nearest explicit seen_from on `path` or an ancestor present in `files`, else `tenant.root_seen_from`.
pub fn effective_seen_from(
    files: &BTreeMap<String, Entry>,
    tenant: &TenantPolicy,
    path: &str,
) -> String {
    seen_from_source(files, tenant, path).0
}

/// `effective_seen_from` plus the ancestor that supplied it; `None` when it is explicit
/// on `path` itself or comes from the tenant root.
pub(crate) fn seen_from_source(
    files: &BTreeMap<String, Entry>,
    tenant: &TenantPolicy,
    path: &str,
) -> (String, Option<String>) {
    lineage(path)
        .filter_map(|p| files.get(p).map(|e| (p, e)))
        .find_map(|(p, e)| {
            e.meta
                .seen_from
                .clone()
                .map(|r| (r, (p != path).then(|| p.to_string())))
        })
        .unwrap_or_else(|| (tenant.root_seen_from.clone(), None))
}

fn seed(tenant: &TenantPolicy, tenant_id: &str) -> u64 {
    tenant.colour_seed.unwrap_or_else(|| {
        let d = Sha256::digest(tenant_id.as_bytes());
        u64::from_be_bytes(d[..8].try_into().unwrap())
    })
}

/// Fisher-Yates over the first six colours, driven by a 64-bit LCG.
fn shuffled(seed: u64) -> [&'static str; 6] {
    let mut pool = [
        COLOURS[0], COLOURS[1], COLOURS[2], COLOURS[3], COLOURS[4], COLOURS[5],
    ];
    let mut state = seed;
    for i in (1..pool.len()).rev() {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        pool.swap(i, (state >> 33) as usize % (i + 1));
    }
    pool
}

/// Explicit, else nearest ancestor's explicit, else for a top-level folder the seed's shuffle of the
/// six by creation order (folders sorted by file_id), `other` from the seventh; a top-level file is `other`.
pub fn effective_colour(
    files: &BTreeMap<String, Entry>,
    tenant: &TenantPolicy,
    tenant_id: &str,
    path: &str,
) -> (String, ColourSource) {
    for (i, p) in lineage(path).enumerate() {
        if let Some(c) = files.get(p).and_then(|e| e.meta.colour.clone()) {
            let source = if i == 0 {
                ColourSource::Explicit
            } else {
                ColourSource::Inherited
            };
            return (c, source);
        }
    }
    let top = path.split('/').next().unwrap_or(path);
    let default = || ("other".to_string(), ColourSource::Default);
    let Some(entry) = files.get(top).filter(|e| e.kind == EntryKind::Folder) else {
        return default();
    };
    let mut folders: Vec<&str> = files
        .iter()
        .filter(|(p, e)| e.kind == EntryKind::Folder && !p.contains('/'))
        .map(|(_, e)| e.file_id.as_str())
        .collect();
    folders.sort_unstable();
    let index = folders.iter().position(|id| *id == entry.file_id);
    match index.and_then(|i| shuffled(seed(tenant, tenant_id)).get(i).copied()) {
        Some(c) => (c.to_string(), ColourSource::Default),
        None => default(),
    }
}

/// `me`: who the caller is in its tenant ladder.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MeResult {
    pub actor_id: String,
    pub kind: ActorKind,
    pub role: Option<String>,
    pub roles: Vec<String>,
    pub rungs: Rungs,
    pub root_seen_from: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Can {
    pub read: bool,
    pub propose: bool,
    pub edit: bool,
    pub approve: bool,
    pub admin: bool,
}
/// `permissions`: a readable path's effective meta and what the caller may do there.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PermissionsResult {
    pub path: String,
    pub seen_from: String,
    pub inherited_from: Option<String>,
    pub colour: String,
    pub colour_source: ColourSource,
    pub can: Can,
}
/// `deciders`: who `decide` would accept on a proposal, proposer and its `on_behalf_of`
/// excluded; `fallback` when only root administrators-by-grant remain. Grants carry no
/// kind, so a non-member grant holder is assumed human (an agent credential holding such a
/// grant may be listed yet refused with `human_required`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DecidersResult {
    pub proposal_id: String,
    pub deciders: Vec<String>,
    pub fallback: bool,
}

/// What `who` may do at `path` in `files`, each permission through `check_at`.
pub(crate) fn can_at(
    s: &State,
    who: &TrustedIdentity,
    path: &str,
    files: &BTreeMap<String, Entry>,
) -> Can {
    let ok = |p| check_at(s, who, p, path, files).is_ok();
    Can {
        read: ok(Permission::Read),
        propose: ok(Permission::Propose),
        edit: ok(Permission::Edit),
        approve: ok(Permission::Approve),
        admin: ok(Permission::Admin),
    }
}

/// `files` with `path` and everything below it removed: the manifest in which `path` is
/// absent. A path the caller may not read answers what it would answer here.
pub(crate) fn as_absent(files: &BTreeMap<String, Entry>, path: &str) -> BTreeMap<String, Entry> {
    let prefix = format!("{path}/");
    files
        .iter()
        .filter(|(p, _)| *p != path && !p.starts_with(&prefix))
        .map(|(p, e)| (p.clone(), e.clone()))
        .collect()
}

/// Who may answer a proposal touching `paths`: exactly the candidates `decide` would
/// accept (tenant gate, then Read and Approve on every path), minus `excluded`. Candidates
/// are human members and holders of an Approve or Admin grant in the tenant; a non-member
/// is assumed human because grants carry no kind. Sorted; the flag is true when every
/// decider qualifies only through the root-administrator rule.
pub(crate) fn deciders(
    s: &State,
    tenant_id: &str,
    paths: &[&str],
    excluded: &[&str],
) -> (Vec<String>, bool) {
    let tp = tenant_policy(s, tenant_id);
    let files = head_files(s, tenant_id);
    let mut candidates: Vec<&str> = tp
        .members
        .iter()
        .filter(|(_, m)| m.kind == ActorKind::Human)
        .map(|(id, _)| id.as_str())
        .chain(
            s.policy
                .grants
                .iter()
                .filter(|g| {
                    g.tenant_id == tenant_id
                        && matches!(g.permission, Permission::Approve | Permission::Admin)
                })
                .map(|g| g.actor_id.as_str()),
        )
        .filter(|id| !id.is_empty() && !excluded.contains(id))
        .collect();
    candidates.sort_unstable();
    candidates.dedup();
    let mut out = Vec::new();
    let mut fallback = true;
    for id in candidates {
        let kind = tp
            .member_role(id)
            .map_or(ActorKind::Human, |m| m.kind.clone());
        if kind != ActorKind::Human {
            continue;
        }
        let who = TrustedIdentity {
            tenant_id: tenant_id.into(),
            actor_id: id.into(),
            kind,
            on_behalf_of: None,
        };
        let accepted = |admin_rule: bool| {
            paths.iter().all(|p| {
                check_with(s, &who, Permission::Read, p, &files, admin_rule).is_ok()
                    && check_with(s, &who, Permission::Approve, p, &files, admin_rule).is_ok()
            })
        };
        if crate::read_tenant(s, &who).is_ok() && accepted(true) {
            fallback &= !accepted(false);
            out.push(id.to_string());
        }
    }
    (out, fallback)
}

/// The stored ladder of `tenant_id`, or the default ladder when none is stored.
pub(crate) fn tenant_policy<'a>(s: &'a State, tenant_id: &str) -> Cow<'a, TenantPolicy> {
    s.policy
        .tenants
        .get(tenant_id)
        .map_or_else(|| Cow::Owned(TenantPolicy::default()), Cow::Borrowed)
}

/// The caller's own rank in its tenant ladder; `None` when it is not a member.
pub(crate) fn role_rank(s: &State, who: &TrustedIdentity) -> Option<usize> {
    let tp = tenant_policy(s, &who.tenant_id);
    let rank = tp.member_role(&who.actor_id).and_then(|m| tp.rank(&m.role));
    rank
}

/// The head manifest of the caller's tenant; empty when the tenant has no revisions yet.
pub(crate) fn head_files<'a>(s: &'a State, tenant_id: &str) -> Cow<'a, BTreeMap<String, Entry>> {
    s.tenants
        .get(tenant_id)
        .and_then(|t| t.revisions.get(&t.head))
        .map_or_else(
            || Cow::Owned(BTreeMap::new()),
            |snap| Cow::Borrowed(&snap.files),
        )
}

/// The identity whose rank decides which link targets `who` is told are above rank
///, or `None` when it is told of none: a human itself; a delegated
/// agent, when the tenant allows it, its human, so it gets exactly the list that human
/// would get (never a name its human reads, nor one hidden only from its own rank); an
/// agent alone none.
pub(crate) fn above_rank_viewer(s: &State, who: &TrustedIdentity) -> Option<TrustedIdentity> {
    match (&who.kind, &who.on_behalf_of) {
        (ActorKind::Human, _) => Some(who.clone()),
        (ActorKind::Agent, Some(human))
            if tenant_policy(s, &who.tenant_id).agent_sees_above_rank =>
        {
            Some(TrustedIdentity {
                tenant_id: who.tenant_id.clone(),
                actor_id: human.clone(),
                kind: ActorKind::Human,
                on_behalf_of: None,
            })
        }
        (ActorKind::Agent, _) => None,
    }
}

/// The single authorization function, evaluated against the head manifest.
/// Grants ∪ rank; agents get read only from rank; `policy_mismatch` when the credential
/// kind disagrees with membership; an agent's `on_behalf_of` human must pass the same check.
/// A member denied `Read` gets `not_found` (except the tenant root, `forbidden`); a member
/// who can read but sits below a rung gets `forbidden`; non-members keep grant behaviour.
pub(crate) fn check(
    s: &State,
    who: &TrustedIdentity,
    permission: Permission,
    path: &str,
) -> Result<(), ContentError> {
    check_at(s, who, permission, path, &head_files(s, &who.tenant_id))
}

/// `check` against an arbitrary manifest: historical reads pass it at head and at the
/// revision being read.
pub(crate) fn check_at(
    s: &State,
    who: &TrustedIdentity,
    permission: Permission,
    path: &str,
    files: &BTreeMap<String, Entry>,
) -> Result<(), ContentError> {
    check_with(s, who, permission, path, files, true)
}

/// `check_at`, optionally without the root-administrator rule.
fn check_with(
    s: &State,
    who: &TrustedIdentity,
    permission: Permission,
    path: &str,
    files: &BTreeMap<String, Entry>,
    admin_rule: bool,
) -> Result<(), ContentError> {
    let tp = tenant_policy(s, &who.tenant_id);
    let membership = tp.member_role(&who.actor_id);
    if membership.is_some_and(|m| m.kind != who.kind) {
        return Err(ContentError::new(
            "policy_mismatch",
            "Credential kind does not match membership",
        ));
    }
    if s.policy.check(who, permission, path).is_ok() {
        return Ok(());
    }
    if !valid_path(path, true) || who.tenant_id.is_empty() || who.actor_id.is_empty() {
        return Err(forbidden());
    }
    if admin_rule
        && matches!(permission, Permission::Read | Permission::Approve)
        && root_admin(s, who)
    {
        return Ok(());
    }
    if permits(s, &tp, files, who, permission, path) {
        return Ok(());
    }
    if membership.is_none()
        || path.is_empty()
        || (permission != Permission::Read && permits(s, &tp, files, who, Permission::Read, path))
    {
        Err(forbidden())
    } else {
        Err(ContentError::new("not_found", "not found"))
    }
}

/// A human holding an `Admin` grant at the tenant root: reads and approves every path.
/// Agents and scoped Admin grants get nothing from this rule.
fn root_admin(s: &State, who: &TrustedIdentity) -> bool {
    who.kind == ActorKind::Human && s.policy.check(who, Permission::Admin, "").is_ok()
}

fn forbidden() -> ContentError {
    ContentError::new("forbidden", "Access denied")
}

/// Own authority (grant or rank) intersected with the delegating human's, if any.
fn permits(
    s: &State,
    tp: &TenantPolicy,
    files: &BTreeMap<String, Entry>,
    who: &TrustedIdentity,
    permission: Permission,
    path: &str,
) -> bool {
    if tp
        .member_role(&who.actor_id)
        .is_some_and(|m| m.kind != who.kind)
    {
        return false;
    }
    let alone = TrustedIdentity {
        on_behalf_of: None,
        ..who.clone()
    };
    let own = s.policy.check(&alone, permission, path).is_ok()
        || by_rank(s, tp, files, who, permission, path);
    let delegated = match (&who.kind, &who.on_behalf_of) {
        (ActorKind::Agent, Some(actor)) => {
            !actor.is_empty()
                && actor != &who.actor_id
                && permits(
                    s,
                    tp,
                    files,
                    &TrustedIdentity {
                        tenant_id: who.tenant_id.clone(),
                        actor_id: actor.clone(),
                        kind: ActorKind::Human,
                        on_behalf_of: None,
                    },
                    permission,
                    path,
                )
        }
        (_, None) => true,
        (ActorKind::Human, Some(_)) => false,
    };
    own && delegated
}

/// Rank rule over `files`: Read from `seen_from`; Propose and Edit/Approve additionally
/// from the rungs and only for humans; Admin never.
fn by_rank(
    s: &State,
    tp: &TenantPolicy,
    files: &BTreeMap<String, Entry>,
    who: &TrustedIdentity,
    permission: Permission,
    path: &str,
) -> bool {
    let Some(rank) = role_rank(s, who) else {
        return false;
    };
    let at_least = |role: &str| tp.rank(role).is_some_and(|needed| rank >= needed);
    let seen_from = if path.is_empty() {
        tp.root_seen_from.clone()
    } else {
        effective_seen_from(files, tp, path)
    };
    let read = at_least(&seen_from);
    let human = who.kind == ActorKind::Human;
    match permission {
        Permission::Read => read,
        Permission::Propose => read && human && at_least(&tp.rungs.propose_from),
        Permission::Edit | Permission::Approve => read && human && at_least(&tp.rungs.publish_from),
        Permission::Admin => false,
    }
}
