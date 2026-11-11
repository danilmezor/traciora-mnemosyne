use mnemosyne_content::{
    auth::{ActorKind, Grant, Permission, Policy, TrustedIdentity},
    Change, EntryKind, Service,
};
use std::{
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};
static NEXT: AtomicUsize = AtomicUsize::new(0);
struct Fixture {
    root: PathBuf,
    s: Service,
    h: TrustedIdentity,
    a: TrustedIdentity,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "mnemosyne-lifecycle-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let s = Service::open(&root).unwrap();
        let h = TrustedIdentity {
            tenant_id: "tenant".into(),
            actor_id: "human".into(),
            kind: ActorKind::Human,
            on_behalf_of: None,
        };
        let a = TrustedIdentity {
            actor_id: "agent".into(),
            kind: ActorKind::Agent,
            ..h.clone()
        };
        let mut grants = vec![];
        for actor in ["human", "agent"] {
            for permission in [
                Permission::Read,
                Permission::Edit,
                Permission::Propose,
                Permission::Approve,
                Permission::Admin,
            ] {
                grants.push(Grant {
                    tenant_id: "tenant".into(),
                    actor_id: actor.into(),
                    permission,
                    scope: "".into(),
                });
            }
        }
        grants.push(Grant {
            tenant_id: "tenant".into(),
            actor_id: "limited".into(),
            permission: Permission::Read,
            scope: "public".into(),
        });
        s.bootstrap_policy(Policy {
            grants,
            tenants: Default::default(),
        })
        .unwrap();
        Self { root, s, h, a }
    }
    fn commit(&self, base: &str, changes: Vec<Change>, key: &str) -> String {
        let p = self
            .s
            .propose(&self.a, base, changes, &format!("{key}-p"), None, None)
            .unwrap();
        assert_eq!(
            self.s
                .apply(&self.a, &p.id, &p.digest, &format!("{key}-early"))
                .unwrap_err()
                .code,
            "approval_required"
        );
        self.s
            .approve(&self.h, &p.id, &p.digest, &format!("{key}-approve"), None)
            .unwrap();
        self.s
            .apply(&self.a, &p.id, &p.digest, &format!("{key}-apply"))
            .unwrap()
            .id
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
#[test]
fn binary_identity_original_recursive_rename_delete_and_reviewed_restore() {
    let f = Fixture::new();
    let bytes = vec![0, 255, 1, 128];
    let first = f.commit(
        "r0",
        vec![
            Change::CreateFolder {
                path: "docs".into(),
            },
            Change::PutBinary {
                path: "docs/file.bin".into(),
                bytes: bytes.clone(),
            },
        ],
        "seed",
    );
    let file =
        f.s.read_binary(&f.h, "docs/file.bin", None, false, None)
            .unwrap();
    assert_eq!(file.bytes, bytes);
    assert_eq!(file.entry.kind, EntryKind::Binary);
    let second = f.commit(
        &first,
        vec![Change::PutBinary {
            path: "docs/file.bin".into(),
            bytes: vec![2, 3],
        }],
        "edit",
    );
    assert_eq!(
        f.s.read_binary(&f.h, "docs/file.bin", None, true, None)
            .unwrap()
            .bytes,
        bytes
    );
    let renamed = f.commit(
        &second,
        vec![Change::Rename {
            path: "docs".into(),
            to: "archive".into(),
        }],
        "rename",
    );
    assert_eq!(
        f.s.read_binary(&f.h, "archive/file.bin", None, false, None)
            .unwrap()
            .entry
            .file_id,
        file.entry.file_id
    );
    assert_eq!(f.s.history(&f.h, "archive/file.bin").unwrap().len(), 3);
    assert_eq!(
        f.s.read_binary(&f.h, "docs/file.bin", Some(&first), false, None)
            .unwrap()
            .bytes,
        bytes
    );
    let deleted = f.commit(
        &renamed,
        vec![Change::Delete {
            path: "archive".into(),
        }],
        "delete",
    );
    assert!(f.s.list(&f.h, "").unwrap().is_empty());
    let restored = f.commit(
        &deleted,
        vec![Change::Restore {
            path: "archive/file.bin".into(),
            revision_id: renamed.clone(),
        }],
        "restore",
    );
    assert_ne!(restored, renamed);
    assert_eq!(
        f.s.read_binary(&f.h, "archive/file.bin", None, false, None)
            .unwrap()
            .bytes,
        vec![2, 3]
    );
    assert_eq!(f.s.history(&f.h, "archive/file.bin").unwrap().len(), 5);
}
#[test]
fn historical_names_deletions_and_mixed_atomic_failure_do_not_leak() {
    let f = Fixture::new();
    let first = f.commit(
        "r0",
        vec![Change::Put {
            path: "private/a".into(),
            content: "secret".into(),
        }],
        "seed",
    );
    let moved = f.commit(
        &first,
        vec![Change::Rename {
            path: "private/a".into(),
            to: "public/a".into(),
        }],
        "move",
    );
    let limited = TrustedIdentity {
        actor_id: "limited".into(),
        ..f.h.clone()
    };
    assert_eq!(
        f.s.read(&limited, "public/a", None).unwrap_err().code,
        "not_found"
    );
    assert!(f.s.list(&limited, "public").unwrap().is_empty());
    assert_eq!(
        f.s.history(&limited, "public/a").unwrap_err().code,
        "not_found"
    );
    let failed = f.s.edit(
        &f.h,
        &moved,
        vec![
            Change::Put {
                path: "okay".into(),
                content: "never published".into(),
            },
            Change::Delete {
                path: "missing".into(),
            },
        ],
        "atomic",
        None,
        None,
    );
    assert!(failed.is_err());
    assert_eq!(f.s.head(&f.h).unwrap(), moved);
    assert!(f.s.read(&f.h, "okay", None).is_err());
    let deleted = f.commit(
        &moved,
        vec![Change::Delete {
            path: "public/a".into(),
        }],
        "delete",
    );
    assert_eq!(f.s.diff(&limited, &deleted).unwrap_err().code, "not_found");
    assert_eq!(
        f.s.read(&limited, "public/a", Some(&moved))
            .unwrap_err()
            .code,
        "not_found"
    );
}
#[test]
fn folder_descendants_require_reviewer_authority_and_agent_restore_cannot_bypass() {
    let f = Fixture::new();
    let first = f.commit(
        "r0",
        vec![
            Change::CreateFolder {
                path: "docs".into(),
            },
            Change::Put {
                path: "docs/a".into(),
                content: "one".into(),
            },
        ],
        "seed",
    );
    let p =
        f.s.propose(
            &f.a,
            &first,
            vec![Change::Delete {
                path: "docs".into(),
            }],
            "delete-p",
            None,
            None,
        )
        .unwrap();
    let mut policy = Policy::default();
    // The human holds no root Admin grant: that would imply Approve everywhere.
    for actor in ["human", "agent"] {
        for permission in [
            Permission::Read,
            Permission::Propose,
            Permission::Edit,
            Permission::Admin,
        ] {
            if actor == "human" && permission == Permission::Admin {
                continue;
            }
            policy.grants.push(Grant {
                tenant_id: "tenant".into(),
                actor_id: actor.into(),
                permission,
                scope: "".into(),
            });
        }
    }
    policy.grants.push(Grant {
        tenant_id: "tenant".into(),
        actor_id: "human".into(),
        permission: Permission::Approve,
        scope: "unrelated".into(),
    });
    f.s.set_policy(&f.h, policy).unwrap();
    assert_eq!(
        f.s.approve(&f.h, &p.id, &p.digest, "denied", None)
            .unwrap_err()
            .code,
        "forbidden"
    );
    assert_eq!(f.s.head(&f.h).unwrap(), first);
    assert_eq!(
        f.s.edit(
            &f.a,
            &first,
            vec![Change::Restore {
                path: "docs/a".into(),
                revision_id: first.clone()
            }],
            "bypass",
            None,
            None
        )
        .unwrap_err()
        .code,
        "human_required"
    );
}

#[test]
fn corrupt_objects_fail_closed_and_unselected_history_does_not_block_edit() {
    let f = Fixture::new();
    let first = f.commit(
        "r0",
        vec![Change::PutBinary {
            path: "data.bin".into(),
            bytes: vec![8; 2048],
        }],
        "first",
    );
    let old =
        f.s.read_binary(&f.h, "data.bin", None, false, None)
            .unwrap()
            .entry
            .blob
            .unwrap();
    let second = f.commit(
        &first,
        vec![Change::PutBinary {
            path: "data.bin".into(),
            bytes: vec![9; 2048],
        }],
        "second",
    );
    let approved =
        f.s.propose(
            &f.a,
            &second,
            vec![Change::Restore {
                path: "data.bin".into(),
                revision_id: first.clone(),
            }],
            "before-corruption",
            None,
            None,
        )
        .unwrap();
    f.s.approve(
        &f.h,
        &approved.id,
        &approved.digest,
        "approve-before-corruption",
        None,
    )
    .unwrap();
    std::fs::write(f.root.join("blobs").join(&old), b"corrupt").unwrap();
    assert_eq!(
        f.s.apply(&f.a, &approved.id, &approved.digest, "after-corruption")
            .unwrap_err()
            .code,
        "corrupt_blob"
    );
    let third =
        f.s.edit(
            &f.h,
            &second,
            vec![Change::Put {
                path: "unrelated".into(),
                content: "valid".into(),
            }],
            "unrelated",
            None,
            None,
        )
        .unwrap();
    assert_eq!(f.s.read(&f.h, "unrelated", None).unwrap().content, "valid");
    assert_eq!(
        f.s.read_binary(&f.h, "data.bin", Some(&first), false, None)
            .unwrap_err()
            .code,
        "corrupt_blob"
    );
    assert_eq!(
        f.s.propose(
            &f.a,
            &third.id,
            vec![Change::Restore {
                path: "data.bin".into(),
                revision_id: first
            }],
            "corrupt-restore",
            None,
            None
        )
        .unwrap_err()
        .code,
        "corrupt_blob"
    );
    let p = f.s.propose(
        &f.a,
        &third.id,
        vec![Change::Restore {
            path: "data.bin".into(),
            revision_id: second,
        }],
        "restore-p",
        None,
        None,
    );
    assert_eq!(p.unwrap_err().code, "corrupt_blob"); // Immutable original is also verified.
    let current =
        f.s.read_binary(&f.h, "data.bin", None, false, None)
            .unwrap()
            .entry
            .blob
            .unwrap();
    std::fs::remove_file(f.root.join("blobs").join(current)).unwrap();
    assert!(f
        .s
        .read_binary(&f.h, "data.bin", None, false, None)
        .is_err());
    assert_eq!(f.s.head(&f.h).unwrap(), third.id);
}

#[test]
fn implicit_directory_collisions_reject_rename_and_restore_without_publication() {
    let f = Fixture::new();
    let first = f.commit(
        "r0",
        vec![Change::Put {
            path: "dst".into(),
            content: "historical file".into(),
        }],
        "historical",
    );
    let deleted = f.commit(
        &first,
        vec![Change::Delete { path: "dst".into() }],
        "remove-file",
    );
    let head = f.commit(
        &deleted,
        vec![
            Change::Put {
                path: "dst/child".into(),
                content: "child".into(),
            },
            Change::Put {
                path: "src".into(),
                content: "source".into(),
            },
        ],
        "implicit-dir",
    );
    let paths = f.s.list(&f.h, "").unwrap();
    for (name, change) in [
        (
            "rename",
            Change::Rename {
                path: "src".into(),
                to: "dst".into(),
            },
        ),
        (
            "restore",
            Change::Restore {
                path: "dst".into(),
                revision_id: first.clone(),
            },
        ),
    ] {
        let changes = vec![
            Change::Put {
                path: "must-not-publish".into(),
                content: "atomic".into(),
            },
            change,
        ];
        assert_eq!(
            f.s.propose(
                &f.a,
                &head,
                changes.clone(),
                &format!("{name}-proposal"),
                None,
                None
            )
            .unwrap_err()
            .code,
            "path_conflict"
        );
        assert_eq!(
            f.s.edit(&f.h, &head, changes, &format!("{name}-edit"), None, None)
                .unwrap_err()
                .code,
            "path_conflict"
        );
        assert_eq!(f.s.head(&f.h).unwrap(), head);
        assert_eq!(f.s.list(&f.h, "").unwrap(), paths);
        assert_eq!(f.s.read(&f.h, "dst/child", None).unwrap().content, "child");
        assert_eq!(f.s.read(&f.h, "src", None).unwrap().content, "source");
    }
}

#[test]
fn cyclic_schema1_parent_chain_is_rejected_without_migration_commit() {
    let root = std::env::temp_dir().join(format!(
        "mnemosyne-schema1-cycle-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let mut legacy: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/schema1-authority.json")).unwrap();
    let head = legacy["tenants"]["fixture"]["head"]
        .as_str()
        .unwrap()
        .to_owned();
    legacy["tenants"]["fixture"]["revisions"][&head]["revision"]["parent"] = head.clone().into();
    let bytes = serde_json::to_vec(&legacy).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("authority.json"), &bytes).unwrap();
    let service = Service::open(&root).unwrap();
    let human = TrustedIdentity {
        tenant_id: "fixture".into(),
        actor_id: "human".into(),
        kind: ActorKind::Human,
        on_behalf_of: None,
    };
    for _ in 0..2 {
        assert_eq!(
            service.head(&human).unwrap_err().code,
            "invalid_legacy_store"
        );
    }
    assert_eq!(std::fs::read(root.join("authority.json")).unwrap(), bytes);
    let db = rusqlite::Connection::open(root.join("authority.sqlite3")).unwrap();
    let count: i64 = db
        .query_row("SELECT count(*) FROM meta", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}
