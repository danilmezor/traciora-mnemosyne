use mnemosyne_content::{
    auth::{ActorKind, Grant, Permission, Policy, TrustedIdentity},
    backup,
    retention::{self, RetentionPolicy},
    Change, Service,
};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "mnemosyne-retention-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&p).unwrap();
        Self(p)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn who(tenant: &str) -> TrustedIdentity {
    TrustedIdentity {
        tenant_id: tenant.into(),
        actor_id: "human".into(),
        kind: ActorKind::Human,
        on_behalf_of: None,
    }
}
fn setup(f: &Fixture) -> Service {
    let s = Service::open(f.0.join("source")).unwrap();
    s.bootstrap_policy(Policy {
        tenants: Default::default(),
        grants: ["a", "b"]
            .into_iter()
            .flat_map(|tenant| {
                [
                    Permission::Read,
                    Permission::Edit,
                    Permission::Propose,
                    Permission::Approve,
                    Permission::Admin,
                ]
                .into_iter()
                .map(move |permission| Grant {
                    tenant_id: tenant.into(),
                    actor_id: "human".into(),
                    permission,
                    scope: "".into(),
                })
            })
            .collect(),
    })
    .unwrap();
    s
}
fn policy(expire: bool) -> RetentionPolicy {
    RetentionPolicy {
        tenant_id: "a".into(),
        keep_revisions: 1,
        expire_pruned_edit_replays: expire,
    }
}
fn changes(index: usize) -> Vec<Change> {
    vec![Change::Put {
        path: "note.md".into(),
        content: format!("version {index} ").repeat(1000),
    }]
}
fn versions(s: &Service) -> Vec<mnemosyne_content::Revision> {
    let mut base = "r0".to_string();
    let mut revisions = vec![];
    for index in 0..5 {
        let r = s
            .edit(
                &who("a"),
                &base,
                changes(index),
                &format!("edit-{index}"),
                None,
                None,
            )
            .unwrap();
        base = r.id.clone();
        revisions.push(r);
    }
    revisions
}
#[test]
fn explicit_expiry_reclaims_history_preserving_original_diff_retries_and_backup() {
    let f = Fixture::new();
    let s = setup(&f);
    let revs = versions(&s);
    let root = f.0.join("source");
    let before = fs::read(root.join("authority.sqlite3")).unwrap();
    let keep_replays = retention::dry_run(&root, policy(false)).unwrap();
    assert_eq!(keep_replays.orphan_blob_bytes, 0);
    assert_eq!(keep_replays.expired_replay_count, 0);
    assert_eq!(fs::read(root.join("authority.sqlite3")).unwrap(), before);
    let plan = retention::dry_run(&root, policy(true)).unwrap();
    assert_eq!(plan.pruned_revision_ids.len(), 4);
    assert_eq!(plan.expired_replay_count, 4);
    assert!(plan.orphan_blob_bytes > 0);
    let removed_blob = format!(
        "{:x}",
        Sha256::digest(format!("version {} ", 1).repeat(1000).as_bytes())
    );
    assert!(root.join("blobs").join(&removed_blob).exists());
    let report = retention::apply(&root, policy(true), &plan.plan_digest).unwrap();
    let database = rusqlite::Connection::open(root.join("authority.sqlite3")).unwrap();
    assert_eq!(
        database
            .query_row("SELECT value FROM meta WHERE key='version'", [], |r| r
                .get::<_, String>(
                0
            ))
            .unwrap(),
        "4"
    );
    assert!(report.applied);
    assert_eq!(report.deleted_blob_bytes, plan.orphan_blob_bytes);
    assert!(!root.join("blobs").join(&removed_blob).exists());
    assert_eq!(
        s.read(&who("a"), "note.md", None).unwrap().content,
        format!("version {} ", 4).repeat(1000)
    );
    assert_eq!(
        s.read(&who("a"), "note.md", Some(&revs[1].id))
            .unwrap_err()
            .code,
        "history_pruned"
    );
    assert_eq!(
        s.diff(&who("a"), &revs[1].id).unwrap_err().code,
        "history_pruned"
    );
    assert_eq!(s.history(&who("a"), "note.md").unwrap().len(), 1);
    assert_eq!(
        serde_json::to_value(s.diff(&who("a"), &revs[4].id).unwrap()).unwrap(),
        serde_json::to_value(&revs[4].changes).unwrap()
    );
    assert_eq!(
        s.edit(&who("a"), &revs[0].id, changes(1), "edit-1", None, None)
            .unwrap_err()
            .code,
        "replay_expired"
    );
    assert_eq!(
        s.edit(&who("a"), &revs[0].id, changes(99), "edit-1", None, None)
            .unwrap_err()
            .code,
        "idempotency_conflict"
    );
    assert_eq!(
        s.edit(&who("a"), &revs[3].id, changes(4), "edit-4", None, None)
            .unwrap()
            .id,
        revs[4].id
    );
    backup::create(&root, f.0.join("backup")).unwrap();
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(f.0.join("backup/manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["authority_version"], 4);
    backup::restore(f.0.join("backup"), f.0.join("restored")).unwrap();
    let restored = Service::open(f.0.join("restored")).unwrap();
    assert_eq!(
        restored.read(&who("a"), "note.md", None).unwrap().content,
        s.read(&who("a"), "note.md", None).unwrap().content
    );
    assert_eq!(
        restored.diff(&who("a"), &revs[1].id).unwrap_err().code,
        "history_pruned"
    );
    let next = s
        .edit(
            &who("a"),
            &revs[4].id,
            changes(5),
            "after-prune",
            None,
            None,
        )
        .unwrap();
    assert_eq!(
        database
            .query_row("SELECT value FROM meta WHERE key='version'", [], |r| r
                .get::<_, String>(
                0
            ))
            .unwrap(),
        "4"
    );
    assert_eq!(s.history(&who("a"), "note.md").unwrap().len(), 2);
    assert_eq!(
        s.read(&who("a"), "note.md", Some(&next.id))
            .unwrap()
            .content,
        format!("version {} ", 5).repeat(1000)
    );
}
#[test]
fn proposal_pins_detached_manifests_and_other_tenant_objects_survive() {
    let f = Fixture::new();
    let s = setup(&f);
    let root = f.0.join("source");
    let first = s
        .edit(&who("a"), "r0", changes(0), "first", None, None)
        .unwrap();
    let second = s
        .edit(&who("a"), &first.id, changes(1), "second", None, None)
        .unwrap();
    let proposal = s
        .propose(
            &who("a"),
            &second.id,
            vec![Change::Restore {
                path: "note.md".into(),
                revision_id: first.id.clone(),
            }],
            "restore-proposal",
            None,
            None,
        )
        .unwrap();
    let mut head = second.id.clone();
    for index in 2..5 {
        head = s
            .edit(
                &who("a"),
                &head,
                changes(index),
                &format!("later-{index}"),
                None,
                None,
            )
            .unwrap()
            .id;
    }
    let other = s
        .edit(
            &who("b"),
            "r0",
            vec![Change::Put {
                path: "private.md".into(),
                content: "other tenant".repeat(500),
            }],
            "other",
            None,
            None,
        )
        .unwrap();
    let plan = retention::dry_run(&root, policy(true)).unwrap();
    assert!(plan.pinned_revision_ids.contains(&first.id));
    assert!(plan.pinned_revision_ids.contains(&second.id));
    retention::apply(&root, policy(true), &plan.plan_digest).unwrap();
    assert_eq!(
        s.read(&who("a"), "note.md", Some(&second.id))
            .unwrap()
            .content,
        format!("version {} ", 1).repeat(1000)
    );
    assert_eq!(
        s.read(&who("a"), "note.md", Some(&first.id))
            .unwrap()
            .content,
        format!("version {} ", 0).repeat(1000)
    );
    assert_eq!(
        s.review(&who("a"), &proposal.id).unwrap().digest,
        proposal.digest
    );
    assert_eq!(s.history(&who("a"), "note.md").unwrap().len(), 1);
    assert_eq!(
        s.read(&who("b"), "private.md", Some(&other.id))
            .unwrap()
            .content,
        "other tenant".repeat(500)
    );
    backup::create(&root, f.0.join("backup")).unwrap();
}
#[test]
fn stale_plan_rejects_and_later_expiry_collects_already_pruned_replays() {
    let f = Fixture::new();
    let s = setup(&f);
    let root = f.0.join("source");
    let revs = versions(&s);
    let stale = retention::dry_run(&root, policy(true)).unwrap();
    s.edit(
        &who("b"),
        "r0",
        vec![Change::Put {
            path: "b.md".into(),
            content: "concurrent write".into(),
        }],
        "other",
        None,
        None,
    )
    .unwrap();
    assert_eq!(
        retention::apply(&root, policy(true), &stale.plan_digest)
            .unwrap_err()
            .code,
        "retention_plan_conflict"
    );
    let plan = retention::dry_run(&root, policy(false)).unwrap();
    retention::apply(&root, policy(false), &plan.plan_digest).unwrap();
    assert_eq!(
        s.edit(&who("a"), &revs[0].id, changes(1), "edit-1", None, None)
            .unwrap()
            .id,
        revs[1].id
    );
    let expire = retention::dry_run(&root, policy(true)).unwrap();
    assert!(expire.pruned_revision_ids.is_empty());
    assert_eq!(expire.expired_replay_count, 4);
    assert!(expire.orphan_blob_bytes > 0);
    retention::apply(&root, policy(true), &expire.plan_digest).unwrap();
    assert_eq!(
        s.edit(&who("a"), &revs[0].id, changes(1), "edit-1", None, None)
            .unwrap_err()
            .code,
        "replay_expired"
    );
}
