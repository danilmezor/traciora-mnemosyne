//! Host authentication and durable, current resource policy.
//! Credential files and process environments belong to the trusted service host;
//! this local transport does not isolate hostile processes running as that OS user.
use crate::ContentError;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorKind {
    Human,
    Agent,
}

/// Deserialize exists for durable records and operator configuration, never requests.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustedIdentity {
    pub tenant_id: String,
    pub actor_id: String,
    pub kind: ActorKind,
    /// Trusted initiating human; agent authority is intersected with this actor's grants.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_behalf_of: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    Read,
    Propose,
    Edit,
    Approve,
    Admin,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    pub tenant_id: String,
    pub actor_id: String,
    pub permission: Permission,
    /// Empty means the tenant root; otherwise a canonical directory or file path.
    pub scope: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub grants: Vec<Grant>,
    #[serde(default)]
    pub tenants: BTreeMap<String, TenantPolicy>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Member {
    pub role: String,
    pub kind: ActorKind,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rungs {
    pub propose_from: String,
    pub publish_from: String,
    pub visibility_from: String,
}

/// Per-tenant rank ladder, members and the rungs gating actions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TenantPolicy {
    /// Lowest first.
    pub roles: Vec<String>,
    #[serde(default)]
    pub members: BTreeMap<String, Member>,
    pub rungs: Rungs,
    pub root_seen_from: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub colour_seed: Option<u64>,
    /// Whether an agent acting on behalf of a human gets the `above_rank` names that
    /// human would get. Omitted while false, so the stored policy and
    /// its digest are unchanged; an agent alone never gets them; humans are unaffected.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub agent_sees_above_rank: bool,
}

impl Default for TenantPolicy {
    fn default() -> Self {
        Self {
            roles: ["worker", "junior", "middle", "senior", "administrator"]
                .map(String::from)
                .to_vec(),
            members: BTreeMap::new(),
            rungs: Rungs {
                propose_from: "middle".into(),
                publish_from: "senior".into(),
                visibility_from: "senior".into(),
            },
            root_seen_from: "junior".into(),
            colour_seed: None,
            agent_sees_above_rank: false,
        }
    }
}

impl TenantPolicy {
    pub fn rank(&self, role: &str) -> Option<usize> {
        self.roles.iter().position(|r| r == role)
    }
    pub fn member_role(&self, actor_id: &str) -> Option<&Member> {
        self.members.get(actor_id)
    }
}

pub fn valid_path(path: &str, allow_root: bool) -> bool {
    (allow_root && path.is_empty())
        || (!path.is_empty()
            && !path.contains('\\')
            && path.len() <= 1024
            && !path.chars().any(char::is_control)
            && !path.starts_with('/')
            && path
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != ".."))
}

impl Policy {
    pub fn check(
        &self,
        identity: &TrustedIdentity,
        permission: Permission,
        path: &str,
    ) -> Result<(), ContentError> {
        if !valid_path(path, true) || identity.tenant_id.is_empty() || identity.actor_id.is_empty()
        {
            return Err(ContentError::new("forbidden", "Access denied"));
        }
        let granted = |actor_id: &str| {
            self.grants.iter().any(|grant| {
                grant.tenant_id == identity.tenant_id
                    && grant.actor_id == actor_id
                    && grant.permission == permission
                    && valid_path(&grant.scope, true)
                    && (grant.scope.is_empty()
                        || path == grant.scope
                        || path
                            .strip_prefix(&grant.scope)
                            .is_some_and(|suffix| suffix.starts_with('/')))
            })
        };
        let delegated = match (&identity.kind, &identity.on_behalf_of) {
            (ActorKind::Agent, Some(actor)) => {
                !actor.is_empty() && actor != &identity.actor_id && granted(actor)
            }
            (_, None) => true,
            (ActorKind::Human, Some(_)) => false,
        };
        if granted(&identity.actor_id) && delegated {
            Ok(())
        } else {
            Err(ContentError::new("forbidden", "Access denied"))
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credential {
    pub token: String,
    pub identity: TrustedIdentity,
}

/// Read only from a host-controlled file; never passed as tool/request arguments.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credentials {
    pub credentials: Vec<Credential>,
}
impl Credentials {
    pub fn authenticate(&self, bearer: &str) -> Result<TrustedIdentity, ContentError> {
        if bearer.is_empty() {
            return Err(ContentError::new(
                "unauthenticated",
                "Authentication required",
            ));
        }
        let matches: Vec<_> = self
            .credentials
            .iter()
            .filter(|c| !c.token.is_empty() && c.token == bearer)
            .collect();
        if matches.len() != 1 {
            return Err(ContentError::new(
                "unauthenticated",
                "Authentication required",
            ));
        }
        let identity = &matches[0].identity;
        if identity.tenant_id.is_empty() || identity.actor_id.is_empty() {
            return Err(ContentError::new(
                "unauthenticated",
                "Authentication required",
            ));
        }
        Ok(identity.clone())
    }
}
