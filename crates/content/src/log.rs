//! The tenant-wide log: revisions newest first, each reduced to what the caller may see.
//! A revision appears when any of its changes inside the scope is readable now and was
//! readable then; hidden revisions are skipped before `offset` and `limit` count.
use super::*;

/// Paths named on one entry; `path_count` counts them all.
const MAX_PATHS: usize = 8;
const MAX_LIMIT: usize = 100;
const MAX_OFFSET: usize = 1_000_000;
/// Each pass that lacks manifests asks for twice as many candidates as the last.
const MAX_PASSES: u32 = 32;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LogEntry {
    pub id: String,
    pub parent: Option<String>,
    pub at: Option<String>,
    pub actor: TrustedIdentity,
    pub proposal_id: Option<String>,
    pub proposer: Option<TrustedIdentity>,
    pub approver: Option<TrustedIdentity>,
    pub message: Option<String>,
    pub origin: Option<Origin>,
    pub kind: Option<RevisionKind>,
    /// The first eight visible changed paths, in change order.
    pub paths: Vec<String>,
    pub path_count: u32,
    /// Changes inside the scope the caller may not see.
    pub hidden_paths: u32,
    pub added_lines: Option<u32>,
    pub removed_lines: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LogResult {
    /// The head the log was read from.
    pub revision_id: String,
    pub entries: Vec<LogEntry>,
    pub next_offset: Option<usize>,
}

/// `path` is `scope` or below it; the empty scope is the whole tenant.
fn within(path: &str, scope: &str) -> bool {
    scope.is_empty()
        || path
            .strip_prefix(scope)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

fn in_scope(c: &FileDiff, scope: &str) -> bool {
    within(&c.path, scope) || c.previous_path.as_deref().is_some_and(|p| within(p, scope))
}

/// Sum of the counts the visible changes carry; `None` when none carries one.
fn total(counts: impl Iterator<Item = Option<u32>>) -> Option<u32> {
    counts
        .flatten()
        .fold(None, |sum, n| Some(sum.unwrap_or(0u32).saturating_add(n)))
}

fn entry(t: &Tenant, revision: &Revision, visible: &[&FileDiff], hidden: usize) -> LogEntry {
    let proposal = revision
        .proposal_id
        .as_ref()
        .and_then(|id| t.proposals.get(id));
    LogEntry {
        id: revision.id.clone(),
        parent: revision.parent.clone(),
        at: revision.at.clone(),
        actor: revision.actor.clone(),
        proposal_id: revision.proposal_id.clone(),
        proposer: proposal.map(|p| p.proposer.clone()),
        approver: proposal.and_then(|p| p.approver.clone()),
        message: revision.message.clone(),
        origin: revision.origin.clone(),
        kind: revision.kind,
        paths: visible
            .iter()
            .take(MAX_PATHS)
            .map(|c| c.path.clone())
            .collect(),
        path_count: visible.len() as u32,
        hidden_paths: hidden as u32,
        added_lines: total(visible.iter().map(|c| c.added_lines)),
        removed_lines: total(visible.iter().map(|c| c.removed_lines)),
    }
}

impl Service {
    pub fn log(
        &self,
        who: &TrustedIdentity,
        scope: &str,
        limit: usize,
        offset: usize,
    ) -> Result<LogResult> {
        if !scope.is_empty() {
            valid_path(scope)?;
        }
        if limit == 0 || limit > MAX_LIMIT || offset > MAX_OFFSET {
            return Err(err("invalid_request"));
        }
        // Revision-time visibility needs manifests, which cost a reconstruction each; a
        // pass asks only for those of candidates it reaches, doubling its reach each time.
        let mut hydrated = HashSet::new();
        for pass in 0..MAX_PASSES {
            let reach = offset
                .saturating_add(limit)
                .saturating_add(1)
                .saturating_mul(1 << pass.min(20));
            match self.log_over(who, scope, limit, offset, &hydrated, reach)? {
                Ok(result) => return Ok(result),
                Err(needed) => hydrated = needed,
            }
        }
        Err(err("conflict"))
    }

    /// The page, or the revisions whose manifests it needs when they are not loaded.
    fn log_over(
        &self,
        who: &TrustedIdentity,
        scope: &str,
        limit: usize,
        offset: usize,
        hydrated: &HashSet<String>,
        reach: usize,
    ) -> Result<std::result::Result<LogResult, HashSet<String>>> {
        let revisions = hydrated.iter().cloned().collect();
        self.transaction_metadata(who, revisions, |s| {
            ranks::check(s, who, Permission::Read, scope)?;
            let Some(t) = s.tenants.get(&who.tenant_id).filter(|t| !t.head.is_empty()) else {
                return Ok(Ok(LogResult {
                    revision_id: "r0".into(),
                    entries: vec![],
                    next_offset: None,
                }));
            };
            // A scope no revision ever touched answers as a hidden one does.
            if !scope.is_empty()
                && !chain(t, &t.head).iter().any(|id| {
                    t.revisions[id]
                        .revision
                        .changes
                        .iter()
                        .any(|c| in_scope(c, scope))
                })
            {
                return Err(err("not_found"));
            }
            let loaded = |id: &str| id == t.head || hydrated.contains(id);
            let mut entries = vec![];
            let mut included = 0usize;
            let mut more = false;
            let mut needed = HashSet::new();
            let mut pending = 0usize;
            for id in chain(t, &t.head) {
                let revision = &t.revisions[&id].revision;
                let scoped: Vec<&FileDiff> = revision
                    .changes
                    .iter()
                    .filter(|c| in_scope(c, scope))
                    .collect();
                // Readable now: a revision with nothing readable at head needs no manifest.
                let now: Vec<&FileDiff> = scoped
                    .iter()
                    .copied()
                    .filter(|c| authorize_diff(s, who, c).is_ok())
                    .collect();
                if now.is_empty() {
                    continue;
                }
                let manifests = std::iter::once(&revision.id).chain(revision.parent.as_ref());
                if !needed.is_empty() || !manifests.clone().all(|m| loaded(m)) {
                    needed.extend(manifests.cloned());
                    pending += 1;
                    if included + pending >= reach {
                        break;
                    }
                    continue;
                }
                let visible: Vec<&FileDiff> = now
                    .into_iter()
                    .filter(|c| {
                        readable_then(s, who, t, revision, std::slice::from_ref(*c)).is_ok()
                    })
                    .collect();
                if visible.is_empty() {
                    continue;
                }
                if included >= offset.saturating_add(limit) {
                    more = true;
                    break;
                }
                if included >= offset {
                    entries.push(entry(t, revision, &visible, scoped.len() - visible.len()));
                }
                included += 1;
            }
            if !needed.is_empty() {
                needed.extend(hydrated.iter().cloned());
                return Ok(Err(needed));
            }
            Ok(Ok(LogResult {
                revision_id: t.head.clone(),
                next_offset: more.then_some(offset.saturating_add(limit)),
                entries,
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_is_a_path_or_its_descendants() {
        assert!(within("a/b.md", ""));
        assert!(within("a", "a"));
        assert!(within("a/b.md", "a"));
        assert!(!within("ab/c.md", "a"));
        assert!(!within("a", "a/b"));
    }

    #[test]
    fn totals_are_none_only_without_counts() {
        assert_eq!(total([None, None].into_iter()), None);
        assert_eq!(total([None, Some(0)].into_iter()), Some(0));
        assert_eq!(total([Some(2), None, Some(3)].into_iter()), Some(5));
    }
}
