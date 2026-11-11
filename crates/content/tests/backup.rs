use mnemosyne_content::{
    auth::{ActorKind, Grant, Permission, Policy, TrustedIdentity},
    backup, Change, Service,
};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};
static NEXT: AtomicUsize = AtomicUsize::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "mnemosyne-backup-{}-{}",
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
fn initialized(f: &Fixture) -> (Service, TrustedIdentity) {
    let s = Service::open(f.0.join("source")).unwrap();
    let h = TrustedIdentity {
        tenant_id: "t".into(),
        actor_id: "h".into(),
        kind: ActorKind::Human,
        on_behalf_of: None,
    };
    s.bootstrap_policy(Policy {
        tenants: Default::default(),
        grants: ["h", "reviewer"]
            .into_iter()
            .flat_map(|actor| {
                [
                    Permission::Read,
                    Permission::Edit,
                    Permission::Propose,
                    Permission::Approve,
                    Permission::Admin,
                ]
                .into_iter()
                .map(move |permission| Grant {
                    tenant_id: "t".into(),
                    actor_id: actor.into(),
                    permission,
                    scope: "".into(),
                })
            })
            .collect(),
    })
    .unwrap();
    (s, h)
}
#[test]
fn backup_preserves_history_pending_approval_replay_binary_and_policy_without_caches() {
    let f = Fixture::new();
    let (s, h) = initialized(&f);
    let changes = vec![
        Change::Put {
            path: "a.md".into(),
            content: "old body".repeat(300),
        },
        Change::PutBinary {
            path: "file.bin".into(),
            bytes: vec![0, 255, 1],
        },
    ];
    let p = s
        .propose(&h, "r0", changes.clone(), "first", None, None)
        .unwrap();
    s.approve(
        &TrustedIdentity {
            actor_id: "reviewer".into(),
            ..h.clone()
        },
        &p.id,
        &p.digest,
        "approval",
        None,
    )
    .unwrap();
    let revision = s.apply(&h, &p.id, &p.digest, "apply").unwrap();
    let pending = s
        .propose(
            &h,
            &revision.id,
            vec![Change::Put {
                path: "a.md".into(),
                content: "new body".repeat(300),
            }],
            "pending",
            None,
            None,
        )
        .unwrap();
    s.approve(
        &TrustedIdentity {
            actor_id: "reviewer".into(),
            ..h.clone()
        },
        &pending.id,
        &pending.digest,
        "pending-approval",
        None,
    )
    .unwrap();
    for name in [
        "auth.json",
        "extraction.sqlite3",
        "semantic.sqlite3",
        "authority.json",
    ] {
        fs::write(f.0.join("source").join(name), "secret or disposable").unwrap();
    }
    fs::create_dir(f.0.join("source/search-cache")).unwrap();
    let report = backup::create(f.0.join("source"), f.0.join("backup")).unwrap();
    assert!(report.blob_count >= 3);
    let result = backup::restore(f.0.join("backup"), f.0.join("restored")).unwrap();
    assert_eq!(report.database_sha256, result.database_sha256);
    assert_eq!(fs::read_dir(f.0.join("restored")).unwrap().count(), 2);
    let restored = Service::open(f.0.join("restored")).unwrap();
    assert_eq!(restored.head(&h).unwrap(), revision.id);
    assert_eq!(
        restored
            .propose(&h, "r0", changes, "first", None, None)
            .unwrap()
            .id,
        p.id
    );
    assert_eq!(
        restored.apply(&h, &p.id, &p.digest, "apply").unwrap().id,
        revision.id
    );
    assert_eq!(
        restored
            .read_binary(&h, "file.bin", None, false, None)
            .unwrap()
            .bytes,
        vec![0, 255, 1]
    );
    assert_eq!(
        restored.history(&h, "a.md").unwrap().len(),
        s.history(&h, "a.md").unwrap().len()
    );
    restored
        .apply(&h, &pending.id, &pending.digest, "pending-apply")
        .unwrap();
    assert_eq!(
        restored
            .read(&h, "a.md", Some(&revision.id))
            .unwrap()
            .content,
        "old body".repeat(300)
    );
    let denied = TrustedIdentity {
        actor_id: "other".into(),
        ..h
    };
    assert!(restored.head(&denied).is_err());
}
#[test]
fn corrupt_missing_and_traversal_backups_cannot_publish_or_overwrite() {
    let f = Fixture::new();
    let (s, h) = initialized(&f);
    let p = s
        .propose(
            &h,
            "r0",
            vec![Change::Put {
                path: "a.md".into(),
                content: "body".into(),
            }],
            "p",
            None,
            None,
        )
        .unwrap();
    s.approve(
        &TrustedIdentity {
            actor_id: "reviewer".into(),
            ..h.clone()
        },
        &p.id,
        &p.digest,
        "approve",
        None,
    )
    .unwrap();
    s.apply(&h, &p.id, &p.digest, "apply").unwrap();
    backup::create(f.0.join("source"), f.0.join("backup")).unwrap();
    fs::create_dir(f.0.join("occupied")).unwrap();
    fs::write(f.0.join("occupied/sentinel"), "keep").unwrap();
    assert_eq!(
        backup::restore(f.0.join("backup"), f.0.join("occupied"))
            .unwrap_err()
            .code,
        "destination_not_empty"
    );
    assert_eq!(
        fs::read_to_string(f.0.join("occupied/sentinel")).unwrap(),
        "keep"
    );
    assert!(backup::create(f.0.join("source"), f.0.join("source/nested")).is_err());
    assert!(backup::restore(f.0.join("backup"), f.0.join("backup/nested")).is_err());
    let manifest_path = f.0.join("backup/manifest.json");
    let original = fs::read(&manifest_path).unwrap();
    let mut m: serde_json::Value = serde_json::from_slice(&original).unwrap();
    m["blobs"]["../../escape"] = 0.into();
    fs::write(&manifest_path, serde_json::to_vec(&m).unwrap()).unwrap();
    assert_eq!(
        backup::restore(f.0.join("backup"), f.0.join("invalid"))
            .unwrap_err()
            .code,
        "invalid_blob_id"
    );
    assert!(!f.0.join("invalid").exists());
    fs::write(&manifest_path, &original).unwrap();
    let blob = fs::read_dir(f.0.join("backup/blobs"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::write(&blob, "corrupted").unwrap();
    assert_eq!(
        backup::restore(f.0.join("backup"), f.0.join("corrupt"))
            .unwrap_err()
            .code,
        "backup_integrity_error"
    );
    assert!(!f.0.join("corrupt").exists());
    fs::remove_file(&blob).unwrap();
    assert!(backup::restore(f.0.join("backup"), f.0.join("missing")).is_err());
    assert!(!f.0.join("missing").exists());
}
#[test]
fn empty_destination_supported_and_missing_reference_is_rejected_even_with_rehashed_manifest() {
    let f = Fixture::new();
    let (s, h) = initialized(&f);
    let p = s
        .propose(
            &h,
            "r0",
            vec![Change::Put {
                path: "a.md".into(),
                content: "pending body".repeat(200),
            }],
            "pending",
            None,
            None,
        )
        .unwrap();
    assert!(!p.id.is_empty());
    backup::create(f.0.join("source"), f.0.join("backup")).unwrap();
    fs::create_dir(f.0.join("empty")).unwrap();
    backup::restore(f.0.join("backup"), f.0.join("empty")).unwrap();
    let manifest_path = f.0.join("backup/manifest.json");
    let mut m: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    m["blobs"] = serde_json::json!({});
    fs::write(&manifest_path, serde_json::to_vec(&m).unwrap()).unwrap();
    assert_eq!(
        backup::restore(f.0.join("backup"), f.0.join("missing-reference"))
            .unwrap_err()
            .code,
        "backup_missing_blob"
    );
    assert!(!f.0.join("missing-reference").exists());
}
#[cfg(unix)]
#[test]
fn symlink_files_and_destination_aliases_are_rejected() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new();
    let (_s, _h) = initialized(&f);
    backup::create(f.0.join("source"), f.0.join("backup")).unwrap();
    symlink(f.0.join("source"), f.0.join("alias")).unwrap();
    assert!(backup::create(f.0.join("source"), f.0.join("alias/nested")).is_err());
    assert!(backup::restore(f.0.join("backup"), f.0.join("alias")).is_err());
    fs::rename(f.0.join("backup/authority.sqlite3"), f.0.join("outside-db")).unwrap();
    symlink(f.0.join("outside-db"), f.0.join("backup/authority.sqlite3")).unwrap();
    assert_eq!(
        backup::restore(f.0.join("backup"), f.0.join("bad-link"))
            .unwrap_err()
            .code,
        "invalid_backup_file"
    );
}

#[test]
fn oversized_manifest_rejected_before_parsing_or_publication() {
    let f = Fixture::new();
    fs::create_dir(f.0.join("oversized")).unwrap();
    fs::create_dir(f.0.join("oversized/blobs")).unwrap();
    let manifest = fs::File::create(f.0.join("oversized/manifest.json")).unwrap();
    // Sparse input avoids constructing a large fixture in the test or repository.
    // The body is invalid JSON as well: this must hit the read bound first.
    manifest.set_len(64 * 1024 * 1024 + 1).unwrap();
    assert_eq!(
        backup::restore(f.0.join("oversized"), f.0.join("restored"))
            .unwrap_err()
            .code,
        "backup_manifest_too_large"
    );
    assert!(!f.0.join("restored").exists());
    assert!(!fs::read_dir(&f.0).unwrap().any(|entry| entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".mnemosyne-stage-")));
}
