use mnemosyne_content::{
    api::{ApiResult, Request},
    auth::{ActorKind, Grant, Permission, Policy, TrustedIdentity},
    onboarding::*,
    Change, Origin, Service,
};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};
static NEXT: AtomicUsize = AtomicUsize::new(0);
struct F {
    root: PathBuf,
    s: Service,
    h: TrustedIdentity,
    a: TrustedIdentity,
}
impl F {
    fn new() -> Self {
        let root = fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!(
                "m8-onboarding-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        let s = Service::open(root.join("store")).unwrap();
        let h = TrustedIdentity {
            tenant_id: "t".into(),
            actor_id: "h".into(),
            kind: ActorKind::Human,
            on_behalf_of: None,
        };
        let a = TrustedIdentity {
            actor_id: "a".into(),
            kind: ActorKind::Agent,
            ..h.clone()
        };
        s.bootstrap_policy(Policy {
            tenants: Default::default(),
            grants: ["h", "a"]
                .into_iter()
                .flat_map(|actor| {
                    [
                        Permission::Read,
                        Permission::Propose,
                        Permission::Edit,
                        Permission::Approve,
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
        Self { root, s, h, a }
    }
    fn files() -> Vec<ImportFile> {
        vec![
            ImportFile {
                path: "docs".into(),
                bytes: vec![],
                directory: true,
            },
            ImportFile {
                path: "docs/a.md".into(),
                bytes: "# Привет\r\n".as_bytes().to_vec(),
                directory: false,
            },
            ImportFile {
                path: "z.bin".into(),
                bytes: vec![255, 0, 128],
                directory: false,
            },
        ]
    }
}
impl Drop for F {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
#[test]
fn bounded_import_retry_restart_conflict_and_original_export() {
    let f = F::new();
    let files = F::files();
    let p =
        f.s.import_preview(
            &f.h,
            "r0",
            files[..2].to_vec(),
            0,
            Some(2),
            ConflictPolicy::Error,
        )
        .unwrap();
    assert_eq!(p.next_offset, Some(2));
    assert_eq!(f.s.head(&f.h).unwrap(), "r0");
    assert!(
        !f.root.join("store/blobs").exists(),
        "preview must not create blobs"
    );
    let ApiResult::Revision(r) =
        f.s.import_commit(&f.h, p.clone(), ImportMode::Edit, "b1", None, None)
            .unwrap()
    else {
        panic!()
    };
    let restarted = Service::open(f.root.join("store")).unwrap();
    let ApiResult::Revision(replay) = restarted
        .import_commit(&f.h, p.clone(), ImportMode::Edit, "b1", None, None)
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(r.id, replay.id);
    assert_eq!(
        restarted
            .import_commit(&f.h, p.clone(), ImportMode::Edit, "stale", None, None)
            .unwrap_err()
            .code,
        "stale_base"
    );
    let mut altered = p;
    altered.changes.clear();
    assert_eq!(
        restarted
            .import_commit(&f.h, altered, ImportMode::Edit, "altered", None, None)
            .unwrap_err()
            .code,
        "invalid_import_preview"
    );
    let p2 =
        f.s.import_preview(
            &f.h,
            &r.id,
            files[2..].to_vec(),
            2,
            None,
            ConflictPolicy::Error,
        )
        .unwrap();
    let ApiResult::Revision(r2) =
        f.s.import_commit(&f.h, p2, ImportMode::Edit, "b2", None, None)
            .unwrap()
    else {
        panic!()
    };
    let collision =
        f.s.import_preview(&f.h, &r2.id, files.clone(), 0, None, ConflictPolicy::Error)
            .unwrap();
    assert_eq!(
        f.s.import_commit(&f.h, collision, ImportMode::Edit, "collision", None, None)
            .unwrap_err()
            .code,
        "import_conflict"
    );
    let skip =
        f.s.import_preview(&f.h, &r2.id, files, 0, None, ConflictPolicy::Skip)
            .unwrap();
    assert!(matches!(
        f.s.import_commit(&f.h, skip, ImportMode::Edit, "skip", None, None)
            .unwrap(),
        ApiResult::ImportSkipped(_)
    ));
    let replacement = vec![ImportFile {
        path: "docs/a.md".into(),
        bytes: b"changed".to_vec(),
        directory: false,
    }];
    let p =
        f.s.import_preview(
            &f.h,
            &r2.id,
            replacement,
            0,
            None,
            ConflictPolicy::Overwrite,
        )
        .unwrap();
    f.s.import_commit(&f.h, p, ImportMode::Edit, "overwrite", None, None)
        .unwrap();
    let snapshot = f.s.export_snapshot(&f.h, "", None, 0, 32, None).unwrap();
    let dest = f.root.join("export");
    write_export(&snapshot, &dest).unwrap();
    assert_eq!(
        fs::read(dest.join("current/docs/a.md")).unwrap(),
        b"changed"
    );
    assert_eq!(
        fs::read(dest.join("original/docs/a.md")).unwrap(),
        "# Привет\r\n".as_bytes()
    );
    assert_eq!(
        fs::read(dest.join("current/z.bin")).unwrap(),
        vec![255, 0, 128]
    );
    assert_eq!(
        write_export(&snapshot, &dest).unwrap_err().code,
        "export_destination_exists"
    );
}
#[test]
fn agent_import_uses_hitl_and_permission_checks() {
    let f = F::new();
    let p =
        f.s.import_preview(&f.a, "r0", F::files(), 0, None, ConflictPolicy::Error)
            .unwrap();
    assert_eq!(
        f.s.import_commit(&f.a, p.clone(), ImportMode::Edit, "direct", None, None)
            .unwrap_err()
            .code,
        "human_required"
    );
    let ApiResult::Proposal(proposal) =
        f.s.import_commit(&f.a, p, ImportMode::Propose, "proposal", None, None)
            .unwrap()
    else {
        panic!()
    };
    assert_eq!(f.s.head(&f.h).unwrap(), "r0");
    assert_eq!(
        f.s.apply(&f.a, &proposal.id, &proposal.digest, "early")
            .unwrap_err()
            .code,
        "approval_required"
    );
    f.s.approve(&f.h, &proposal.id, &proposal.digest, "approve", None)
        .unwrap();
    f.s.apply(&f.a, &proposal.id, &proposal.digest, "apply")
        .unwrap();
    let denied = TrustedIdentity {
        actor_id: "none".into(),
        ..f.h.clone()
    };
    assert_eq!(
        f.s.export_snapshot(&denied, "", None, 0, 32, None)
            .unwrap_err()
            .code,
        "forbidden"
    );
}
#[test]
fn scan_rejects_symlinks_and_export_rejects_escapes() {
    let f = F::new();
    let source = f.root.join("source");
    fs::create_dir_all(source.join("nested/empty")).unwrap();
    fs::write(source.join("nested/data"), [0, 255, 2]).unwrap();
    let files = scan_directory(&source).unwrap();
    assert_eq!(files.len(), 3);
    assert!(files
        .iter()
        .any(|f| f.path == "nested/empty" && f.directory));
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(source.join("nested/data"), source.join("link")).unwrap();
        assert_eq!(scan_directory(&source).unwrap_err().code, "unsafe_source");
        let symlink = f.root.join("linked");
        std::os::unix::fs::symlink(&source, &symlink).unwrap();
        assert_eq!(
            write_export(
                &ExportSnapshot {
                    revision_id: "r0".into(),
                    policy_digest: "test".into(),
                    next_offset: None,
                    files: vec![]
                },
                &symlink.join("dest")
            )
            .unwrap_err()
            .code,
            "unsafe_path"
        );
    }
    let snap = ExportSnapshot {
        revision_id: "r0".into(),
        policy_digest: "test".into(),
        next_offset: None,
        files: vec![ExportFile {
            path: "../escape".into(),
            directory: false,
            bytes: vec![],
            original_bytes: vec![],
            bytes_base64: None,
            original_bytes_base64: None,
        }],
    };
    assert_eq!(
        write_export(&snap, &f.root.join("bad")).unwrap_err().code,
        "invalid_path"
    );
}
#[test]
fn invalid_source_collisions_and_empty_source_are_explicit() {
    let f = F::new();
    let files = vec![
        ImportFile {
            path: "a".into(),
            bytes: vec![1],
            directory: false,
        },
        ImportFile {
            path: "a/b".into(),
            bytes: vec![],
            directory: false,
        },
    ];
    assert_eq!(
        f.s.import_preview(&f.h, "r0", files, 0, None, ConflictPolicy::Error)
            .unwrap_err()
            .code,
        "invalid_import"
    );
    let p =
        f.s.import_preview(&f.h, "r0", vec![], 0, None, ConflictPolicy::Error)
            .unwrap();
    assert!(matches!(
        f.s.import_commit(&f.h, p, ImportMode::Edit, "empty", None, None)
            .unwrap(),
        ApiResult::ImportSkipped(_)
    ));
    let _ = Change::CreateFolder {
        path: "unused".into(),
    };
}
#[test]
fn export_pages_pin_revision_and_reject_authorization_changes() {
    let f = F::new();
    let files = F::files();
    let p =
        f.s.import_preview(&f.h, "r0", files, 0, None, ConflictPolicy::Error)
            .unwrap();
    f.s.import_commit(&f.h, p, ImportMode::Edit, "seed", None, None)
        .unwrap();
    let first = f.s.export_snapshot(&f.h, "", None, 0, 1, None).unwrap();
    assert_eq!(first.files.len(), 1);
    assert_eq!(first.next_offset, Some(1));
    let pinned = first.revision_id.clone();
    let next = first.next_offset.unwrap();
    let p =
        f.s.import_preview(
            &f.h,
            &pinned,
            vec![ImportFile {
                path: "new.md".into(),
                bytes: b"new".to_vec(),
                directory: false,
            }],
            0,
            None,
            ConflictPolicy::Error,
        )
        .unwrap();
    f.s.import_commit(&f.h, p, ImportMode::Edit, "new", None, None)
        .unwrap();
    // New paths change lineage authorization digest: an in-progress export fails closed.
    assert_eq!(
        f.s.export_snapshot(
            &f.h,
            "",
            Some(&pinned),
            next,
            32,
            Some(&first.policy_digest)
        )
        .unwrap_err()
        .code,
        "export_policy_changed"
    );
    let old =
        f.s.export_snapshot(&f.h, "", Some(&pinned), 0, 32, None)
            .unwrap();
    assert!(!old.files.iter().any(|f| f.path == "new.md"));
    let dest = f.root.join("staged");
    {
        let mut writer = ExportWriter::new(&dest, &first.revision_id).unwrap();
        writer.append(&first).unwrap();
        assert!(!dest.exists());
    }
    assert!(!dest.exists());
    assert!(!fs::read_dir(&f.root).unwrap().any(|p| p
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".mnemosyne-export-")));
}
#[test]
fn scanner_bounds_batches_without_directory_total_ceiling() {
    let f = F::new();
    let source = f.root.join("many");
    fs::create_dir(&source).unwrap();
    for i in 0..70 {
        fs::write(source.join(format!("{i:03}.md")), b"small").unwrap();
    }
    let (one, next) = scan_directory_batch(&source, 0, 32).unwrap();
    assert_eq!(one.len(), 32);
    assert_eq!(next, Some(32));
    let (two, next) = scan_directory_batch(&source, 32, 32).unwrap();
    assert_eq!(two.len(), 32);
    assert_eq!(next, Some(64));
    let (last, next) = scan_directory_batch(&source, 64, 32).unwrap();
    assert_eq!(last.len(), 6);
    assert_eq!(next, None);
}

fn execute(
    s: &Service,
    who: &TrustedIdentity,
    key: Option<&str>,
    operation: serde_json::Value,
) -> serde_json::Value {
    let request: Request = serde_json::from_value(serde_json::json!({
        "version": 2,
        "idempotency_key": key,
        "operation": operation,
    }))
    .unwrap();
    serde_json::to_value(s.execute(who, request)).unwrap()
}

const PNG: &[u8] = &[
    0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0x0d, 0xff,
];

fn seeded_for_export() -> F {
    let f = F::new();
    let mut files = F::files();
    files.push(ImportFile {
        path: "img/photo.png".into(),
        bytes: PNG.to_vec(),
        directory: false,
    });
    files.push(ImportFile {
        path: "note.md".into(),
        bytes: "# Note\n\nЗаметка.\n".as_bytes().to_vec(),
        directory: false,
    });
    let p =
        f.s.import_preview(&f.h, "r0", files, 0, None, ConflictPolicy::Error)
            .unwrap();
    f.s.import_commit(&f.h, p, ImportMode::Edit, "seed", None, None)
        .unwrap();
    f
}

/// Every file of `dir`, by path below it, with its bytes; folders as `None`.
fn tree(dir: &std::path::Path) -> std::collections::BTreeMap<String, Option<Vec<u8>>> {
    fn walk(
        root: &std::path::Path,
        dir: &std::path::Path,
        out: &mut std::collections::BTreeMap<String, Option<Vec<u8>>>,
    ) {
        for child in fs::read_dir(dir).unwrap() {
            let path = child.unwrap().path();
            let name = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if path.is_dir() {
                out.insert(name, None);
                walk(root, &path, out);
            } else {
                out.insert(name, Some(fs::read(&path).unwrap()));
            }
        }
    }
    let mut out = std::collections::BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

#[test]
fn export_base64_round_trips() {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD;
    let f = seeded_for_export();
    let array = execute(
        &f.s,
        &f.h,
        None,
        serde_json::json!({"type": "export_snapshot"}),
    );
    let explicit = execute(
        &f.s,
        &f.h,
        None,
        serde_json::json!({"type": "export_snapshot", "encoding": "array"}),
    );
    assert_eq!(array, explicit);
    assert!(!array.to_string().contains("bytes_base64"), "{array}");
    let encoded = execute(
        &f.s,
        &f.h,
        None,
        serde_json::json!({"type": "export_snapshot", "encoding": "base64"}),
    );
    assert!(encoded["error"].is_null(), "{encoded}");
    let (a, e) = (&array["result"], &encoded["result"]);
    for key in ["revision_id", "policy_digest", "next_offset"] {
        assert_eq!(a[key], e[key]);
    }
    let a_files = a["files"].as_array().unwrap();
    let e_files = e["files"].as_array().unwrap();
    assert_eq!(a_files.len(), e_files.len());
    assert!(a_files.iter().any(|f| f["path"] == "img/photo.png"));
    for (a, e) in a_files.iter().zip(e_files) {
        assert_eq!(a["path"], e["path"]);
        assert_eq!(a["directory"], e["directory"]);
        for (array_key, base64_key) in [
            ("bytes", "bytes_base64"),
            ("original_bytes", "original_bytes_base64"),
        ] {
            assert_eq!(e[array_key], serde_json::json!([]), "{e}");
            let decoded = b64.decode(e[base64_key].as_str().unwrap()).unwrap();
            let expected: Vec<u8> = serde_json::from_value(a[array_key].clone()).unwrap();
            assert_eq!(decoded, expected, "{}", a["path"]);
        }
    }
    let photo = e_files
        .iter()
        .find(|f| f["path"] == "img/photo.png")
        .unwrap();
    assert_eq!(photo["bytes_base64"], b64.encode(PNG));
    // The writer decodes either form into the same tree.
    let pages: Vec<ExportSnapshot> = [a, e]
        .iter()
        .map(|v| serde_json::from_value((*v).clone()).unwrap())
        .collect();
    let (from_array, from_base64) = (f.root.join("array"), f.root.join("base64"));
    write_export(&pages[0], &from_array).unwrap();
    write_export(&pages[1], &from_base64).unwrap();
    assert_eq!(tree(&from_array), tree(&from_base64));
    assert_eq!(
        fs::read(from_base64.join("current/img/photo.png")).unwrap(),
        PNG
    );
    // A page carrying bytes in both forms, or base64 that does not decode, is refused.
    let mut both = pages[1].clone();
    both.files[0].bytes = vec![1];
    let mut broken = pages[1].clone();
    broken.files[0].bytes_base64 = Some("not base64!".into());
    for page in [both, broken] {
        assert_eq!(
            write_export(&page, &f.root.join("refused"))
                .unwrap_err()
                .code,
            "invalid_request"
        );
    }
    for encoding in ["BASE64", "", "hex"] {
        let v = execute(
            &f.s,
            &f.h,
            None,
            serde_json::json!({"type": "export_snapshot", "encoding": encoding}),
        );
        assert_eq!(v["error"]["code"], "invalid_request", "{encoding:?}");
    }
    let v1: Request = serde_json::from_value(serde_json::json!({
        "version": 1,
        "operation": {"type": "export_snapshot", "encoding": "base64"},
    }))
    .unwrap();
    assert_eq!(
        f.s.execute(&f.h, v1).error.unwrap().code,
        "unsupported_version"
    );
}

#[test]
fn import_commit_records_message_and_origin() {
    let f = F::new();
    let message = "Imported from the tenant's files";
    let origin = Origin {
        source: "assistant".into(),
        model: Some("model-x".into()),
        chat_id: Some("chat-1".into()),
    };
    let p =
        f.s.import_preview(&f.h, "r0", F::files(), 0, None, ConflictPolicy::Error)
            .unwrap();
    let commit = |key: &str, message: Option<&str>, origin: Option<Origin>| {
        f.s.import_commit(&f.h, p.clone(), ImportMode::Edit, key, message, origin)
    };
    assert_eq!(
        commit("m1", Some(&"x".repeat(2049)), None)
            .unwrap_err()
            .code,
        "invalid_message"
    );
    assert_eq!(
        commit("m1", Some("bell\u{7}"), None).unwrap_err().code,
        "invalid_message"
    );
    let human = Origin {
        source: "human".into(),
        model: None,
        chat_id: None,
    };
    assert_eq!(
        commit("m1", None, Some(human)).unwrap_err().code,
        "invalid_origin"
    );
    let ApiResult::Revision(r) = commit("m1", Some(message), Some(origin.clone())).unwrap() else {
        panic!()
    };
    assert_eq!(r.message.as_deref(), Some(message));
    assert_eq!(r.origin, Some(origin.clone()));
    // The same key replays only the same words.
    let ApiResult::Revision(again) = commit("m1", Some(message), Some(origin.clone())).unwrap()
    else {
        panic!()
    };
    assert_eq!(again.id, r.id);
    for (other, o) in [
        (Some("Another message"), Some(origin.clone())),
        (None, Some(origin.clone())),
        (Some(message), None),
    ] {
        assert_eq!(
            commit("m1", other, o).unwrap_err().code,
            "idempotency_conflict"
        );
    }
    let log = serde_json::to_value(f.s.log(&f.h, "", 20, 0).unwrap()).unwrap();
    let entry = &log["entries"][0];
    assert_eq!(entry["id"], r.id.as_str());
    assert_eq!(entry["kind"], "import");
    assert_eq!(entry["message"], message);
    assert_eq!(entry["origin"]["model"], "model-x");
    assert_eq!(entry["actor"]["actor_id"], "h");
    // Propose mode, through the API: the message is the proposal's reason.
    let files = vec![ImportFile {
        path: "later.md".into(),
        bytes: b"later".to_vec(),
        directory: false,
    }];
    let p2 =
        f.s.import_preview(&f.h, &r.id, files, 0, None, ConflictPolicy::Error)
            .unwrap();
    let v = execute(
        &f.s,
        &f.a,
        Some("m2"),
        serde_json::json!({"type": "import_commit", "preview": p2, "mode": "propose",
                           "message": message, "origin": origin}),
    );
    assert!(v["error"].is_null(), "{v}");
    assert_eq!(v["result"]["reason"], message);
    assert_eq!(v["result"]["origin"]["chat_id"], "chat-1");
    let v = execute(
        &f.s,
        &f.a,
        Some("m2"),
        serde_json::json!({"type": "import_commit", "preview": p2, "mode": "propose",
                           "message": "Other words", "origin": origin}),
    );
    assert_eq!(v["error"]["code"], "idempotency_conflict");
    // A skipped batch publishes nothing, so it records neither.
    let p3 =
        f.s.import_preview(&f.h, &r.id, F::files(), 0, None, ConflictPolicy::Skip)
            .unwrap();
    assert!(p3.changes.is_empty());
    let v = execute(
        &f.s,
        &f.h,
        Some("m3"),
        serde_json::json!({"type": "import_commit", "preview": p3, "mode": "edit",
                           "message": message, "origin": origin}),
    );
    assert_eq!(v["result"]["skipped"], true, "{v}");
    assert!(v["result"].get("message").is_none());
    assert_eq!(f.s.head(&f.h).unwrap(), r.id);
}
