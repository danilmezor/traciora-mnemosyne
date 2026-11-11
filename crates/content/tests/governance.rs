use mnemosyne_content::{
    auth::{ActorKind, Grant, Permission, Policy, TrustedIdentity},
    Change, Service,
};
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
fn identity(actor: &str, kind: ActorKind) -> TrustedIdentity {
    TrustedIdentity {
        tenant_id: "t1".into(),
        actor_id: actor.into(),
        kind,
        on_behalf_of: None,
    }
}
fn policy() -> Policy {
    let mut grants = vec![];
    for (actor, permissions) in [
        (
            "human",
            vec![
                Permission::Read,
                Permission::Edit,
                Permission::Approve,
                Permission::Admin,
            ],
        ),
        (
            "agent",
            vec![
                Permission::Read,
                Permission::Propose,
                Permission::Edit,
                Permission::Approve,
            ],
        ),
        ("scoped", vec![Permission::Read, Permission::Propose]),
    ] {
        for permission in permissions {
            grants.push(Grant {
                tenant_id: "t1".into(),
                actor_id: actor.into(),
                permission,
                scope: if actor == "scoped" { "public" } else { "" }.into(),
            })
        }
    }
    Policy {
        grants,
        tenants: Default::default(),
    }
}
struct Fixture {
    root: PathBuf,
    service: Service,
    human: TrustedIdentity,
    agent: TrustedIdentity,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "mnemosyne-governance-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let service = Service::open(&root).unwrap();
        service.bootstrap_policy(policy()).unwrap();
        Self {
            root,
            service,
            human: identity("human", ActorKind::Human),
            agent: identity("agent", ActorKind::Agent),
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
fn put(path: &str, content: &str) -> Change {
    Change::Put {
        path: path.into(),
        content: content.into(),
    }
}
#[test]
fn durable_exact_approval_atomic_publication_and_retry() {
    let f = Fixture::new();
    let p = f
        .service
        .propose(
            &f.agent,
            "r0",
            vec![put("public/a.md", "one"), put("private/b.md", "two")],
            "propose",
            None,
            None,
        )
        .unwrap();
    assert_eq!(f.service.head(&f.human).unwrap(), "r0");
    assert_eq!(
        f.service
            .apply(&f.agent, &p.id, &p.digest, "too-early")
            .unwrap_err()
            .code,
        "approval_required"
    );
    assert_eq!(
        f.service
            .approve(&f.agent, &p.id, &p.digest, "agent-approve", None)
            .unwrap_err()
            .code,
        "human_required"
    );
    assert_eq!(
        f.service
            .edit(
                &f.agent,
                "r0",
                vec![put("bypass", "bad")],
                "bypass",
                None,
                None
            )
            .unwrap_err()
            .code,
        "human_required"
    );
    assert_eq!(
        f.service
            .approve(&f.human, &p.id, "altered", "altered", None)
            .unwrap_err()
            .code,
        "proposal_mismatch"
    );
    let reopened = Service::open(&f.root).unwrap();
    assert_eq!(reopened.review(&f.human, &p.id).unwrap().diff.len(), 2);
    reopened
        .approve(&f.human, &p.id, &p.digest, "approve", None)
        .unwrap();
    let reopened = Service::open(&f.root).unwrap();
    let revision = reopened.apply(&f.agent, &p.id, &p.digest, "apply").unwrap();
    let again = Service::open(&f.root)
        .unwrap()
        .apply(&f.agent, &p.id, &p.digest, "apply")
        .unwrap();
    assert_eq!(revision.id, again.id);
    assert_eq!(
        reopened
            .apply(&f.agent, &p.id, &p.digest, "another-apply")
            .unwrap()
            .id,
        revision.id
    );
    assert_eq!(
        reopened
            .read(&f.human, "public/a.md", None)
            .unwrap()
            .content,
        "one"
    );
    assert_eq!(
        reopened
            .read(&f.human, "private/b.md", None)
            .unwrap()
            .revision_id,
        revision.id
    );
    assert_eq!(reopened.history(&f.human, "public/a.md").unwrap().len(), 1);
    let db = rusqlite::Connection::open(f.root.join("authority.sqlite3")).unwrap();
    let count: i64 = db
        .query_row(
            "SELECT count(*) FROM revisions WHERE tenant='t1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
    let persisted: String = db
        .query_row(
            "SELECT data FROM proposals WHERE tenant='t1' AND id=?1",
            [&p.id],
            |r| r.get(0),
        )
        .unwrap();
    let state: serde_json::Value = serde_json::from_str(&persisted).unwrap();
    assert_eq!(state["state"], "applied");
}
#[test]
fn rejected_stale_changed_permission_and_changed_payload_never_publish() {
    let f = Fixture::new();
    let p = f
        .service
        .propose(&f.agent, "r0", vec![put("a", "pending")], "p", None, None)
        .unwrap();
    assert_eq!(
        f.service
            .propose(&f.agent, "r0", vec![put("a", "different")], "p", None, None)
            .unwrap_err()
            .code,
        "idempotency_conflict"
    );
    f.service
        .reject(&f.human, &p.id, &p.digest, "reject", None)
        .unwrap();
    assert_eq!(
        f.service
            .apply(&f.agent, &p.id, &p.digest, "apply-rejected")
            .unwrap_err()
            .code,
        "approval_required"
    );
    assert_eq!(f.service.head(&f.human).unwrap(), "r0");
    let p = f
        .service
        .propose(&f.agent, "r0", vec![put("a", "stale")], "p2", None, None)
        .unwrap();
    f.service
        .approve(&f.human, &p.id, &p.digest, "approve", None)
        .unwrap();
    let r = f
        .service
        .edit(&f.human, "r0", vec![put("a", "human")], "human", None, None)
        .unwrap();
    assert_eq!(
        f.service
            .apply(&f.agent, &p.id, &p.digest, "stale")
            .unwrap_err()
            .code,
        "stale_base"
    );
    let p = f
        .service
        .propose(&f.agent, &r.id, vec![put("a", "revoked")], "p3", None, None)
        .unwrap();
    f.service
        .approve(&f.human, &p.id, &p.digest, "a3", None)
        .unwrap();
    let mut changed = policy();
    changed
        .grants
        .retain(|g| g.permission != Permission::Approve);
    f.service.set_policy(&f.human, changed).unwrap();
    assert_eq!(
        f.service
            .apply(&f.agent, &p.id, &p.digest, "revoked")
            .unwrap_err()
            .code,
        "approval_required"
    );
    f.service.set_policy(&f.human, policy()).unwrap();
    assert_eq!(
        f.service
            .apply(&f.agent, &p.id, &p.digest, "restored")
            .unwrap_err()
            .code,
        "approval_required"
    );
    assert_eq!(
        f.service.read(&f.human, "a", None).unwrap().content,
        "human"
    );
}
#[test]
fn scopes_tenants_history_and_diffs_do_not_leak() {
    let f = Fixture::new();
    let r = f
        .service
        .edit(
            &f.human,
            "r0",
            vec![put("public/a", "public"), put("private/b", "secret")],
            "seed",
            None,
            None,
        )
        .unwrap();
    let scoped = identity("scoped", ActorKind::Agent);
    assert_eq!(
        f.service.read(&scoped, "private/b", None).unwrap_err().code,
        "forbidden"
    );
    assert_eq!(
        f.service
            .read(&scoped, "private/b", Some(&r.id))
            .unwrap_err()
            .code,
        "forbidden"
    );
    assert_eq!(
        f.service.history(&scoped, "private/b").unwrap_err().code,
        "forbidden"
    );
    assert_eq!(
        f.service.diff(&scoped, &r.id).unwrap_err().code,
        "not_found"
    );
    assert_eq!(r.id.len(), 65);
    assert_eq!(
        f.service
            .diff(&scoped, "unknown-revision")
            .unwrap_err()
            .code,
        "not_found"
    );
    let hidden = f
        .service
        .propose(
            &f.agent,
            &r.id,
            vec![put("private/b", "pending secret")],
            "hidden",
            None,
            None,
        )
        .unwrap();
    assert_eq!(
        f.service.review(&scoped, &hidden.id).unwrap_err().code,
        "not_found"
    );
    assert_eq!(
        f.service
            .review(&scoped, "unknown-proposal")
            .unwrap_err()
            .code,
        "not_found"
    );
    let history = f.service.history(&scoped, "public/a").unwrap();
    assert_eq!(history[0].changes.len(), 1);
    assert!(!serde_json::to_string(&history).unwrap().contains("secret"));
    assert_eq!(f.service.list(&scoped, "public").unwrap(), vec!["public/a"]);
    assert_eq!(
        f.service
            .propose(
                &scoped,
                &r.id,
                vec![put("public/a", "allowed"), put("private/b", "denied")],
                "mixed",
                None,
                None
            )
            .unwrap_err()
            .code,
        "forbidden"
    );
    let outsider = TrustedIdentity {
        tenant_id: "t2".into(),
        ..f.human.clone()
    };
    assert_eq!(
        f.service
            .read(&outsider, "public/a", None)
            .unwrap_err()
            .code,
        "forbidden"
    );
    assert_eq!(
        f.service.diff(&outsider, &r.id).unwrap_err().code,
        "forbidden"
    );
    assert_eq!(f.service.head(&f.human).unwrap(), r.id);
}
#[test]
fn concurrent_cas_and_abandoned_prepublication_file() {
    let f = Fixture::new();
    std::fs::write(
        f.root.join("authority.next"),
        b"partial abandoned transaction",
    )
    .unwrap();
    let root = f.root.clone();
    let who = f.human.clone();
    let first = std::thread::spawn(move || {
        Service::open(root)
            .unwrap()
            .edit(&who, "r0", vec![put("a", "first")], "first", None, None)
    });
    let root = f.root.clone();
    let who = f.human.clone();
    let second = std::thread::spawn(move || {
        Service::open(root).unwrap().edit(
            &who,
            "r0",
            vec![put("a", "second"), put("b", "second")],
            "second",
            None,
            None,
        )
    });
    let a = first.join().unwrap();
    let b = second.join().unwrap();
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    assert_eq!(a.err().or_else(|| b.err()).unwrap().code, "stale_base");
    let reopened = Service::open(&f.root).unwrap();
    let content = reopened.read(&f.human, "a", None).unwrap();
    assert!(content.content == "first" || content.content == "second");
    let list = reopened.list(&f.human, "").unwrap();
    assert_eq!(list.len(), if content.content == "first" { 1 } else { 2 });
}

#[test]
fn policy_admin_is_tenant_scoped_and_review_audit_survives_invalidation() {
    let f = Fixture::new();
    let p = f
        .service
        .propose(&f.agent, "r0", vec![put("a", "reviewed")], "p", None, None)
        .unwrap();
    f.service
        .approve(&f.human, &p.id, &p.digest, "approval", None)
        .unwrap();
    let foreign = Policy {
        tenants: Default::default(),
        grants: vec![Grant {
            tenant_id: "t2".into(),
            actor_id: "human".into(),
            permission: Permission::Admin,
            scope: "".into(),
        }],
    };
    assert_eq!(
        f.service.set_policy(&f.human, foreign).unwrap_err().code,
        "forbidden"
    );
    f.service.set_policy(&f.human, policy()).unwrap();
    let reviewed = f.service.review(&f.human, &p.id).unwrap();
    assert_eq!(reviewed.state, mnemosyne_content::ProposalState::Pending);
    assert_eq!(reviewed.decisions.len(), 1);
    assert_eq!(reviewed.decisions[0].actor.actor_id, "human");
    assert!(reviewed.decisions[0].approved);
    assert!(!reviewed.decisions[0].policy_digest.is_empty());
}

#[test]
fn delegated_requester_is_audited_and_replay_is_identity_bound() {
    let f = Fixture::new();
    let mut configured = policy();
    for actor in ["requester-one", "requester-two"] {
        for permission in [Permission::Read, Permission::Propose] {
            configured.grants.push(Grant {
                tenant_id: "t1".into(),
                actor_id: actor.into(),
                permission,
                scope: "docs".into(),
            });
        }
    }
    f.service.set_policy(&f.human, configured).unwrap();
    let first = TrustedIdentity {
        on_behalf_of: Some("requester-one".into()),
        ..f.agent.clone()
    };
    let second = TrustedIdentity {
        on_behalf_of: Some("requester-two".into()),
        ..f.agent.clone()
    };
    let p1 = f
        .service
        .propose(
            &first,
            "r0",
            vec![put("docs/a", "one")],
            "same-key",
            None,
            None,
        )
        .unwrap();
    let p2 = f
        .service
        .propose(
            &second,
            "r0",
            vec![put("docs/a", "two")],
            "same-key",
            None,
            None,
        )
        .unwrap();
    assert_ne!(p1.id, p2.id);
    assert_eq!(p1.proposer.on_behalf_of.as_deref(), Some("requester-one"));
    f.service
        .approve(&f.human, &p1.id, &p1.digest, "approved", None)
        .unwrap();
    let r = f
        .service
        .apply(&first, &p1.id, &p1.digest, "apply")
        .unwrap();
    assert_eq!(r.actor.on_behalf_of.as_deref(), Some("requester-one"));
}

// ---- content-grant ----

fn grant(actor: &str, permission: Permission, scope: &str) -> Grant {
    Grant {
        tenant_id: "t1".into(),
        actor_id: actor.into(),
        permission,
        scope: scope.into(),
    }
}

#[test]
fn add_grant_is_idempotent_and_validated() {
    use mnemosyne_content::grant::{add_grant, ServeLock};
    let f = Fixture::new();
    let p = f
        .service
        .propose(&f.agent, "r0", vec![put("a", "reviewed")], "p", None, None)
        .unwrap();
    f.service
        .approve(&f.human, &p.id, &p.digest, "approval", None)
        .unwrap();
    let first = add_grant(
        &f.root,
        grant("host-admin", Permission::Edit, ""),
        false,
    )
    .unwrap();
    assert!(first.added);
    assert_eq!(
        (
            first.tenant_id.as_str(),
            first.actor_id.as_str(),
            first.scope.as_str()
        ),
        ("t1", "host-admin", "")
    );
    assert_eq!(first.permission, Permission::Edit);
    let state = f.service.policy_state(&f.human).unwrap();
    assert_eq!(first.policy_digest, state.digest);
    assert!(state
        .policy
        .grants
        .iter()
        .any(|g| g.actor_id == "host-admin"
            && g.permission == Permission::Edit
            && g.scope.is_empty()));
    // The added grant returned the approval to pending, as policy_set does.
    let reviewed = f.service.review(&f.human, &p.id).unwrap();
    assert_eq!(reviewed.state, mnemosyne_content::ProposalState::Pending);
    // Approve again: an identical grant changes nothing, the approval included.
    let p = f.service.review(&f.human, &p.id).unwrap();
    f.service
        .approve(&f.human, &p.id, &p.digest, "approval-2", None)
        .unwrap();
    let again = add_grant(
        &f.root,
        grant("host-admin", Permission::Edit, ""),
        false,
    )
    .unwrap();
    assert!(!again.added);
    assert_eq!(again.policy_digest, first.policy_digest);
    assert_eq!(
        f.service.review(&f.human, &p.id).unwrap().state,
        mnemosyne_content::ProposalState::Approved
    );
    assert_eq!(
        add_grant(&f.root, grant("x", Permission::Read, "../x"), false)
            .unwrap_err()
            .code,
        "invalid_policy"
    );
    assert_eq!(
        add_grant(&f.root, grant("", Permission::Read, ""), false)
            .unwrap_err()
            .code,
        "invalid_policy"
    );
    // Uninitialised: an empty store directory, and none at all (not created).
    let empty = f.root.with_extension("uninitialised");
    Service::open(&empty).unwrap();
    assert_eq!(
        add_grant(&empty, grant("a", Permission::Read, ""), false)
            .unwrap_err()
            .code,
        "not_initialized"
    );
    let _ = std::fs::remove_dir_all(&empty);
    assert_eq!(
        add_grant(&empty, grant("a", Permission::Read, ""), false)
            .unwrap_err()
            .code,
        "not_initialized"
    );
    assert!(!empty.exists());
    // A held serve lock refuses unless while_serving.
    let lock = ServeLock::try_acquire(&f.root).unwrap().expect("free");
    assert!(ServeLock::try_acquire(&f.root).unwrap().is_none());
    let err = add_grant(&f.root, grant("late", Permission::Read, ""), false).unwrap_err();
    assert_eq!(err.code, "store_in_use");
    assert!(
        err.message.contains(&std::process::id().to_string()),
        "{}",
        err.message
    );
    assert!(
        add_grant(&f.root, grant("late", Permission::Read, ""), true)
            .unwrap()
            .added
    );
    drop(lock);
    assert!(ServeLock::try_acquire(&f.root).unwrap().is_some());
    // The report is its own API result.
    let response = mnemosyne_content::api::Response {
        version: 2,
        result: Some(mnemosyne_content::api::ApiResult::Grant(again)),
        error: None,
    };
    let back: mnemosyne_content::api::Response =
        serde_json::from_str(&serde_json::to_string(&response).unwrap()).unwrap();
    assert!(matches!(
        back.result,
        Some(mnemosyne_content::api::ApiResult::Grant(_))
    ));
}
