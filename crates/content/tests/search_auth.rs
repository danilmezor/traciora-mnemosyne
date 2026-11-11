use mnemosyne_content::{
    auth::{ActorKind, Grant, Permission, Policy, TrustedIdentity},
    search::{SearchField, SearchRequest},
    Change, Service,
};
use std::sync::atomic::{AtomicU64, Ordering};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(std::path::PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn actor(name: &str, kind: ActorKind) -> TrustedIdentity {
    TrustedIdentity {
        tenant_id: "tenant".into(),
        actor_id: name.into(),
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
fn query(q: &str, scope: &str) -> SearchRequest {
    SearchRequest {
        query: q.into(),
        mode: Default::default(),
        min_score: None,
        scope: scope.into(),
        extensions: vec![],
        limit: 20,
        offset: 0,
        snapshot_id: None,
        fields: SearchField::all(),
        typos: true,
    }
}
fn put(path: &str, text: &str) -> Change {
    Change::Put {
        path: path.into(),
        content: text.into(),
    }
}
#[test]
fn search_authorizes_before_pagination_and_tracks_lineage_and_policy() {
    let fixture = Fixture(std::env::temp_dir().join(format!(
        "mnemosyne-search-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )));
    let service = Service::open(&fixture.0).unwrap();
    let owner = actor("owner", ActorKind::Human);
    let reader = actor("reader", ActorKind::Agent);
    let owner_grants = vec![
        grant(&owner, Permission::Read, ""),
        grant(&owner, Permission::Edit, ""),
        grant(&owner, Permission::Admin, ""),
    ];
    let mut grants = owner_grants.clone();
    grants.push(grant(&reader, Permission::Read, "public"));
    service
        .bootstrap_policy(Policy {
            grants,
            tenants: Default::default(),
        })
        .unwrap();
    let first = service
        .edit(
            &owner,
            "r0",
            vec![
                put("private/secret.md", "needle forbidden-secret"),
                put("public/a.md", "needle safe alpha"),
                put("public/b.txt", "needle safe beta"),
            ],
            "seed",
            None,
            None,
        )
        .unwrap();
    let mut request = query("needle", "public");
    request.limit = 1;
    let first_page = service.search(&reader, &request).unwrap();
    assert_eq!(first_page.total, 2);
    assert_eq!(first_page.hits.len(), 1);
    assert_eq!(first_page.next_offset, Some(1));
    assert!(!serde_json::to_string(&first_page)
        .unwrap()
        .contains("forbidden-secret"));
    request.offset = 1;
    request.snapshot_id = Some(first_page.snapshot_id.clone());
    let second_page = service.search(&reader, &request).unwrap();
    assert_ne!(first_page.hits[0].path, second_page.hits[0].path);
    assert_eq!(second_page.next_offset, None);
    let moved = service
        .edit(
            &owner,
            &first.id,
            vec![Change::Rename {
                path: "private/secret.md".into(),
                to: "public/moved.md".into(),
            }],
            "move",
            None,
            None,
        )
        .unwrap();
    assert_eq!(
        service.search(&reader, &request).unwrap_err().code,
        "stale_search"
    );
    assert_eq!(
        service
            .search(&reader, &query("needle", "public"))
            .unwrap()
            .total,
        2
    );
    let before_hidden_edit = service.search(&reader, &query("needle", "public")).unwrap();
    let hidden_edit = service
        .edit(
            &owner,
            &moved.id,
            vec![put(
                "public/moved.md",
                &"needle forbidden-secret ".repeat(100),
            )],
            "hidden-edit",
            None,
            None,
        )
        .unwrap();
    let after_hidden_edit = service.search(&reader, &query("needle", "public")).unwrap();
    assert_eq!(
        before_hidden_edit
            .hits
            .iter()
            .map(|h| (&h.path, h.score))
            .collect::<Vec<_>>(),
        after_hidden_edit
            .hits
            .iter()
            .map(|h| (&h.path, h.score))
            .collect::<Vec<_>>()
    );
    let mut filtered = query("needle", "public");
    filtered.extensions = vec!["MD".into()];
    assert_eq!(service.search(&reader, &filtered).unwrap().total, 1);
    let mut delegated = reader.clone();
    delegated.on_behalf_of = Some("requester".into());
    assert_eq!(
        service
            .search(&delegated, &query("needle", "public"))
            .unwrap_err()
            .code,
        "forbidden"
    );
    service
        .set_policy(
            &owner,
            Policy {
                tenants: Default::default(),
                grants: owner_grants,
            },
        )
        .unwrap();
    assert_eq!(
        service
            .search(&reader, &query("needle", "public"))
            .unwrap_err()
            .code,
        "forbidden"
    );
    assert_eq!(
        service
            .search(&owner, &query("needle", ""))
            .unwrap()
            .revision_id,
        hidden_edit.id
    );
}
#[test]
fn search_ranks_multilingual_matches_and_excludes_pending_drafts() {
    let fixture = Fixture(std::env::temp_dir().join(format!(
        "mnemosyne-ranking-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )));
    let service = Service::open(&fixture.0).unwrap();
    let owner = actor("owner", ActorKind::Human);
    service
        .bootstrap_policy(Policy {
            tenants: Default::default(),
            grants: vec![
                grant(&owner, Permission::Read, ""),
                grant(&owner, Permission::Edit, ""),
                grant(&owner, Permission::Propose, ""),
            ],
        })
        .unwrap();
    let revision=service.edit(&owner,"r0",vec![put("a.md","собаками running search engine"),put("b.md","search engine unrelated filler filler filler filler filler filler filler filler filler filler")],"seed", None, None).unwrap();
    assert_eq!(
        service
            .search(&owner, &query("собака runs", ""))
            .unwrap()
            .hits[0]
            .path,
        "a.md"
    );
    let ranked = service.search(&owner, &query("search engine", "")).unwrap();
    assert_eq!(ranked.total, 2);
    assert_eq!(ranked.hits[0].path, "a.md");
    service
        .propose(
            &owner,
            &revision.id,
            vec![put("draft.md", "unpublishedtoken")],
            "draft",
            None,
            None,
        )
        .unwrap();
    assert_eq!(
        service
            .search(&owner, &query("unpublishedtoken", ""))
            .unwrap()
            .total,
        0
    );
    for q in ["", "!!!"] {
        assert_eq!(
            service.search(&owner, &query(q, "")).unwrap_err().code,
            "invalid_search"
        );
    }
    let request = serde_json::from_value(
        serde_json::json!({"version":1,"operation":{"type":"search","query":"search"}}),
    )
    .unwrap();
    assert_eq!(
        service.execute(&owner, request).error.unwrap().code,
        "unsupported_version"
    );
    assert!(serde_json::from_value::<mnemosyne_content::api::Request>(serde_json::json!({"version":2,"operation":{"type":"search","query":"search","tenant_id":"other"}})).is_err());
}

#[test]
fn search_token_limit_includes_eighty_utf8_bytes() {
    let fixture = Fixture(std::env::temp_dir().join(format!(
        "mnemosyne-search-token-boundary-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )));
    let service = Service::open(&fixture.0).unwrap();
    let owner = actor("owner", ActorKind::Human);
    service
        .bootstrap_policy(Policy {
            tenants: Default::default(),
            grants: vec![
                grant(&owner, Permission::Read, ""),
                grant(&owner, Permission::Edit, ""),
            ],
        })
        .unwrap();
    let ascii = "x".repeat(80);
    let cyrillic = "ж".repeat(40);
    assert_eq!(ascii.len(), 80);
    assert_eq!(cyrillic.len(), 80);
    service
        .edit(
            &owner,
            "r0",
            vec![put("ascii.md", &ascii), put("cyrillic.md", &cyrillic)],
            "token-boundary",
            None,
            None,
        )
        .unwrap();
    for (token, expected_path) in [(&ascii, "ascii.md"), (&cyrillic, "cyrillic.md")] {
        let result = service.search(&owner, &query(token, "")).unwrap();
        assert_eq!(result.total, 1);
        assert_eq!(result.hits[0].path, expected_path);
        let invalid = format!("{token}x");
        assert_eq!(invalid.len(), 81);
        assert_eq!(
            service
                .search(&owner, &query(&invalid, ""))
                .unwrap_err()
                .code,
            "invalid_search"
        );
    }
}

#[test]
fn alternating_visibility_keeps_the_current_persistent_manifest() {
    let fixture = Fixture(std::env::temp_dir().join(format!(
        "mnemosyne-search-manifest-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )));
    let service = Service::open(&fixture.0).unwrap();
    let owner = actor("owner", ActorKind::Human);
    let reader = actor("reader", ActorKind::Agent);
    service
        .bootstrap_policy(Policy {
            tenants: Default::default(),
            grants: vec![
                grant(&owner, Permission::Read, ""),
                grant(&owner, Permission::Edit, ""),
                grant(&reader, Permission::Read, "public"),
            ],
        })
        .unwrap();
    service
        .edit(
            &owner,
            "r0",
            vec![
                put("public/a.md", "sharedterm public"),
                put("private/b.txt", "sharedterm private"),
                Change::PutBinary {
                    path: "private/unparsed.pdf".into(),
                    bytes: b"must not parse on reader request".to_vec(),
                },
            ],
            "seed",
            None,
            None,
        )
        .unwrap();
    // The scoped reader builds every current text source, without parsing a
    // denied binary source. Changing query visibility must only change allowed
    // keys, never remove/re-add unchanged documents in the shared tenant index.
    let first = service
        .search(&reader, &query("sharedterm", "public"))
        .unwrap();
    assert_eq!(first.index.status, "rebuilt");
    assert_eq!(first.total, 1);
    assert_eq!(
        service
            .extraction_status(&owner, "private/unparsed.pdf")
            .unwrap()
            .extraction
            .status,
        mnemosyne_content::extraction::ExtractionStatus::Pending
    );
    for (who, scope, extensions, count) in [
        (&owner, "public", vec![], 1),
        (&owner, "private", vec!["txt".into()], 1),
        (&reader, "public", vec!["md".into()], 1),
        (&owner, "", vec!["md".into(), "txt".into()], 2),
    ] {
        let mut request = query("sharedterm", scope);
        request.extensions = extensions;
        let found = service.search(who, &request).unwrap();
        assert_eq!(
            found.index.status, "current",
            "unchanged sources must not be reindexed"
        );
        assert_eq!(found.total, count);
    }
}

#[test]
fn typed_retrieval_controls_preserve_keyword_defaults_and_reject_provider_injection() {
    use mnemosyne_content::search::SearchMode;
    let request: mnemosyne_content::api::Request = serde_json::from_value(serde_json::json!({
        "version": 2, "operation": {"type": "search", "query": "исследование research"}
    }))
    .unwrap();
    match request.operation {
        mnemosyne_content::api::Operation::Search {
            mode, min_score, ..
        } => {
            assert_eq!(mode, SearchMode::Keyword);
            assert!(min_score.is_none());
        }
        _ => panic!("expected typed search"),
    }
    for mode in ["semantic", "hybrid"] {
        let request: mnemosyne_content::api::Request = serde_json::from_value(serde_json::json!({
            "version": 2, "operation": {"type": "search", "query": "meaning", "mode":mode, "min_score":0.7}
        })).unwrap();
        assert!(matches!(
            request.operation,
            mnemosyne_content::api::Operation::Search {
                min_score: Some(_),
                ..
            }
        ));
        for field in [
            "model_dir",
            "runtime_library",
            "worker",
            "provider",
            "endpoint",
        ] {
            let mut json = serde_json::json!({"version":2,"operation":{"type":"search","query":"meaning","mode":mode}});
            json["operation"][field] = serde_json::json!("attacker-controlled");
            assert!(
                serde_json::from_value::<mnemosyne_content::api::Request>(json).is_err(),
                "{field}"
            );
        }
    }
    assert!(
        serde_json::from_value::<mnemosyne_content::api::Request>(serde_json::json!({
            "version":2,"operation":{"type":"search","query":"meaning","mode":"remote"}
        }))
        .is_err()
    );
}

#[test]
fn every_retrieval_mode_checks_scope_and_delegation_before_provider_access() {
    use mnemosyne_content::search::SearchMode;
    let fixture = Fixture(std::env::temp_dir().join(format!(
        "mnemosyne-search-mode-auth-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )));
    let service = Service::open(&fixture.0).unwrap();
    let reader = actor("reader", ActorKind::Agent);
    service
        .bootstrap_policy(Policy {
            tenants: Default::default(),
            grants: vec![grant(&reader, Permission::Read, "public")],
        })
        .unwrap();
    for mode in [
        SearchMode::Keyword,
        SearchMode::Semantic,
        SearchMode::Hybrid,
    ] {
        let mut request = query("meaning смысл", "private");
        request.mode = mode;
        assert_eq!(
            service.search(&reader, &request).unwrap_err().code,
            "forbidden"
        );
        request.scope = "public".into();
        let mut delegated = reader.clone();
        delegated.on_behalf_of = Some("unprivileged-human".into());
        assert_eq!(
            service.search(&delegated, &request).unwrap_err().code,
            "forbidden"
        );
        request.min_score = Some(1.01);
        assert_eq!(
            service.search(&reader, &request).unwrap_err().code,
            "invalid_search"
        );
    }
    let mut keyword = query("valid", "public");
    keyword.min_score = Some(0.1);
    assert_eq!(
        service.search(&reader, &keyword).unwrap_err().code,
        "invalid_search"
    );
    keyword.min_score = None;
    let result = service.search(&reader, &keyword).unwrap();
    assert_eq!(result.total, 0);
    let json = serde_json::to_value(result).unwrap();
    assert!(json.get("retrieval").is_none());
}

#[test]
fn search_fields_name_boosts_and_binds_snapshot() {
    let fixture = Fixture(std::env::temp_dir().join(format!(
        "mnemosyne-fields-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )));
    let service = Service::open(&fixture.0).unwrap();
    let owner = actor("owner", ActorKind::Human);
    service
        .bootstrap_policy(Policy {
            tenants: Default::default(),
            grants: vec![
                grant(&owner, Permission::Read, ""),
                grant(&owner, Permission::Edit, ""),
            ],
        })
        .unwrap();
    service
        .edit(
            &owner,
            "r0",
            vec![
                // A heading, so the title is not the file name: no body hit at all.
                put("Warranty.md", "# Coverage\nWhat the plan pays for."),
                put("notes/a.md", "# Claims\nwarranty claims take ten days"),
                put(
                    "notes/b.md",
                    "# Repairs\nrepairs under warranty are free of charge",
                ),
                put("notes/warranties.md", "# Terms\nwarranty terms"),
            ],
            "seed",
            None,
            None,
        )
        .unwrap();
    let paths = |r: &mnemosyne_content::search::SearchResult| {
        r.hits.iter().map(|h| h.path.clone()).collect::<Vec<_>>()
    };
    let with = |fields: Vec<SearchField>| {
        let mut q = query("warranty", "");
        q.fields = fields;
        service.search(&owner, &q).unwrap()
    };

    // Default (body and name): the name hit ranks first; a name and body hit gains 2.0.
    let both = service.search(&owner, &query("warranty", "")).unwrap();
    assert_eq!(
        paths(&both),
        [
            "notes/warranties.md",
            "Warranty.md",
            "notes/a.md",
            "notes/b.md"
        ]
    );
    let body = with(vec![SearchField::Body]);
    assert_eq!(
        paths(&body),
        ["notes/warranties.md", "notes/a.md", "notes/b.md"]
    );
    let score = |r: &mnemosyne_content::search::SearchResult, p: &str| {
        r.hits.iter().find(|h| h.path == p).unwrap().score
    };
    assert_eq!(score(&both, "Warranty.md"), 2.0);
    assert_eq!(
        score(&both, "notes/warranties.md"),
        score(&body, "notes/warranties.md") + 2.0
    );
    assert_eq!(score(&both, "notes/a.md"), score(&body, "notes/a.md"));
    let name = with(vec![SearchField::Name]);
    assert_eq!(paths(&name), ["Warranty.md", "notes/warranties.md"]);
    assert!(name.hits.iter().all(|h| h.score == 2.0));
    // Field order and repeats do not matter; no fields is refused.
    assert_eq!(
        with(vec![
            SearchField::Name,
            SearchField::Body,
            SearchField::Name
        ])
        .snapshot_id,
        both.snapshot_id
    );
    let mut none = query("warranty", "");
    none.fields = vec![];
    assert_eq!(
        service.search(&owner, &none).unwrap_err().code,
        "invalid_search"
    );

    // The snapshot binds the fields.
    let mut page = query("warranty", "");
    page.limit = 1;
    let first = service.search(&owner, &page).unwrap();
    page.offset = 1;
    page.snapshot_id = Some(first.snapshot_id.clone());
    assert!(service.search(&owner, &page).is_ok());
    page.fields = vec![SearchField::Body];
    assert_eq!(
        service.search(&owner, &page).unwrap_err().code,
        "stale_search"
    );

    // JSON: `fields` defaults to both; unknown names are refused; v1 refuses search.
    let parse = |op: serde_json::Value| {
        serde_json::from_value::<mnemosyne_content::api::Request>(
            serde_json::json!({"version": 2, "operation": op}),
        )
    };
    let parsed = parse(serde_json::json!({"type":"search","query":"warranty"})).unwrap();
    assert!(matches!(
        parsed.operation,
        mnemosyne_content::api::Operation::Search { ref fields, .. }
            if *fields == SearchField::all()
    ));
    assert!(parse(serde_json::json!({"type":"search","query":"w","fields":["title"]})).is_err());
    let request: mnemosyne_content::api::Request = serde_json::from_value(serde_json::json!(
        {"version":1,"operation":{"type":"search","query":"warranty","fields":["name"]}}
    ))
    .unwrap();
    assert_eq!(
        service.execute(&owner, request).error.unwrap().code,
        "unsupported_version"
    );
    let r = service.execute(
        &owner,
        parse(serde_json::json!({"type":"search","query":"warranty","fields":["name"]})).unwrap(),
    );
    let v = serde_json::to_value(&r).unwrap();
    assert_eq!(v["result"]["total"], 2, "{v}");
}
