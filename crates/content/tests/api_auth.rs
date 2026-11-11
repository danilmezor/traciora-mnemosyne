use mnemosyne_content::{
    api::{Operation, Request},
    auth::{ActorKind, Credentials, Grant, Permission, Policy, TrustedIdentity},
    Change, Service,
};
use std::sync::atomic::{AtomicU64, Ordering};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(std::path::PathBuf);
impl Fixture {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "mnemosyne-auth-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )))
    }
    fn service(&self) -> Service {
        Service::open(&self.0).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn identity(tenant: &str, actor: &str, kind: ActorKind) -> TrustedIdentity {
    TrustedIdentity {
        tenant_id: tenant.into(),
        actor_id: actor.into(),
        kind,
        on_behalf_of: None,
    }
}
fn grant(who: &TrustedIdentity, permission: Permission, scope: &str) -> Grant {
    Grant {
        tenant_id: who.tenant_id.clone(),
        actor_id: who.actor_id.clone(),
        permission,
        scope: scope.into(),
    }
}
fn request(operation: Operation, key: Option<&str>) -> Request {
    Request {
        version: 1,
        idempotency_key: key.map(str::to_owned),
        operation,
    }
}

#[test]
fn identity_and_unknown_authority_cannot_enter_operation_json() {
    for json in [
        r#"{"version":1,"identity":{"tenant_id":"a","actor_id":"admin","kind":"human"},"operation":{"type":"head"}}"#,
        r#"{"version":1,"operation":{"type":"read","path":"docs/a","tenant_id":"other"}}"#,
        r#"{"version":1,"operation":{"type":"approve","proposal_id":"p1","digest":"d","actor":"admin"}}"#,
        r#"{"version":1,"operation":{"type":"bootstrap_policy","grants":[]}}"#,
    ] {
        assert!(serde_json::from_str::<Request>(json).is_err());
    }
}

#[test]
fn credentials_fail_closed_and_policy_scopes_have_segment_boundaries() {
    let credentials: Credentials = serde_json::from_str(r#"{"credentials":[{"token":"secret","identity":{"tenant_id":"a","actor_id":"reader","kind":"agent"}}]}"#).unwrap();
    assert!(credentials.authenticate("").is_err());
    assert!(credentials.authenticate("wrong").is_err());
    let who = credentials.authenticate("secret").unwrap();
    let policy = Policy {
        tenants: Default::default(),
        grants: vec![grant(&who, Permission::Read, "docs")],
    };
    assert!(policy.check(&who, Permission::Read, "docs/a").is_ok());
    for path in [
        "docs-private/a",
        "docs/../private",
        "/docs/a",
        "docs//a",
        "docs/line\nbreak",
        "",
    ] {
        assert!(policy.check(&who, Permission::Read, path).is_err());
    }
    assert!(policy
        .check(
            &identity("b", "reader", ActorKind::Agent),
            Permission::Read,
            "docs/a"
        )
        .is_err());
    assert!(policy.check(&who, Permission::Approve, "docs/a").is_err());
}

#[test]
fn api_denies_scoped_reads_history_diff_list_and_cross_tenant_proposals() {
    let f = Fixture::new();
    let s = f.service();
    let owner = identity("a", "owner", ActorKind::Human);
    let reader = identity("a", "reader", ActorKind::Agent);
    let foreign = identity("b", "reader", ActorKind::Agent);
    s.bootstrap_policy(Policy {
        tenants: Default::default(),
        grants: vec![
            grant(&owner, Permission::Read, ""),
            grant(&owner, Permission::Edit, ""),
            grant(&reader, Permission::Read, "public"),
        ],
    })
    .unwrap();
    let revision = s
        .edit(
            &owner,
            "r0",
            vec![
                Change::Put {
                    path: "public/a".into(),
                    content: "visible".into(),
                },
                Change::Put {
                    path: "private/a".into(),
                    content: "secret".into(),
                },
            ],
            "seed",
            None,
            None,
        )
        .unwrap();
    assert_eq!(
        serde_json::to_value(
            s.execute(
                &reader,
                request(
                    Operation::List {
                        scope: "public".into()
                    },
                    None
                )
            )
            .result
            .unwrap()
        )
        .unwrap(),
        serde_json::json!(["public/a"])
    );
    for operation in [
        Operation::Read {
            path: "private/a".into(),
            revision: None,
        },
        Operation::History {
            path: "private/a".into(),
        },
        Operation::List { scope: "".into() },
        Operation::Diff {
            revision_id: revision.id.clone(),
        },
    ] {
        let expected = if matches!(&operation, Operation::Diff { .. }) {
            "not_found"
        } else {
            "forbidden"
        };
        let response = s.execute(&reader, request(operation, None));
        assert!(response.result.is_none());
        assert_eq!(response.error.unwrap().code, expected);
    }
    for operation in [
        Operation::Read {
            path: "public/a".into(),
            revision: None,
        },
        Operation::Propose {
            base_revision: revision.id,
            changes: vec![Change::Put {
                path: "public/a".into(),
                content: "attack".into(),
            }],
            reason: None,
            origin: None,
        },
    ] {
        let response = s.execute(&foreign, request(operation, Some("cross")));
        assert!(response.result.is_none());
        assert_eq!(response.error.unwrap().code, "forbidden");
    }
}

#[test]
fn version_and_mutation_keys_are_structured_errors() {
    let f = Fixture::new();
    let s = f.service();
    let who = identity("a", "agent", ActorKind::Agent);
    let mut req = request(Operation::Head, None);
    req.version = 99;
    assert_eq!(
        s.execute(&who, req).error.unwrap().code,
        "unsupported_version"
    );
    let response = s.execute(
        &who,
        request(
            Operation::Propose {
                base_revision: "r0".into(),
                changes: vec![],
                reason: None,
                origin: None,
            },
            None,
        ),
    );
    assert_eq!(response.error.unwrap().code, "invalid_request");
}

#[test]
fn delegated_agent_uses_intersection_of_executor_and_requester_scope() {
    let mut agent = identity("tenant", "service-agent", ActorKind::Agent);
    agent.on_behalf_of = Some("requester".into());
    let requester = identity("tenant", "requester", ActorKind::Human);
    let policy = Policy {
        tenants: Default::default(),
        grants: vec![
            grant(&agent, Permission::Read, ""),
            grant(&agent, Permission::Propose, ""),
            grant(&requester, Permission::Read, "docs"),
            grant(&requester, Permission::Propose, "docs"),
        ],
    };
    assert!(policy.check(&agent, Permission::Read, "docs/a").is_ok());
    assert!(policy.check(&agent, Permission::Propose, "docs/a").is_ok());
    assert!(policy.check(&agent, Permission::Read, "private/a").is_err());
    assert!(policy.check(&agent, Permission::Approve, "docs/a").is_err());
    let mut revoked = policy.clone();
    revoked.grants.retain(|g| g.actor_id != "requester");
    assert!(revoked.check(&agent, Permission::Read, "docs/a").is_err());
    let mut human = requester;
    human.on_behalf_of = Some("service-agent".into());
    assert!(policy.check(&human, Permission::Read, "docs/a").is_err());
}

#[test]
fn public_policy_requires_root_human_admin_and_validates_replacement() {
    let f = Fixture::new();
    let s = f.service();
    let owner = identity("a", "owner", ActorKind::Human);
    let agent = identity("a", "agent", ActorKind::Agent);
    let foreign = identity("b", "owner", ActorKind::Human);
    let scoped = identity("a", "scoped", ActorKind::Human);
    let policy = Policy {
        tenants: Default::default(),
        grants: vec![
            grant(&owner, Permission::Admin, ""),
            grant(&agent, Permission::Admin, ""),
            grant(&scoped, Permission::Admin, "docs"),
            grant(&foreign, Permission::Admin, ""),
        ],
    };
    s.bootstrap_policy(policy).unwrap();
    let v2 = |operation| Request {
        version: 2,
        idempotency_key: None,
        operation,
    };
    let response = s.execute(&owner, v2(Operation::PolicyGet));
    let value = serde_json::to_value(response).unwrap();
    assert_eq!(
        value["result"]["policy"]["grants"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert!(s.execute(&agent, v2(Operation::PolicyGet)).error.is_some());
    assert!(s.execute(&scoped, v2(Operation::PolicyGet)).error.is_some());
    assert!(s
        .execute(
            &scoped,
            v2(Operation::PolicySet {
                policy: Policy::default(),
                expected_digest: String::new()
            })
        )
        .error
        .is_some());
    let mut delegated = owner.clone();
    delegated.on_behalf_of = Some("owner".into());
    assert!(s
        .execute(&delegated, v2(Operation::PolicyGet))
        .error
        .is_some());
    let digest = s.policy_state(&owner).unwrap().digest;
    for bad in [
        grant(&foreign, Permission::Read, ""),
        grant(&owner, Permission::Read, "../escape"),
    ] {
        assert!(s
            .execute(
                &owner,
                v2(Operation::PolicySet {
                    policy: Policy {
                        grants: vec![bad],
                        tenants: Default::default()
                    },
                    expected_digest: digest.clone()
                })
            )
            .error
            .is_some());
    }
    assert!(s
        .execute(&owner, request(Operation::PolicyGet, None))
        .error
        .is_some());
    assert!(s
        .execute(
            &owner,
            v2(Operation::PolicySet {
                policy: Policy::default(),
                expected_digest: digest.clone()
            })
        )
        .error
        .is_none());
    assert!(s.execute(&owner, v2(Operation::PolicyGet)).error.is_some());
    assert_eq!(
        s.execute(
            &foreign,
            v2(Operation::PolicySet {
                policy: Policy::default(),
                expected_digest: digest
            })
        )
        .error
        .unwrap()
        .code,
        "policy_conflict"
    );
    // Tenant replacement cannot erase another tenant's administration.
    assert!(s
        .execute(&foreign, v2(Operation::PolicyGet))
        .error
        .is_none());
}
