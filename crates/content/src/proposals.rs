//! Compact proposal discovery exposes exactly the proposals current review permits.
//! Bodies are obtained separately through review(); a summary names only the first few
//! readable paths, never contents.
//! Authorization and state filtering precede pagination; no global count or
//! hidden proposal cursor is returned. Ordering is stable lexicographic ID order;
//! offset pages are live views and may shift when proposals or policy change.
use crate::{
    auth::{Permission, TrustedIdentity},
    authorize_diff, check_changes, err, read_tenant, storage, Change, ProposalState, Result,
    Service,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProposalListRequest {
    #[serde(default)]
    pub states: Vec<ProposalState>,
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default)]
    pub offset: usize,
}
fn default_limit() -> usize {
    20
}
impl Default for ProposalListRequest {
    fn default() -> Self {
        Self {
            states: vec![],
            limit: default_limit(),
            offset: 0,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProposalSummary {
    pub id: String,
    pub state: ProposalState,
    pub digest: String,
    pub base_revision: String,
    pub revision_id: Option<String>,
    pub proposer: TrustedIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<crate::Origin>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
    /// The first eight changed paths the caller may read.
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub path_count: u32,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProposalListResult {
    pub proposals: Vec<ProposalSummary>,
    pub next_offset: Option<usize>,
}
impl Service {
    pub fn list_proposals(
        &self,
        who: &TrustedIdentity,
        request: &ProposalListRequest,
    ) -> Result<ProposalListResult> {
        if request.limit == 0
            || request.limit > 100
            || request.offset > 1_000_000
            || request.states.len() > 6
        {
            return Err(err("invalid_request"));
        }
        storage::transaction(
            &self.root,
            false,
            storage::Scope {
                tenant: Some(&who.tenant_id),
                revisions: vec![],
                history: false,
                proposal: None,
                replay: None,
                hydrate_text: true,
                metadata: vec![],
            },
            |state| {
                read_tenant(state, who)?;
                let Some(tenant) = state.tenants.get(&who.tenant_id) else {
                    return Ok(ProposalListResult {
                        proposals: vec![],
                        next_offset: None,
                    });
                };
                // Keep this identical to review(), including changes and all diff
                // names/lineage. An unreadable member hides the complete proposal.
                let mut visible = tenant
                    .proposals
                    .values()
                    .filter(|p| {
                        (request.states.is_empty() || request.states.contains(&p.state))
                            && check_changes(state, who, Permission::Read, &p.changes).is_ok()
                            && p.diff
                                .iter()
                                .all(|diff| authorize_diff(state, who, diff).is_ok())
                    })
                    .skip(request.offset)
                    .take(request.limit + 1)
                    .map(|p| {
                        let visible_paths = p
                            .changes
                            .iter()
                            .map(Change::path)
                            .filter(|path| {
                                crate::ranks::check(state, who, Permission::Read, path).is_ok()
                            })
                            .collect::<Vec<_>>();
                        ProposalSummary {
                            id: p.id.clone(),
                            state: p.state.clone(),
                            digest: p.digest.clone(),
                            base_revision: p.base_revision.clone(),
                            revision_id: p.revision_id.clone(),
                            proposer: p.proposer.clone(),
                            at: p.at.clone(),
                            reason: p.reason.clone(),
                            origin: p.origin.clone(),
                            supersedes: p.supersedes.clone(),
                            paths: visible_paths
                                .iter()
                                .take(8)
                                .map(|s| s.to_string())
                                .collect(),
                            path_count: u32::try_from(visible_paths.len()).unwrap_or(u32::MAX),
                        }
                    })
                    .collect::<Vec<_>>();
                let next_offset =
                    (visible.len() > request.limit).then_some(request.offset + request.limit);
                visible.truncate(request.limit);
                Ok(ProposalListResult {
                    proposals: visible,
                    next_offset,
                })
            },
        )
    }
}
