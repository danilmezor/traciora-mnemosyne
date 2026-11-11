use mnemosyne_content::{
    api::{Operation, Request},
    auth::{ActorKind, Grant, Permission, Policy, TrustedIdentity},
    extraction::ExtractionStatus,
    Change, Service,
};

#[test]
fn extraction_is_current_authorized_and_versioned_before_cache_access() {
    let root =
        std::env::temp_dir().join(format!("mnemosyne-extraction-api-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let service = Service::open(&root).unwrap();
    let owner = TrustedIdentity {
        tenant_id: "t".into(),
        actor_id: "owner".into(),
        kind: ActorKind::Human,
        on_behalf_of: None,
    };
    let reader = TrustedIdentity {
        actor_id: "reader".into(),
        kind: ActorKind::Agent,
        ..owner.clone()
    };
    let grant = |actor: &str, permission, scope: &str| Grant {
        tenant_id: "t".into(),
        actor_id: actor.into(),
        permission,
        scope: scope.into(),
    };
    service
        .bootstrap_policy(Policy {
            tenants: Default::default(),
            grants: vec![
                grant("owner", Permission::Read, ""),
                grant("owner", Permission::Edit, ""),
                grant("reader", Permission::Read, "public"),
            ],
        })
        .unwrap();
    let first = service
        .edit(
            &owner,
            "r0",
            vec![Change::PutBinary {
                path: "private/source.pdf".into(),
                bytes: b"first source".to_vec(),
            }],
            "seed",
            None,
            None,
        )
        .unwrap();
    assert_eq!(
        service
            .extraction_status(&reader, "private/source.pdf")
            .unwrap_err()
            .code,
        "forbidden"
    );
    assert!(!root.join("extraction-v1").exists());
    let status = service
        .extraction_status(&owner, "private/source.pdf")
        .unwrap();
    assert_eq!(status.extraction.status, ExtractionStatus::Pending);
    assert_eq!(status.revision_id, first.id);
    assert!(status.extraction.segments.is_empty());
    let next = service
        .edit(
            &owner,
            &first.id,
            vec![Change::Rename {
                path: "private/source.pdf".into(),
                to: "public/source.pdf".into(),
            }],
            "move",
            None,
            None,
        )
        .unwrap();
    assert_eq!(
        service
            .extraction_status(&reader, "public/source.pdf")
            .unwrap_err()
            .code,
        "not_found"
    );
    assert_eq!(
        service
            .extraction_status(&owner, "private/source.pdf")
            .unwrap_err()
            .code,
        "not_found"
    );
    let moved = service
        .extraction_status(&owner, "public/source.pdf")
        .unwrap();
    assert_eq!(moved.file_id, status.file_id);
    assert_eq!(moved.source_blob, status.source_blob);
    assert_eq!(moved.revision_id, next.id);
    // A read grant alone never authorizes expensive explicit regeneration.
    assert_eq!(
        service
            .retry_extraction(&reader, "public/source.pdf")
            .unwrap_err()
            .code,
        "forbidden"
    );
    let replaced = service
        .edit(
            &owner,
            &next.id,
            vec![Change::PutBinary {
                path: "public/source.pdf".into(),
                bytes: b"new source".to_vec(),
            }],
            "replace",
            None,
            None,
        )
        .unwrap();
    let current = service
        .extraction_status(&owner, "public/source.pdf")
        .unwrap();
    assert_ne!(current.source_blob, status.source_blob);
    assert_eq!(current.revision_id, replaced.id);
    for operation in [
        Operation::ExtractionStatus {
            path: "public/source.pdf".into(),
        },
        Operation::ExtractedText {
            path: "public/source.pdf".into(),
        },
        Operation::RetryExtraction {
            path: "public/source.pdf".into(),
        },
    ] {
        assert_eq!(
            service
                .execute(
                    &owner,
                    Request {
                        version: 1,
                        idempotency_key: None,
                        operation
                    }
                )
                .error
                .unwrap()
                .code,
            "unsupported_version"
        );
    }
    assert!(serde_json::from_value::<Request>(serde_json::json!({"version":2,"operation":{"type":"extracted_text","path":"public/source.pdf","revision":"r1"}})).is_err());
    service
        .edit(
            &owner,
            &replaced.id,
            vec![Change::Delete {
                path: "public/source.pdf".into(),
            }],
            "delete",
            None,
            None,
        )
        .unwrap();
    assert_eq!(
        service
            .extraction_status(&owner, "public/source.pdf")
            .unwrap_err()
            .code,
        "not_found"
    );
    std::fs::remove_dir_all(root).unwrap();
}
