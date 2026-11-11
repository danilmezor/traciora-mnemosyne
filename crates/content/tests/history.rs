use mnemosyne_content::{
    auth::{ActorKind, Grant, Member, Permission, Policy, TenantPolicy, TrustedIdentity},
    onboarding::{ConflictPolicy, ImportFile, ImportMode},
    proposals::ProposalListRequest,
    Change, FileDiff, Origin, ProposalState, Revision, RevisionKind, Service,
};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

const AT: &str = "2026-10-28T22:00:00.000Z";

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(std::path::PathBuf);
impl Fixture {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "mnemosyne-history-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )))
    }
    fn service(&self) -> Service {
        Service::open_with_clock(&self.0, Box::new(|| AT.into())).unwrap()
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
fn member(role: &str, kind: ActorKind) -> Member {
    Member {
        role: role.into(),
        kind,
    }
}
fn put(path: &str, content: &str) -> Change {
    Change::Put {
        path: path.into(),
        content: content.into(),
    }
}
fn code<T: std::fmt::Debug>(r: Result<T, mnemosyne_content::ContentError>) -> String {
    r.unwrap_err().code
}
/// Tenant "t1" with root (administrator), a senior and a middle member.
fn fixture() -> (
    Fixture,
    Service,
    TrustedIdentity,
    TrustedIdentity,
    TrustedIdentity,
) {
    let fx = Fixture::new();
    let s = fx.service();
    let root = identity("t1", "root", ActorKind::Human);
    let senior = identity("t1", "senior-s", ActorKind::Human);
    let middle = identity("t1", "middle-m", ActorKind::Human);
    let section = TenantPolicy {
        members: BTreeMap::from([
            (
                "root".to_string(),
                member("administrator", ActorKind::Human),
            ),
            ("senior-s".to_string(), member("senior", ActorKind::Human)),
            ("middle-m".to_string(), member("middle", ActorKind::Human)),
            ("bot-b".to_string(), member("middle", ActorKind::Agent)),
        ]),
        ..TenantPolicy::default()
    };
    s.bootstrap_policy(Policy {
        grants: vec![
            Grant {
                tenant_id: "t1".into(),
                actor_id: "root".into(),
                permission: Permission::Admin,
                scope: "".into(),
            },
            Grant {
                tenant_id: "t1".into(),
                actor_id: "bot-b".into(),
                permission: Permission::Propose,
                scope: "".into(),
            },
            Grant {
                tenant_id: "t1".into(),
                actor_id: "bot-b".into(),
                permission: Permission::Read,
                scope: "".into(),
            },
        ],
        tenants: BTreeMap::from([("t1".to_string(), section)]),
    })
    .unwrap();
    (fx, s, root, senior, middle)
}
fn assistant() -> Origin {
    Origin {
        source: "assistant".into(),
        model: Some("sonnet-5.5".into()),
        chat_id: Some("c1".into()),
    }
}

#[test]
fn revisions_and_proposals_carry_time_message_reason_and_origin() {
    let (_fx, s, _root, senior, middle) = fixture();
    let r = s
        .edit(
            &senior,
            "r0",
            vec![put("warranty.md", "12 months")],
            "e1",
            Some("Fix warranty term"),
            Some(assistant()),
        )
        .unwrap();
    assert_eq!(r.at.as_deref(), Some(AT));
    assert_eq!(r.message.as_deref(), Some("Fix warranty term"));
    assert_eq!(r.origin, Some(assistant()));
    let h = s.history(&senior, "warranty.md").unwrap();
    assert_eq!(h.len(), 1);
    assert_eq!(h[0].at.as_deref(), Some(AT));
    assert_eq!(h[0].message.as_deref(), Some("Fix warranty term"));
    assert_eq!(h[0].origin, Some(assistant()));

    // Rejected proposal: reason on the proposal and on the decision.
    let p = s
        .propose(
            &middle,
            &r.id,
            vec![put("warranty.md", "24 months")],
            "p1",
            Some("Supplier changed the term"),
            None,
        )
        .unwrap();
    assert_eq!(p.at.as_deref(), Some(AT));
    assert_eq!(p.reason.as_deref(), Some("Supplier changed the term"));
    assert_eq!(p.origin, None);
    let rejected = s
        .reject(&senior, &p.id, &p.digest, "d1", Some("Wait for November"))
        .unwrap();
    assert_eq!(
        rejected.decisions[0].reason.as_deref(),
        Some("Wait for November")
    );
    assert_eq!(rejected.decisions[0].at.as_deref(), Some(AT));
    let reviewed = s.review(&senior, &p.id).unwrap();
    assert_eq!(
        reviewed.reason.as_deref(),
        Some("Supplier changed the term")
    );
    assert_eq!(
        reviewed.decisions[0].reason.as_deref(),
        Some("Wait for November")
    );

    // Approved proposal carrying an origin, then applied.
    let p2 = s
        .propose(
            &middle,
            &r.id,
            vec![put("warranty.md", "18 months")],
            "p2",
            Some("Compromise"),
            Some(Origin {
                source: "assistant".into(),
                model: None,
                chat_id: None,
            }),
        )
        .unwrap();
    assert_eq!(p2.origin.as_ref().unwrap().source, "assistant");
    let approved = s
        .approve(&senior, &p2.id, &p2.digest, "d2", Some("Agreed with legal"))
        .unwrap();
    assert_eq!(
        approved.decisions[0].reason.as_deref(),
        Some("Agreed with legal")
    );
    assert_eq!(approved.decisions[0].at.as_deref(), Some(AT));
    let applied = s.apply(&senior, &p2.id, &p2.digest, "a2").unwrap();
    assert_eq!(applied.at.as_deref(), Some(AT));

    // A decision without a reason serialises without the field.
    let p3 = s
        .propose(
            &middle,
            &applied.id,
            vec![put("x.md", "x")],
            "p3",
            None,
            None,
        )
        .unwrap();
    let plain = s.reject(&senior, &p3.id, &p3.digest, "d3", None).unwrap();
    let json = serde_json::to_value(&plain).unwrap();
    assert!(json.get("reason").is_none());
    assert!(json["decisions"][0].get("reason").is_none());
    assert_eq!(json["decisions"][0]["at"], AT);

    // An older record has none of the new fields.
    let old: Revision = serde_json::from_value(serde_json::json!({
        "id": "r1",
        "parent": null,
        "actor": {"tenant_id": "t1", "actor_id": "root", "kind": "human", "on_behalf_of": null},
        "proposal_id": null,
        "changes": []
    }))
    .unwrap();
    assert_eq!(old.at, None);
    assert_eq!(old.message, None);
    assert_eq!(old.origin, None);
    let json = serde_json::to_value(&old).unwrap();
    assert!(json.get("at").is_none() && json.get("message").is_none());
}

#[test]
fn system_clock_stamps_rfc3339_utc_milliseconds() {
    let fx = Fixture::new();
    let s = Service::open(&fx.0).unwrap();
    let root = identity("t1", "root", ActorKind::Human);
    s.bootstrap_policy(Policy {
        grants: vec![Grant {
            tenant_id: "t1".into(),
            actor_id: "root".into(),
            permission: Permission::Admin,
            scope: "".into(),
        }],
        tenants: BTreeMap::from([(
            "t1".to_string(),
            TenantPolicy {
                members: BTreeMap::from([(
                    "root".to_string(),
                    member("administrator", ActorKind::Human),
                )]),
                ..TenantPolicy::default()
            },
        )]),
    })
    .unwrap();
    let r = s
        .edit(&root, "r0", vec![put("a.md", "a")], "k", None, None)
        .unwrap();
    let at = r.at.unwrap();
    // e.g. 2026-10-28T22:00:00.120Z; exact widths are unit-tested on fixed inputs.
    assert_eq!(at.len(), 24, "{at}");
    assert!(at.ends_with('Z'), "{at}");
    // A retry with the same key and no message/origin replays the same revision.
    let again = s
        .edit(&root, "r0", vec![put("a.md", "a")], "k", None, None)
        .unwrap();
    assert_eq!(again.id, r.id);
}

#[test]
fn message_and_origin_are_validated() {
    let (_fx, s, _root, senior, middle) = fixture();
    let long = "a".repeat(2049);
    assert_eq!(
        code(s.edit(
            &senior,
            "r0",
            vec![put("a.md", "a")],
            "v1",
            Some(&long),
            None
        )),
        "invalid_message"
    );
    assert_eq!(
        code(s.edit(
            &senior,
            "r0",
            vec![put("a.md", "a")],
            "v2",
            Some("a\x07b"),
            None
        )),
        "invalid_message"
    );
    assert_eq!(
        code(s.propose(
            &middle,
            "r0",
            vec![put("a.md", "a")],
            "v3",
            Some(&long),
            None
        )),
        "invalid_message"
    );
    let r = s
        .edit(
            &senior,
            "r0",
            vec![put("a.md", "a")],
            "ok",
            Some(&format!("line one\nline two{}", "a".repeat(2048 - 17))),
            None,
        )
        .unwrap();
    assert_eq!(r.message.unwrap().len(), 2048);
    let human = Origin {
        source: "human".into(),
        model: None,
        chat_id: None,
    };
    assert_eq!(
        code(s.edit(
            &senior,
            &r.id,
            vec![put("b.md", "b")],
            "v4",
            None,
            Some(human)
        )),
        "invalid_origin"
    );
    let big_model = Origin {
        source: "assistant".into(),
        model: Some("m".repeat(129)),
        chat_id: None,
    };
    assert_eq!(
        code(s.propose(
            &middle,
            &r.id,
            vec![put("b.md", "b")],
            "v5",
            None,
            Some(big_model)
        )),
        "invalid_origin"
    );
    let big_chat = Origin {
        source: "assistant".into(),
        model: None,
        chat_id: Some("c".repeat(257)),
    };
    assert_eq!(
        code(s.edit(
            &senior,
            &r.id,
            vec![put("b.md", "b")],
            "v6",
            None,
            Some(big_chat)
        )),
        "invalid_origin"
    );
    let p = s
        .propose(&middle, &r.id, vec![put("b.md", "b")], "p", None, None)
        .unwrap();
    assert_eq!(
        code(s.reject(&senior, &p.id, &p.digest, "v7", Some("a\rb"))),
        "invalid_message"
    );

    // Replays fingerprint the new fields.
    s.reject(&senior, &p.id, &p.digest, "d", Some("no"))
        .unwrap();
    assert_eq!(
        code(s.reject(&senior, &p.id, &p.digest, "d", Some("nope"))),
        "idempotency_conflict"
    );
    let e = s
        .edit(
            &senior,
            &r.id,
            vec![put("c.md", "c")],
            "same",
            Some("one"),
            None,
        )
        .unwrap();
    let again = s
        .edit(
            &senior,
            &r.id,
            vec![put("c.md", "c")],
            "same",
            Some("one"),
            None,
        )
        .unwrap();
    assert_eq!(e.id, again.id);
    assert_eq!(
        code(s.edit(
            &senior,
            &r.id,
            vec![put("c.md", "c")],
            "same",
            Some("two"),
            None
        )),
        "idempotency_conflict"
    );
    assert_eq!(
        code(s.edit(
            &senior,
            &r.id,
            vec![put("c.md", "c")],
            "same",
            Some("one"),
            Some(assistant())
        )),
        "idempotency_conflict"
    );
    assert_eq!(
        code(s.propose(
            &middle,
            &r.id,
            vec![put("b.md", "b")],
            "p",
            Some("why"),
            None
        )),
        "idempotency_conflict"
    );
}

#[test]
fn api_carries_new_fields_on_v2_and_refuses_them_on_v1() {
    use mnemosyne_content::api::{ApiResult, Request};
    let (_fx, s, _root, senior, _middle) = fixture();
    let request = |version: u32, op: serde_json::Value| -> Request {
        serde_json::from_value(serde_json::json!({
            "version": version,
            "idempotency_key": format!("k{version}"),
            "operation": op,
        }))
        .unwrap()
    };
    let op = serde_json::json!({
        "type": "edit",
        "base_revision": "r0",
        "changes": [{"type": "put", "path": "a.md", "content": "a"}],
        "message": "Why",
        "origin": {"source": "assistant", "model": "sonnet-5.5"}
    });
    let v1 = s.execute(&senior, request(1, op.clone()));
    assert_eq!(v1.error.unwrap().code, "unsupported_version");
    let v2 = s.execute(&senior, request(2, op));
    match v2.result {
        Some(ApiResult::Revision(r)) => {
            assert_eq!(r.message.as_deref(), Some("Why"));
            assert_eq!(r.origin.unwrap().model.as_deref(), Some("sonnet-5.5"));
            assert_eq!(r.at.as_deref(), Some(AT));
        }
        other => panic!("{other:?}"),
    }
    let plain = serde_json::json!({
        "type": "edit",
        "base_revision": s.head(&senior).unwrap(),
        "changes": [{"type": "put", "path": "b.md", "content": "b"}],
    });
    assert!(s.execute(&senior, request(1, plain)).error.is_none());
    let unknown = serde_json::json!({
        "type": "edit",
        "base_revision": "r0",
        "changes": [],
        "origin": {"source": "assistant", "colour": "red"}
    });
    assert!(serde_json::from_value::<Request>(serde_json::json!({
        "version": 2, "operation": unknown
    }))
    .is_err());
}

#[test]
fn withdraw_is_the_proposers_own_decline() {
    let (_fx, s, _root, senior, middle) = fixture();
    let r = s
        .edit(&senior, "r0", vec![put("w.md", "12")], "e1", None, None)
        .unwrap();
    let p = s
        .propose(&middle, &r.id, vec![put("w.md", "24")], "p1", None, None)
        .unwrap();
    assert_eq!(
        code(s.withdraw(&senior, &p.id, &p.digest, "w0")),
        "not_proposer"
    );
    assert_eq!(
        code(s.withdraw(&middle, &p.id, "bad", "w0")),
        "proposal_mismatch"
    );
    let w = s.withdraw(&middle, &p.id, &p.digest, "w1").unwrap();
    assert_eq!(w.state, ProposalState::Withdrawn);
    assert!(w.approver.is_none());
    let d = w.decisions.last().unwrap();
    assert_eq!(d.actor, middle);
    assert!(!d.approved);
    assert_eq!(d.reason.as_deref(), Some("Withdrawn"));
    assert_eq!(d.at.as_deref(), Some(AT));
    // Replay returns the same record; a fresh key on a withdrawn proposal is refused.
    let again = s.withdraw(&middle, &p.id, &p.digest, "w1").unwrap();
    assert_eq!(again.decisions.len(), w.decisions.len());
    assert_eq!(again.state, ProposalState::Withdrawn);
    assert_eq!(
        code(s.withdraw(&middle, &p.id, &p.digest, "w2")),
        "invalid_review_state"
    );
    assert_eq!(
        code(s.apply(&senior, &p.id, &p.digest, "a1")),
        "invalid_review_state"
    );
    assert_eq!(
        code(s.approve(&senior, &p.id, &p.digest, "d1", None)),
        "invalid_review_state"
    );
    let listed = s
        .list_proposals(
            &senior,
            &ProposalListRequest {
                states: vec![ProposalState::Withdrawn],
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(listed.proposals.len(), 1);
    assert_eq!(listed.proposals[0].id, p.id);

    // An approved proposal may still be withdrawn.
    let p2 = s
        .propose(&middle, &r.id, vec![put("w.md", "36")], "p2", None, None)
        .unwrap();
    s.approve(&senior, &p2.id, &p2.digest, "d2", None).unwrap();
    let w2 = s.withdraw(&middle, &p2.id, &p2.digest, "w3").unwrap();
    assert_eq!(w2.state, ProposalState::Withdrawn);
    assert!(w2.approver.is_none());

    // A bot's proposal may be withdrawn by its on_behalf_of human.
    let bot = TrustedIdentity {
        on_behalf_of: Some("middle-m".into()),
        ..identity("t1", "bot-b", ActorKind::Agent)
    };
    let p3 = s
        .propose(&bot, &r.id, vec![put("w.md", "48")], "p3", None, None)
        .unwrap();
    assert_eq!(
        code(s.withdraw(&senior, &p3.id, &p3.digest, "w4")),
        "not_proposer"
    );
    let w3 = s.withdraw(&middle, &p3.id, &p3.digest, "w5").unwrap();
    assert_eq!(w3.state, ProposalState::Withdrawn);
    assert_eq!(w3.decisions.last().unwrap().actor, middle);
}

fn diff<'a>(r: &'a Revision, path: &str) -> &'a FileDiff {
    r.changes.iter().find(|d| d.path == path).unwrap()
}

#[test]
fn publish_records_line_counts_and_kind() {
    let (_fx, s, root, senior, middle) = fixture();
    let r1 = s
        .edit(&senior, "r0", vec![put("a.md", "a\nb\n")], "e1", None, None)
        .unwrap();
    assert_eq!(r1.kind, Some(RevisionKind::Edit));
    assert_eq!(diff(&r1, "a.md").added_lines, Some(2));
    assert_eq!(diff(&r1, "a.md").removed_lines, Some(0));
    let r2 = s
        .edit(
            &senior,
            &r1.id,
            vec![put("a.md", "a\nc\nd\n")],
            "e2",
            None,
            None,
        )
        .unwrap();
    assert_eq!(diff(&r2, "a.md").added_lines, Some(2));
    assert_eq!(diff(&r2, "a.md").removed_lines, Some(1));
    // CRLF versus LF, and a trailing newline alone, are not changes.
    let r3 = s
        .edit(
            &senior,
            &r2.id,
            vec![put("a.md", "a\r\nc\r\nd")],
            "e3",
            None,
            None,
        )
        .unwrap();
    assert_eq!(diff(&r3, "a.md").added_lines, Some(0));
    assert_eq!(diff(&r3, "a.md").removed_lines, Some(0));
    // Deleted: removed = line count.
    let r4 = s
        .edit(
            &senior,
            &r3.id,
            vec![Change::Delete {
                path: "a.md".into(),
            }],
            "e4",
            None,
            None,
        )
        .unwrap();
    assert_eq!(diff(&r4, "a.md").added_lines, Some(0));
    assert_eq!(diff(&r4, "a.md").removed_lines, Some(3));

    let r5 = s
        .edit(
            &senior,
            &r4.id,
            vec![
                put("b.md", "x\n"),
                Change::CreateFolder { path: "dir".into() },
            ],
            "e5",
            None,
            None,
        )
        .unwrap();
    assert_eq!(r5.kind, Some(RevisionKind::Edit));
    assert_eq!(diff(&r5, "dir").added_lines, None);
    assert_eq!(diff(&r5, "dir").removed_lines, None);

    let r6 = s
        .edit(
            &senior,
            &r5.id,
            vec![Change::Rename {
                path: "b.md".into(),
                to: "c.md".into(),
            }],
            "e6",
            None,
            None,
        )
        .unwrap();
    assert_eq!(r6.kind, Some(RevisionKind::Rename));
    assert_eq!(diff(&r6, "c.md").added_lines, None);
    assert_eq!(diff(&r6, "c.md").removed_lines, None);

    let r7 = s
        .edit(
            &root,
            &r6.id,
            vec![Change::SetMeta {
                path: "c.md".into(),
                seen_from: None,
                colour: Some(mnemosyne_content::ranks::COLOURS[0].into()),
            }],
            "e7",
            None,
            None,
        )
        .unwrap();
    assert_eq!(r7.kind, Some(RevisionKind::Meta));
    assert_eq!(diff(&r7, "c.md").added_lines, None);
    assert_eq!(diff(&r7, "c.md").removed_lines, None);

    let r8 = s
        .edit(
            &senior,
            &r7.id,
            vec![Change::Restore {
                path: "c.md".into(),
                revision_id: r6.id.clone(),
            }],
            "e8",
            None,
            None,
        )
        .unwrap();
    assert_eq!(r8.kind, Some(RevisionKind::Restore));
    // r7 (and r8 before it) only changed meta/names; restore real content to see counts.
    let r8b = s
        .edit(
            &senior,
            &r8.id,
            vec![put("c.md", "new\n")],
            "e8b",
            None,
            None,
        )
        .unwrap();
    let r8c = s
        .edit(
            &senior,
            &r8b.id,
            vec![Change::Restore {
                path: "c.md".into(),
                revision_id: r6.id.clone(),
            }],
            "e8c",
            None,
            None,
        )
        .unwrap();
    assert_eq!(r8c.kind, Some(RevisionKind::Restore));
    assert_eq!(diff(&r8c, "c.md").added_lines, Some(1));
    assert_eq!(diff(&r8c, "c.md").removed_lines, Some(1));
    let r8 = r8c;

    let bin = |b: &[u8]| Change::PutBinary {
        path: "f.bin".into(),
        bytes: b.to_vec(),
    };
    let r9 = s
        .edit(&senior, &r8.id, vec![bin(b"\x00\x01")], "e9", None, None)
        .unwrap();
    assert_eq!(
        r9.kind,
        Some(RevisionKind::Edit),
        "a new binary is not a new version"
    );
    assert_eq!(diff(&r9, "f.bin").added_lines, None);
    let r10 = s
        .edit(&senior, &r9.id, vec![bin(b"\x00\x02")], "e10", None, None)
        .unwrap();
    assert_eq!(r10.kind, Some(RevisionKind::NewVersion));
    assert_eq!(diff(&r10, "f.bin").removed_lines, None);

    // Mixed changes are plain edits.
    let r11 = s
        .edit(
            &senior,
            &r10.id,
            vec![
                put("m.md", "m"),
                Change::Rename {
                    path: "c.md".into(),
                    to: "d.md".into(),
                },
            ],
            "e11",
            None,
            None,
        )
        .unwrap();
    assert_eq!(r11.kind, Some(RevisionKind::Edit));

    // A proposal applies as a Proposal revision.
    let p = s
        .propose(
            &middle,
            &r11.id,
            vec![put("m.md", "m\nn\n")],
            "p1",
            None,
            None,
        )
        .unwrap();
    s.approve(&senior, &p.id, &p.digest, "d1", None).unwrap();
    let r12 = s.apply(&senior, &p.id, &p.digest, "a1").unwrap();
    assert_eq!(r12.kind, Some(RevisionKind::Proposal));
    assert_eq!(diff(&r12, "m.md").added_lines, Some(1));
    assert_eq!(diff(&r12, "m.md").removed_lines, Some(0));

    // Import through edit mode.
    let files = vec![ImportFile {
        path: "imp.md".into(),
        bytes: b"i\n".to_vec(),
        directory: false,
    }];
    let prev = s
        .import_preview(&senior, &r12.id, files, 0, None, ConflictPolicy::Error)
        .unwrap();
    let mnemosyne_content::api::ApiResult::Revision(ri) = s
        .import_commit(&senior, prev, ImportMode::Edit, "i1", None, None)
        .unwrap()
    else {
        panic!("expected a revision")
    };
    assert_eq!(ri.kind, Some(RevisionKind::Import));
}

#[test]
fn deciders_are_empty_for_proposals_that_have_ended() {
    let (_fx, s, _root, senior, middle) = fixture();
    let r = s
        .edit(&senior, "r0", vec![put("w.md", "12")], "e1", None, None)
        .unwrap();
    let p = s
        .propose(&middle, &r.id, vec![put("w.md", "24")], "p1", None, None)
        .unwrap();
    assert!(!s.deciders(&senior, &p.id).unwrap().deciders.is_empty());
    s.withdraw(&middle, &p.id, &p.digest, "w1").unwrap();
    let d = s.deciders(&senior, &p.id).unwrap();
    assert!(d.deciders.is_empty() && !d.fallback);
    let p2 = s
        .propose(&middle, &r.id, vec![put("w.md", "36")], "p2", None, None)
        .unwrap();
    s.reject(&senior, &p2.id, &p2.digest, "d2", None).unwrap();
    let d = s.deciders(&senior, &p2.id).unwrap();
    assert!(d.deciders.is_empty() && !d.fallback);
}

fn folder(path: &str) -> Change {
    Change::CreateFolder { path: path.into() }
}
fn hide_meta(path: &str, role: &str) -> Change {
    Change::SetMeta {
        path: path.into(),
        seen_from: Some(role.into()),
        colour: None,
    }
}
fn ids(r: &mnemosyne_content::log::LogResult) -> Vec<&str> {
    r.entries.iter().map(|e| e.id.as_str()).collect()
}

#[test]
fn log_pages_what_the_caller_may_see() {
    let (_fx, s, root, senior, middle) = fixture();
    let r1 = s
        .edit(
            &root,
            "r0",
            vec![put("a.md", "one\n"), folder("senior-only")],
            "l1",
            Some("first"),
            None,
        )
        .unwrap();
    // Touches only senior-only/: hidden from middle now and (once hidden) then.
    let r2 = s
        .edit(
            &root,
            &r1.id,
            vec![hide_meta("senior-only", "senior")],
            "l2",
            None,
            None,
        )
        .unwrap();
    // Through a proposal: middle proposes, senior approves and applies.
    let p = s
        .propose(
            &middle,
            &r2.id,
            vec![put("a.md", "one\ntwo\n")],
            "l3p",
            Some("add two"),
            Some(assistant()),
        )
        .unwrap();
    let p = s.approve(&senior, &p.id, &p.digest, "l3a", None).unwrap();
    let r3 = s.apply(&senior, &p.id, &p.digest, "l3").unwrap();
    // Touches both: one visible text file and one hidden one.
    let r4 = s
        .edit(
            &root,
            &r3.id,
            vec![
                put("b.md", "b\n"),
                put("senior-only/pay.md", "pay\nmore\nlines\n"),
            ],
            "l4",
            None,
            None,
        )
        .unwrap();
    let r5 = s
        .edit(
            &root,
            &r4.id,
            vec![put("a.md", "three\n")],
            "l5",
            None,
            None,
        )
        .unwrap();

    let all = s.log(&middle, "", 20, 0).unwrap();
    assert_eq!(all.revision_id, r5.id);
    assert_eq!(ids(&all), vec![&*r5.id, &*r4.id, &*r3.id, &*r1.id]);
    assert_eq!(all.next_offset, None);
    let e5 = &all.entries[0];
    assert_eq!(e5.paths, vec!["a.md".to_string()]);
    assert_eq!((e5.path_count, e5.hidden_paths), (1, 0));
    assert_eq!((e5.added_lines, e5.removed_lines), (Some(1), Some(2)));
    assert_eq!(e5.actor, root);
    assert_eq!(e5.at.as_deref(), Some(AT));
    assert_eq!(e5.parent.as_deref(), Some(&*r4.id));
    assert_eq!(e5.kind, Some(RevisionKind::Edit));
    let mixed = &all.entries[1];
    assert_eq!(mixed.paths, vec!["b.md".to_string()]);
    assert_eq!((mixed.path_count, mixed.hidden_paths), (1, 1));
    assert_eq!((mixed.added_lines, mixed.removed_lines), (Some(1), Some(0)));
    let e3 = &all.entries[2];
    assert_eq!(e3.proposal_id.as_deref(), Some(&*p.id));
    assert_eq!(e3.proposer.as_ref(), Some(&middle));
    assert_eq!(e3.approver.as_ref(), Some(&senior));
    assert_eq!(e3.kind, Some(RevisionKind::Proposal));
    let e1 = &all.entries[3];
    assert_eq!(e1.message.as_deref(), Some("first"));
    assert_eq!(e1.paths, vec!["a.md".to_string()]);
    assert_eq!((e1.path_count, e1.hidden_paths), (1, 1));
    assert_eq!((e1.added_lines, e1.removed_lines), (Some(1), Some(0)));
    assert_eq!((e1.proposer.as_ref(), e1.approver.as_ref()), (None, None));
    assert_eq!(e1.parent, None);

    let page = s.log(&middle, "", 2, 0).unwrap();
    assert_eq!(ids(&page), vec![&*r5.id, &*r4.id]);
    assert_eq!(page.next_offset, Some(2));
    let page = s.log(&middle, "", 2, 2).unwrap();
    assert_eq!(ids(&page), vec![&*r3.id, &*r1.id]);
    assert_eq!(page.next_offset, None);
    let page = s.log(&middle, "", 2, 9).unwrap();
    assert!(page.entries.is_empty());
    assert_eq!(page.next_offset, None);

    // A path scope lists only revisions touching it.
    let a = s.log(&middle, "a.md", 20, 0).unwrap();
    assert_eq!(ids(&a), vec![&*r5.id, &*r3.id, &*r1.id]);

    let seen = s.log(&senior, "", 20, 0).unwrap();
    assert_eq!(seen.entries.len(), 5);
    let mixed = &seen.entries[1];
    assert_eq!(
        mixed.paths,
        vec!["b.md".to_string(), "senior-only/pay.md".to_string()]
    );
    assert_eq!((mixed.path_count, mixed.hidden_paths), (2, 0));
    assert_eq!((mixed.added_lines, mixed.removed_lines), (Some(4), Some(0)));
    let only = &seen.entries[3];
    assert_eq!(only.id, r2.id);
    assert_eq!(only.paths, vec!["senior-only".to_string()]);
    assert_eq!((only.added_lines, only.removed_lines), (None, None));
    let hidden = s.log(&senior, "senior-only", 20, 0).unwrap();
    assert_eq!(ids(&hidden), vec![&*r4.id, &*r2.id, &*r1.id]);
    assert_eq!(
        hidden.entries[0].paths,
        vec!["senior-only/pay.md".to_string()]
    );

    assert_eq!(code(s.log(&middle, "senior-only", 20, 0)), "not_found");
    assert_eq!(
        code(s.log(&middle, "senior-only/pay.md", 20, 0)),
        "not_found"
    );
    assert_eq!(code(s.log(&middle, "a//b", 20, 0)), "invalid_path");
    assert_eq!(code(s.log(&middle, "/a.md", 20, 0)), "invalid_path");
}

#[test]
fn log_window_of_only_hidden_revisions_keeps_offsets_honest() {
    let (_fx, s, root, senior, middle) = fixture();
    let first = s
        .edit(
            &root,
            "r0",
            vec![put("a.md", "a\n"), folder("senior-only")],
            "w1",
            None,
            None,
        )
        .unwrap();
    let mut head = s
        .edit(
            &root,
            &first.id,
            vec![hide_meta("senior-only", "senior")],
            "w2",
            None,
            None,
        )
        .unwrap()
        .id;
    for i in 0..4 {
        head = s
            .edit(
                &root,
                &head,
                vec![put("senior-only/pay.md", &format!("{i}\n"))],
                &format!("w3-{i}"),
                None,
                None,
            )
            .unwrap()
            .id;
    }
    let last = s
        .edit(&root, &head, vec![put("a.md", "b\n")], "w4", None, None)
        .unwrap();

    // Between the two visible revisions sit five hidden ones; none of them counts.
    let page = s.log(&middle, "", 1, 0).unwrap();
    assert_eq!(ids(&page), vec![&*last.id]);
    assert_eq!(page.next_offset, Some(1));
    assert_eq!(page.entries[0].hidden_paths, 0);
    let page = s.log(&middle, "", 1, 1).unwrap();
    assert_eq!(ids(&page), vec![&*first.id]);
    assert_eq!(page.next_offset, None);
    let page = s.log(&middle, "", 3, 1).unwrap();
    assert_eq!(ids(&page), vec![&*first.id]);
    assert_eq!(page.next_offset, None);
    let page = s.log(&middle, "", 3, 2).unwrap();
    assert!(page.entries.is_empty());
    assert_eq!(page.next_offset, None);
    // The hidden revisions are there for a caller who may see them.
    assert_eq!(s.log(&senior, "", 100, 0).unwrap().entries.len(), 7);
}

#[test]
fn log_api_is_v2_with_bounded_limit() {
    use mnemosyne_content::api::{ApiResult, Operation, Request};
    let (_fx, s, root, _senior, middle) = fixture();
    s.edit(&root, "r0", vec![put("a.md", "a\n")], "v1", None, None)
        .unwrap();
    let op = |v: serde_json::Value| -> Operation { serde_json::from_value(v).unwrap() };
    let run = |version, operation| {
        s.execute(
            &middle,
            Request {
                version,
                idempotency_key: None,
                operation,
            },
        )
    };
    let r = run(2, op(serde_json::json!({"type": "log"})));
    match r.result {
        Some(ApiResult::Log(l)) => {
            assert_eq!(l.entries.len(), 1);
            assert_eq!(l.next_offset, None);
        }
        other => panic!("{other:?} {:?}", r.error),
    }
    let r = run(
        2,
        op(serde_json::json!({"type": "log", "scope": "a.md", "limit": 1, "offset": 0})),
    );
    assert!(matches!(r.result, Some(ApiResult::Log(_))));
    for limit in [0, 101] {
        let r = run(2, op(serde_json::json!({"type": "log", "limit": limit})));
        assert_eq!(r.error.unwrap().code, "invalid_request");
    }
    let r = run(1, op(serde_json::json!({"type": "log"})));
    assert_eq!(r.error.unwrap().code, "unsupported_version");
}

#[test]
fn log_keeps_what_was_hidden_then_hidden() {
    let (_fx, s, root, senior, middle) = fixture();
    let first = s
        .edit(
            &root,
            "r0",
            vec![put("a.md", "a\n"), folder("vault")],
            "h1",
            None,
            None,
        )
        .unwrap();
    let mut head = s
        .edit(
            &root,
            &first.id,
            vec![hide_meta("vault", "senior")],
            "h2",
            None,
            None,
        )
        .unwrap()
        .id;
    for i in 0..3 {
        head = s
            .edit(
                &root,
                &head,
                vec![put("vault/x.md", &format!("{i}\n"))],
                &format!("h3-{i}"),
                None,
                None,
            )
            .unwrap()
            .id;
    }
    // Opened to everyone: readable now, but the edits made while hidden stay hidden.
    let opened = s
        .edit(
            &root,
            &head,
            vec![hide_meta("vault", "junior")],
            "h4",
            None,
            None,
        )
        .unwrap();
    let last = s
        .edit(
            &root,
            &opened.id,
            vec![put("vault/x.md", "open\n")],
            "h5",
            None,
            None,
        )
        .unwrap();
    let all = s.log(&middle, "", 20, 0).unwrap();
    assert_eq!(ids(&all), vec![&*last.id, &*first.id]);
    assert_eq!(
        all.entries[1].paths,
        vec!["a.md".to_string(), "vault".to_string()]
    );
    assert_eq!(all.entries[1].hidden_paths, 0);
    let page = s.log(&middle, "vault", 1, 0).unwrap();
    assert_eq!(ids(&page), vec![&*last.id]);
    assert_eq!(page.next_offset, Some(1));
    let page = s.log(&middle, "vault", 1, 1).unwrap();
    assert_eq!(ids(&page), vec![&*first.id]);
    assert_eq!(page.entries[0].paths, vec!["vault".to_string()]);
    assert_eq!(page.next_offset, None);
    assert_eq!(s.log(&senior, "", 100, 0).unwrap().entries.len(), 7);
}

// ---- render ----

use mnemosyne_content::render::{FileRender, ParagraphKind, RenderResult, SpanKind};

fn kinds(f: &FileRender) -> Vec<ParagraphKind> {
    f.paragraphs.iter().map(|p| p.kind).collect()
}
fn only<'a>(r: &'a RenderResult, path: &str) -> &'a FileRender {
    r.files.iter().find(|f| f.path == path).unwrap()
}
fn edit(s: &Service, who: &TrustedIdentity, base: &str, changes: Vec<Change>, key: &str) -> String {
    s.edit(who, base, changes, key, None, None).unwrap().id
}

const WARRANTY: &str = "# Warranty\n\nThe warranty lasts two years.\n\nReturns are accepted within 30 days.\nKeep the receipt.\n\nContact support for repairs.\n";
const WARRANTY_NEW: &str = "# Warranty\n\nThe warranty lasts three years. It covers parts and labour.\n\nReturns are accepted within 30 days.\nKeep the receipt.\n\nContact support for repairs.\n";

#[test]
fn render_turns_a_change_into_reading() {
    use mnemosyne_content::api::{ApiResult, Operation, Request};
    let (_fx, s, root, senior, middle) = fixture();
    let r1 = edit(&s, &root, "r0", vec![put("warranty.md", WARRANTY)], "w1");
    let r2 = edit(&s, &root, &r1, vec![put("warranty.md", WARRANTY_NEW)], "w2");

    let r = s.render(&middle, Some(&r2), None, None, 0).unwrap();
    assert_eq!(r.revision_id, r2);
    assert_eq!(
        (r.stale, r.head_revision.as_deref(), r.hidden_paths),
        (false, None, 0)
    );
    assert_eq!(r.proposal_id, None);
    let f = only(&r, "warranty.md");
    assert_eq!(f.kind, "text");
    assert_eq!((f.moved_from.as_deref(), f.meta.is_some()), (None, false));
    assert_eq!((f.summary.added_lines, f.summary.removed_lines), (1, 1));
    assert_eq!(f.summary.paragraphs_changed, 1);
    assert!(!f.summary.rewritten);
    assert_eq!(f.next_offset, None);
    assert_eq!(
        kinds(f),
        vec![
            ParagraphKind::Same,
            ParagraphKind::Changed,
            ParagraphKind::Same
        ]
    );
    // Unchanged runs collapse to their line count: the heading, then two paragraphs.
    assert_eq!((f.paragraphs[0].index, f.paragraphs[0].lines), (0, Some(1)));
    assert_eq!((f.paragraphs[2].index, f.paragraphs[2].lines), (2, Some(3)));
    assert!(f.paragraphs[0].spans.is_empty());
    let changed = &f.paragraphs[1];
    assert_eq!((changed.index, changed.lines), (1, None));
    let spans: Vec<(SpanKind, &str)> = changed
        .spans
        .iter()
        .map(|s| (s.kind, s.text.as_str()))
        .collect();
    assert_eq!(
        spans,
        vec![
            (SpanKind::Same, "The warranty lasts "),
            (SpanKind::Removed, "two"),
            (SpanKind::Added, "three"),
            (SpanKind::Same, " years."),
            (SpanKind::Added, " It covers parts and labour."),
        ]
    );
    assert_eq!((changed.fits, changed.conflict), (None, None));

    // The same file at a path; the first revision renders as a created file.
    let one = s
        .render(&middle, Some(&r2), None, Some("warranty.md"), 0)
        .unwrap();
    assert_eq!(one.files.len(), 1);
    let first = s.render(&middle, Some(&r1), None, None, 0).unwrap();
    let created = only(&first, "warranty.md");
    assert_eq!(created.kind, "created");
    assert_eq!(created.paragraphs.len(), 4);
    assert!(kinds(created).iter().all(|k| *k == ParagraphKind::Added));
    assert_eq!(created.paragraphs[0].spans[0].text, "# Warranty");
    assert_eq!(
        (created.summary.added_lines, created.summary.removed_lines),
        (8, 0)
    );
    assert!(!created.summary.rewritten);

    // A pending proposal at head renders base → proposed, not stale, without fits.
    let p = s
        .propose(
            &middle,
            &r2,
            vec![put("warranty.md", WARRANTY)],
            "wp",
            None,
            None,
        )
        .unwrap();
    let pr = s.render(&senior, None, Some(&p.id), None, 0).unwrap();
    assert_eq!(pr.proposal_id.as_deref(), Some(&*p.id));
    assert_eq!(pr.revision_id, r2);
    assert_eq!((pr.stale, pr.head_revision.as_deref()), (false, None));
    let pf = only(&pr, "warranty.md");
    assert_eq!(pf.summary.paragraphs_changed, 1);
    assert!(pf
        .paragraphs
        .iter()
        .all(|p| p.fits.is_none() && p.conflict.is_none()));

    // Exactly one id; unknown ids are not found.
    assert_eq!(
        code(s.render(&middle, None, None, None, 0)),
        "invalid_request"
    );
    assert_eq!(
        code(s.render(&middle, Some(&r2), Some(&p.id), None, 0)),
        "invalid_request"
    );
    assert_eq!(
        code(s.render(&middle, Some("r999"), None, None, 0)),
        "not_found"
    );
    assert_eq!(
        code(s.render(&middle, None, Some("p404"), None, 0)),
        "not_found"
    );
    assert_eq!(
        code(s.render(&middle, Some(&r2), None, Some("other.md"), 0)),
        "not_found"
    );
    assert_eq!(
        code(s.render(&middle, Some(&r2), None, Some("/x"), 0)),
        "invalid_path"
    );

    // The API: v2 only, JSON shape as documented.
    let op = |v: serde_json::Value| -> Operation { serde_json::from_value(v).unwrap() };
    let run = |version, operation| {
        s.execute(
            &middle,
            Request {
                version,
                idempotency_key: None,
                operation,
            },
        )
    };
    let resp = run(
        2,
        op(serde_json::json!({"type": "render", "revision_id": r2})),
    );
    let Some(ApiResult::Render(api)) = &resp.result else {
        panic!("{:?}", resp.error)
    };
    let json = serde_json::to_value(api).unwrap();
    let file = &json["files"][0];
    assert_eq!(file["kind"], "text");
    assert_eq!(
        file["paragraphs"][0],
        serde_json::json!({"index": 0, "kind": "same", "lines": 1})
    );
    assert_eq!(file["paragraphs"][1]["kind"], "changed");
    assert_eq!(
        file["paragraphs"][1]["spans"][1],
        serde_json::json!({"kind": "removed", "text": "two"})
    );
    assert!(file["paragraphs"][1].get("fits").is_none());
    assert!(json.get("proposal_id").is_none() && json.get("head_revision").is_none());
    assert_eq!(json["stale"], false);
    // A serialised result reads back as a render result.
    let back: mnemosyne_content::api::Response =
        serde_json::from_value(serde_json::to_value(&resp).unwrap()).unwrap();
    assert!(matches!(back.result, Some(ApiResult::Render(_))));
    let resp = run(
        2,
        op(
            serde_json::json!({"type": "render", "proposal_id": p.id, "path": "warranty.md", "offset": 0}),
        ),
    );
    assert!(
        matches!(resp.result, Some(ApiResult::Render(_))),
        "{:?}",
        resp.error
    );
    let resp = run(2, op(serde_json::json!({"type": "render"})));
    assert_eq!(resp.error.unwrap().code, "invalid_request");
    let resp = run(
        1,
        op(serde_json::json!({"type": "render", "revision_id": r2})),
    );
    assert_eq!(resp.error.unwrap().code, "unsupported_version");
}

#[test]
fn render_flags_rewrites_and_folds_long_changes() {
    let (_fx, s, root, _senior, middle) = fixture();
    // At least 60 % of the lines changed: a rewrite, counts only.
    let r1 = edit(&s, &root, "r0", vec![put("rw.md", "a\nb\nc\nd\ne\n")], "f1");
    let r2 = edit(&s, &root, &r1, vec![put("rw.md", "v\nw\nx\ny\ne\n")], "f2");
    let r = s.render(&middle, Some(&r2), None, None, 0).unwrap();
    let f = only(&r, "rw.md");
    assert!(f.summary.rewritten);
    assert_eq!((f.summary.added_lines, f.summary.removed_lines), (4, 4));
    assert_eq!(f.summary.paragraphs_changed, 1);
    assert!(f.paragraphs.is_empty());
    assert_eq!(f.next_offset, None);
    // Just under the threshold is not a rewrite.
    let r3 = edit(&s, &root, &r2, vec![put("rw.md", "v\nw\nx\ny\nz\n")], "f3");
    let f = s
        .render(&middle, Some(&r3), None, None, 0)
        .unwrap()
        .files
        .remove(0);
    assert_eq!((f.summary.added_lines, f.summary.removed_lines), (1, 1));
    assert!(!f.summary.rewritten);
    assert!(!f.paragraphs.is_empty());

    // Sixty paragraphs, every second one changed: thirty changed paragraphs, 12 a page.
    let text = |word: &str| {
        (0..60)
            .map(|i| {
                if i % 2 == 0 {
                    format!("keep {i}")
                } else {
                    format!("{word} {i}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    let l1 = edit(&s, &root, &r3, vec![put("long.md", &text("old"))], "f4");
    let l2 = edit(&s, &root, &l1, vec![put("long.md", &text("new"))], "f5");
    let all = s.render(&middle, Some(&l2), None, None, 0).unwrap();
    let f = only(&all, "long.md");
    assert!(!f.summary.rewritten);
    assert_eq!(f.summary.paragraphs_changed, 30);
    assert_eq!((f.summary.added_lines, f.summary.removed_lines), (30, 30));
    let changed = |f: &FileRender| {
        f.paragraphs
            .iter()
            .filter(|p| p.kind == ParagraphKind::Changed)
            .count()
    };
    assert_eq!(changed(f), 12);
    assert_eq!(f.next_offset, Some(12));
    assert_eq!(f.paragraphs.len(), 24);
    assert_eq!(
        (f.paragraphs[0].kind, f.paragraphs[0].lines),
        (ParagraphKind::Same, Some(1))
    );
    assert_eq!(f.paragraphs[1].index, 1);
    assert_eq!(f.paragraphs.last().unwrap().index, 23);
    let page = |offset| {
        s.render(&middle, Some(&l2), None, Some("long.md"), offset)
            .unwrap()
            .files
            .remove(0)
    };
    let p2 = page(12);
    assert_eq!(changed(&p2), 12);
    assert_eq!(p2.next_offset, Some(24));
    assert_eq!(
        (p2.paragraphs[0].kind, p2.paragraphs[0].index),
        (ParagraphKind::Same, 24)
    );
    assert_eq!(p2.paragraphs[1].index, 25);
    let p3 = page(24);
    assert_eq!(changed(&p3), 6);
    assert_eq!(p3.next_offset, None);
    assert_eq!(p3.paragraphs.last().unwrap().index, 59);
    assert!(page(30).paragraphs.is_empty());
    // `offset` pages one file; without a path it is refused.
    assert_eq!(
        code(s.render(&middle, Some(&l2), None, None, 12)),
        "invalid_request"
    );
}

#[test]
fn render_hides_what_the_caller_may_not_see() {
    let (_fx, s, root, senior, middle) = fixture();
    let r1 = edit(
        &s,
        &root,
        "r0",
        vec![put("a.md", "a\n"), folder("senior-only")],
        "h1",
    );
    let r2 = edit(
        &s,
        &root,
        &r1,
        vec![hide_meta("senior-only", "senior")],
        "h2",
    );
    let r3 = edit(
        &s,
        &root,
        &r2,
        vec![put("senior-only/pay.md", "secret salary\n")],
        "h3",
    );
    // A hidden file renamed into a visible place, alongside a visible edit.
    let r4 = edit(
        &s,
        &root,
        &r3,
        vec![
            Change::Rename {
                path: "senior-only/pay.md".into(),
                to: "pay.md".into(),
            },
            put("a.md", "a\nb\n"),
        ],
        "h4",
    );
    let r = s.render(&middle, Some(&r4), None, None, 0).unwrap();
    assert_eq!(r.files.len(), 1);
    assert_eq!(r.files[0].path, "a.md");
    assert_eq!(r.hidden_paths, 1);
    let json = serde_json::to_string(&r).unwrap();
    assert!(
        !json.contains("secret") && !json.contains("pay.md"),
        "{json}"
    );
    assert_eq!(
        code(s.render(&middle, Some(&r4), None, Some("pay.md"), 0)),
        "not_found"
    );
    assert_eq!(
        code(s.render(&middle, Some(&r4), None, Some("senior-only/pay.md"), 0)),
        "not_found"
    );
    // A revision with nothing the caller may see is not found.
    assert_eq!(
        code(s.render(&middle, Some(&r3), None, None, 0)),
        "not_found"
    );
    assert_eq!(
        code(s.render(&middle, Some(&r2), None, None, 0)),
        "not_found"
    );

    // The senior sees both, the rename as a move.
    let r = s.render(&senior, Some(&r4), None, None, 0).unwrap();
    assert_eq!(r.hidden_paths, 0);
    let moved = only(&r, "pay.md");
    assert_eq!(moved.moved_from.as_deref(), Some("senior-only/pay.md"));
    assert_eq!(moved.kind, "text");
    assert_eq!(moved.summary.paragraphs_changed, 0);

    // A proposal on a hidden path is not found for the caller who may not review it.
    let p = s
        .propose(
            &senior,
            &r4,
            vec![put("senior-only/new.md", "x\n")],
            "hp",
            None,
            None,
        )
        .unwrap();
    assert_eq!(
        code(s.render(&middle, None, Some(&p.id), None, 0)),
        "not_found"
    );
    assert!(s.render(&root, None, Some(&p.id), None, 0).is_ok());
}

#[test]
fn render_ignores_line_ending_and_trailing_newline_noise() {
    let (_fx, s, root, _senior, middle) = fixture();
    let r1 = edit(
        &s,
        &root,
        "r0",
        vec![put("crlf.md", "Para one.\r\nline two\r\n\r\nPara two.\r\n")],
        "n1",
    );
    let r2 = edit(
        &s,
        &root,
        &r1,
        vec![put("crlf.md", "Para one.\nline two\n\nPara two.")],
        "n2",
    );
    let f = s
        .render(&middle, Some(&r2), None, None, 0)
        .unwrap()
        .files
        .remove(0);
    assert_eq!(f.kind, "text");
    assert_eq!((f.summary.added_lines, f.summary.removed_lines), (0, 0));
    assert_eq!(f.summary.paragraphs_changed, 0);
    assert!(!f.summary.rewritten);
    assert!(f.paragraphs.iter().all(|p| p.kind == ParagraphKind::Same));
    assert_eq!(f.next_offset, None);
}

#[test]
fn render_binary_meta_and_rename_kinds() {
    let (_fx, s, root, _senior, middle) = fixture();
    let fenced = "Intro\n\n```\nfn a() {}\n\nfn b() {}\n```\n\nOutro\n";
    let r1 = edit(
        &s,
        &root,
        "r0",
        vec![
            put("doc.md", "Hello\n\nWorld\n"),
            Change::PutBinary {
                path: "img.png".into(),
                bytes: vec![1, 2, 3],
            },
            folder("f"),
        ],
        "k1",
    );
    let r2 = edit(
        &s,
        &root,
        &r1,
        vec![Change::PutBinary {
            path: "img.png".into(),
            bytes: vec![4, 5, 6],
        }],
        "k2",
    );
    let f = s
        .render(&middle, Some(&r2), None, None, 0)
        .unwrap()
        .files
        .remove(0);
    assert_eq!(f.kind, "new_version");
    assert!(f.paragraphs.is_empty());
    let r3 = edit(
        &s,
        &root,
        &r2,
        vec![Change::SetMeta {
            path: "doc.md".into(),
            seen_from: None,
            colour: Some("blue".into()),
        }],
        "k3",
    );
    let f = s
        .render(&middle, Some(&r3), None, None, 0)
        .unwrap()
        .files
        .remove(0);
    assert_eq!(f.kind, "meta");
    let (before, after) = f.meta.clone().unwrap();
    assert_eq!(before.and_then(|m| m.colour), None);
    assert_eq!(after.unwrap().colour.as_deref(), Some("blue"));
    assert!(f.paragraphs.is_empty());
    let r4 = edit(
        &s,
        &root,
        &r3,
        vec![Change::Rename {
            path: "doc.md".into(),
            to: "f/doc.md".into(),
        }],
        "k4",
    );
    let f = s
        .render(&middle, Some(&r4), None, None, 0)
        .unwrap()
        .files
        .remove(0);
    assert_eq!(
        (f.path.as_str(), f.moved_from.as_deref()),
        ("f/doc.md", Some("doc.md"))
    );
    assert_eq!(f.summary.paragraphs_changed, 0);
    let r5 = edit(
        &s,
        &root,
        &r4,
        vec![
            put("code.md", fenced),
            Change::Delete {
                path: "f/doc.md".into(),
            },
        ],
        "k5",
    );
    let r = s.render(&middle, Some(&r5), None, None, 0).unwrap();
    let created = only(&r, "code.md");
    assert_eq!(created.kind, "created");
    // The fenced block, blank line and all, is one paragraph.
    assert_eq!(kinds(created), vec![ParagraphKind::Added; 3]);
    assert_eq!(
        created.paragraphs[1].spans[0].text,
        "```\nfn a() {}\n\nfn b() {}\n```"
    );
    let deleted = only(&r, "f/doc.md");
    assert_eq!(deleted.kind, "deleted");
    assert_eq!(kinds(deleted), vec![ParagraphKind::Removed; 2]);
    assert_eq!(deleted.paragraphs[1].spans[0].kind, SpanKind::Removed);
    assert_eq!(
        (deleted.summary.added_lines, deleted.summary.removed_lines),
        (0, 3)
    );
}

// ---- stale proposals ----

const DOC: &str = "P0 zero.\n\nP1 one.\n\nP2 two.\n\nP3 three.\n\nP4 four.\n";
fn doc_with(changes: &[(usize, &str)]) -> String {
    let mut p: Vec<String> = DOC.trim_end().split("\n\n").map(String::from).collect();
    for (i, text) in changes {
        p[*i] = text.to_string();
    }
    p.join("\n\n") + "\n"
}
/// `(index, fits, conflict)` of every paragraph the proposal changed.
fn flags(f: &FileRender) -> Vec<(u32, Option<bool>, Option<bool>)> {
    f.paragraphs
        .iter()
        .filter(|p| p.kind != ParagraphKind::Same)
        .map(|p| (p.index, p.fits, p.conflict))
        .collect()
}

#[test]
fn stale_proposal_renders_against_today() {
    let (_fx, s, root, senior, middle) = fixture();
    let r1 = edit(
        &s,
        &root,
        "r0",
        vec![
            put("doc.md", DOC),
            put("other.md", "O0\n\nO1\n\nO2\n\nO3\n"),
            put("moved.md", "M0\n\nM1\n\nM2\n\nM3\n"),
        ],
        "s1",
    );
    let propose = |path: &str, text: &str, key: &str| {
        s.propose(&middle, &r1, vec![put(path, text)], key, None, None)
            .unwrap()
            .id
    };
    let pa = propose("doc.md", &doc_with(&[(3, "P3 three, revised.")]), "pa");
    let pb = propose(
        "doc.md",
        &doc_with(&[(2, "P2 two, proposed."), (3, "P3 three, proposed.")]),
        "pb",
    );
    let pc = propose("doc.md", &doc_with(&[(4, "P4 four, proposed.")]), "pc");
    let pd = propose("other.md", "O0\n\nO1 proposed\n\nO2\n\nO3\n", "pd");
    let pe = propose("moved.md", "M0\n\nM1 proposed\n\nM2\n\nM3\n", "pe");

    // At head nothing is stale and nothing carries fits.
    let fresh = s.render(&senior, None, Some(&pa), None, 0).unwrap();
    assert_eq!((fresh.stale, fresh.head_revision.as_deref()), (false, None));
    assert_eq!(flags(&fresh.files[0]), vec![(3, None, None)]);

    // Head changes paragraph 1, deletes other.md and renames moved.md.
    let r2 = edit(
        &s,
        &root,
        &r1,
        vec![
            put("doc.md", &doc_with(&[(1, "P1 one, today.")])),
            Change::Delete {
                path: "other.md".into(),
            },
            Change::Rename {
                path: "moved.md".into(),
                to: "renamed.md".into(),
            },
        ],
        "s2",
    );
    let a = s.render(&senior, None, Some(&pa), None, 0).unwrap();
    assert!(a.stale);
    assert_eq!(a.head_revision.as_deref(), Some(&*r2));
    assert_eq!(a.revision_id, r1);
    assert_eq!(flags(&a.files[0]), vec![(3, Some(true), Some(false))]);
    // Unchanged paragraphs carry neither flag.
    assert!(a.files[0]
        .paragraphs
        .iter()
        .filter(|p| p.kind == ParagraphKind::Same)
        .all(|p| p.fits.is_none() && p.conflict.is_none()));

    // Head also changes paragraph 2: the proposal's paragraph 2 conflicts, 3 still fits.
    let r3 = edit(
        &s,
        &root,
        &r2,
        vec![
            put(
                "doc.md",
                &doc_with(&[(1, "P1 one, today."), (2, "P2 two, today.")]),
            ),
            put("renamed.md", "M0 today\n\nM1\n\nM2\n\nM3\n"),
        ],
        "s3",
    );
    let b = s.render(&senior, None, Some(&pb), None, 0).unwrap();
    assert_eq!(b.head_revision.as_deref(), Some(&*r3));
    let f = &b.files[0];
    assert_eq!(
        flags(f),
        vec![(2, Some(false), Some(true)), (3, Some(true), Some(false))]
    );
    // Spans still show the proposal's change against the base text.
    let two: Vec<(SpanKind, &str)> = f.paragraphs[1]
        .spans
        .iter()
        .map(|s| (s.kind, s.text.as_str()))
        .collect();
    assert_eq!(
        two,
        vec![
            (SpanKind::Same, "P2 two"),
            (SpanKind::Added, ", proposed"),
            (SpanKind::Same, "."),
        ]
    );
    let a = s.render(&senior, None, Some(&pa), None, 0).unwrap();
    assert_eq!(flags(&a.files[0]), vec![(3, Some(true), Some(false))]);

    // Three revisions behind, head changed only unrelated paragraphs: all fits.
    let r4 = edit(
        &s,
        &root,
        &r3,
        vec![put(
            "doc.md",
            &doc_with(&[
                (0, "P0 zero, today."),
                (1, "P1 one, today."),
                (2, "P2 two, today."),
            ]),
        )],
        "s4",
    );
    let c = s.render(&senior, None, Some(&pc), None, 0).unwrap();
    assert_eq!(c.head_revision.as_deref(), Some(&*r4));
    assert_eq!(flags(&c.files[0]), vec![(4, Some(true), Some(false))]);
    assert!(c.files[0]
        .paragraphs
        .iter()
        .all(|p| p.conflict != Some(true)));

    // A file deleted at head: everything the proposal changed conflicts.
    let d = s.render(&senior, None, Some(&pd), None, 0).unwrap();
    assert_eq!(flags(&d.files[0]), vec![(1, Some(false), Some(true))]);
    // A file renamed at head is followed by identity; its unrelated edit fits.
    let e = s.render(&senior, None, Some(&pe), None, 0).unwrap();
    assert_eq!(flags(&e.files[0]), vec![(1, Some(true), Some(false))]);

    // A proposal that has ended is not compared with today.
    s.withdraw(&middle, &pe, &s.review(&middle, &pe).unwrap().digest, "we")
        .unwrap();
    let ended = s.render(&senior, None, Some(&pe), None, 0).unwrap();
    assert_eq!((ended.stale, ended.head_revision.as_deref()), (false, None));
    assert_eq!(flags(&ended.files[0]), vec![(1, None, None)]);
    // A new proposal at today's head is not stale.
    let pf = s
        .propose(
            &middle,
            &r4,
            vec![put("doc.md", &doc_with(&[(4, "P4 now.")]))],
            "pf",
            None,
            None,
        )
        .unwrap();
    let f = s.render(&senior, None, Some(&pf.id), None, 0).unwrap();
    assert!(!f.stale);
    assert!(f.files[0]
        .paragraphs
        .iter()
        .all(|p| p.fits.is_none() && p.conflict.is_none()));
}

#[test]
fn stale_created_file_conflicts_when_head_created_the_same_path() {
    let (_fx, s, root, senior, middle) = fixture();
    let r1 = edit(&s, &root, "r0", vec![put("base.md", "b\n")], "c1");
    let p = s
        .propose(
            &middle,
            &r1,
            vec![put("x.md", "Proposed one.\n\nProposed two.\n")],
            "cp",
            None,
            None,
        )
        .unwrap();
    let fresh = s.render(&senior, None, Some(&p.id), None, 0).unwrap();
    assert!(!fresh.stale);
    // Head creates the same path from a later base, so under another identity.
    let r2 = edit(&s, &root, &r1, vec![put("base.md", "c\n")], "c2");
    edit(&s, &root, &r2, vec![put("x.md", "Head's own.\n")], "c3");
    let r = s.render(&senior, None, Some(&p.id), None, 0).unwrap();
    assert!(r.stale);
    assert_eq!(
        flags(&r.files[0]),
        vec![(0, Some(false), Some(true)), (1, Some(false), Some(true))]
    );
}

#[test]
fn render_without_path_pages_at_most_24_files() {
    let (_fx, s, root, _senior, middle) = fixture();
    let changes: Vec<Change> = (0..30)
        .map(|i| put(&format!("f{i:02}.md"), "one\n\ntwo\n"))
        .collect();
    let r1 = edit(&s, &root, "r0", changes, "m1");
    let r = s.render(&middle, Some(&r1), None, None, 0).unwrap();
    assert_eq!(r.files.len(), 30);
    for (i, f) in r.files.iter().enumerate() {
        assert_eq!(f.summary.paragraphs_changed, 2);
        if i < 24 {
            assert_eq!((f.paragraphs.len(), f.next_offset), (2, None));
        } else {
            // Summary only; the file's first page is read with its path.
            assert_eq!((f.paragraphs.len(), f.next_offset), (0, Some(0)));
        }
    }
    let one = s
        .render(&middle, Some(&r1), None, Some("f29.md"), 0)
        .unwrap();
    assert_eq!(one.files[0].paragraphs.len(), 2);
}

fn run_api(
    s: &Service,
    who: &TrustedIdentity,
    version: u32,
    op: serde_json::Value,
) -> mnemosyne_content::api::Response {
    s.execute(
        who,
        mnemosyne_content::api::Request {
            version,
            idempotency_key: Some("k-api".into()),
            operation: serde_json::from_value(op).unwrap(),
        },
    )
}

#[test]
fn stat_reports_counts_meta_and_stamps() {
    let (_fx, s, root, senior, middle) = fixture();
    let text = vec!["alpha"; 1240].join(" ");
    let r1 = s
        .edit(&senior, "r0", vec![put("big.md", &text)], "s1", None, None)
        .unwrap();
    let r2 = s
        .edit(
            &root,
            &r1.id,
            vec![put("big.md", &format!("{text} omega"))],
            "s2",
            None,
            None,
        )
        .unwrap();
    let st = s.stat(&middle, "big.md", None).unwrap();
    assert_eq!(st.path, "big.md");
    assert_eq!(st.revision_id, r2.id);
    assert_eq!(st.mime, "text/markdown");
    assert_eq!(st.words, Some(1241));
    assert_eq!(st.characters, Some(text.chars().count() as u32 + 6));
    let tokens = st.tokens.unwrap();
    assert!(tokens > 0 && tokens < st.characters.unwrap());
    assert_eq!(st.token_model, "cl100k_base");
    let created = st.created.unwrap();
    assert_eq!(created.actor.actor_id, "senior-s");
    assert_eq!(created.revision_id, r1.id);
    assert_eq!(created.at.as_deref(), Some(AT));
    let changed = st.changed.unwrap();
    assert_eq!(changed.actor.actor_id, "root");
    assert_eq!(changed.revision_id, r2.id);
    let p = s.permissions(&middle, "big.md").unwrap();
    assert_eq!(st.seen_from, p.seen_from);
    assert_eq!(st.inherited_from, p.inherited_from);
    assert_eq!(st.colour, p.colour);
    assert_eq!(st.colour_source, p.colour_source);

    // At the first revision, the file had one stamp and the old counts.
    let old = s.stat(&middle, "big.md", Some(&r1.id)).unwrap();
    assert_eq!(old.words, Some(1240));
    assert_eq!(old.changed.unwrap().revision_id, r1.id);
    assert_eq!(old.created.unwrap().revision_id, r1.id);
}

#[test]
fn stat_on_binary_without_extraction_is_null() {
    let (_fx, s, root, _senior, middle) = fixture();
    s.edit(
        &root,
        "r0",
        vec![
            Change::PutBinary {
                path: "scan.pdf".into(),
                bytes: b"not really a pdf".to_vec(),
            },
            Change::PutBinary {
                path: "pic.png".into(),
                bytes: vec![1, 2, 3],
            },
        ],
        "b1",
        None,
        None,
    )
    .unwrap();
    for (path, mime) in [("scan.pdf", "application/pdf"), ("pic.png", "image/png")] {
        let st = s.stat(&middle, path, None).unwrap();
        assert_eq!(st.mime, mime);
        assert_eq!((st.words, st.characters, st.tokens), (None, None, None));
        assert_eq!(st.created.unwrap().actor.actor_id, "root");
    }
}

#[test]
fn stat_hidden_is_not_found() {
    let (_fx, s, root, _senior, middle) = fixture();
    let r1 = edit(
        &s,
        &root,
        "r0",
        vec![
            put("open.md", "visible"),
            folder("secret"),
            put("secret/a.md", "hidden"),
        ],
        "h1",
    );
    edit(
        &s,
        &root,
        &r1,
        vec![hide_meta("secret", "administrator")],
        "h2",
    );
    let st = s.stat(&middle, "open.md", None).unwrap();
    assert_eq!(st.words, Some(1));
    assert_eq!(code(s.stat(&middle, "secret/a.md", None)), "not_found");
    assert_eq!(code(s.stat(&middle, "missing.md", None)), "not_found");
    assert!(s.stat(&root, "secret/a.md", None).is_ok());
}

#[test]
fn stat_mime_table_and_word_counting() {
    use mnemosyne_content::api::ApiResult;
    let (_fx, s, root, _senior, _middle) = fixture();
    let folder = folder("f");
    s.edit(
        &root,
        "r0",
        vec![folder, put("f/x.py", "don't stop-gap -- a--b\n")],
        "m1",
        None,
        None,
    )
    .unwrap();
    let st = s.stat(&root, "f", None).unwrap();
    assert_eq!(st.mime, "inode/directory");
    assert_eq!(st.words, None);
    let st = s.stat(&root, "f/x.py", None).unwrap();
    assert_eq!(st.mime, "text/x-python");
    // don't, stop-gap, a, b
    assert_eq!(st.words, Some(4));
    let r = run_api(
        &s,
        &root,
        2,
        serde_json::json!({"type": "stat", "path": "f/x.py"}),
    );
    assert!(
        matches!(r.result, Some(ApiResult::Stat(_))),
        "{:?}",
        r.error
    );
}

#[test]
fn list_proposals_summaries_carry_time_reason_and_paths() {
    let (_fx, s, _root, _senior, middle) = fixture();
    let changes: Vec<Change> = (0..10).map(|i| put(&format!("d/f{i}.md"), "x")).collect();
    let p = s
        .propose(
            &middle,
            "r0",
            changes,
            "p1",
            Some("Ten notes"),
            Some(assistant()),
        )
        .unwrap();
    let list = s
        .list_proposals(&middle, &ProposalListRequest::default())
        .unwrap();
    let sum = &list.proposals[0];
    assert_eq!(sum.id, p.id);
    assert_eq!(sum.at.as_deref(), Some(AT));
    assert_eq!(sum.reason.as_deref(), Some("Ten notes"));
    assert_eq!(sum.origin, Some(assistant()));
    assert_eq!(sum.path_count, 10);
    assert_eq!(sum.paths.len(), 8);
    assert_eq!(sum.paths[0], "d/f0.md");
}

#[test]
fn new_operations_require_v2() {
    let (_fx, s, root, _senior, _middle) = fixture();
    let ops = [
        serde_json::json!({"type": "withdraw", "proposal_id": "p", "digest": "d"}),
        serde_json::json!({"type": "log"}),
        serde_json::json!({"type": "render", "revision_id": "r1"}),
        serde_json::json!({"type": "stat", "path": "a.md"}),
    ];
    for op in ops {
        let r = run_api(&s, &root, 1, op.clone());
        assert_eq!(r.error.unwrap().code, "unsupported_version", "{op}");
    }
}

#[test]
fn stat_loads_only_the_counted_text_not_the_history() {
    use sha2::{Digest, Sha256};
    let (fx, s, root, _senior, middle) = fixture();
    let old = "old ".repeat(600);
    let r1 = edit(&s, &root, "r0", vec![put("n.md", &old)], "t1");
    let r2 = edit(&s, &root, &r1, vec![put("n.md", &"mid ".repeat(600))], "t2");
    edit(&s, &root, &r2, vec![put("n.md", &"new ".repeat(600))], "t3");
    // Remove the stored text of the first two versions: stamps and counts for the head
    // must not need them.
    for text in [old.clone(), "mid ".repeat(600)] {
        let id = format!("{:x}", Sha256::digest(text.as_bytes()));
        std::fs::remove_file(fx.0.join("blobs").join(id)).unwrap();
    }
    let st = s.stat(&middle, "n.md", None).unwrap();
    assert_eq!(st.words, Some(600));
    assert_eq!(st.created.unwrap().revision_id, r1);
}

#[test]
fn stat_tokens_are_null_above_four_mebibytes_but_words_still_count() {
    let (_fx, s, root, _senior, _middle) = fixture();
    let small = edit(&s, &root, "r0", vec![put("s.md", "a few words")], "c1");
    let big = "ab ".repeat(4 * 1024 * 1024 / 3 + 1);
    assert!(big.len() > 4 * 1024 * 1024);
    edit(&s, &root, &small, vec![put("big.md", &big)], "c2");
    let st = s.stat(&root, "big.md", None).unwrap();
    assert_eq!(st.token_model, "cl100k_base");
    assert_eq!(st.tokens, None);
    assert_eq!(st.words, Some(4 * 1024 * 1024 / 3 + 1));
    assert_eq!(st.characters, Some(big.chars().count() as u32));
    let st = s.stat(&root, "s.md", None).unwrap();
    assert!(st.tokens.is_some_and(|t| t > 0));
}

#[test]
fn applied_proposal_keeps_the_proposals_origin_in_the_log() {
    let (_fx, s, root, senior, middle) = fixture();
    let r = s
        .edit(&root, "r0", vec![put("a.md", "one")], "e1", None, None)
        .unwrap();
    let origin = Origin {
        source: "assistant".into(),
        model: Some("sonnet-5.5".into()),
        chat_id: Some("chat-1".into()),
    };
    let p = s
        .propose(
            &middle,
            &r.id,
            vec![put("a.md", "two")],
            "p1",
            Some("Draft"),
            Some(origin.clone()),
        )
        .unwrap();
    s.approve(&senior, &p.id, &p.digest, "d1", None).unwrap();
    let applied = s.apply(&senior, &p.id, &p.digest, "a1").unwrap();
    assert_eq!(applied.origin, Some(origin.clone()));
    let log = s.log(&root, "", 20, 0).unwrap();
    assert_eq!(log.entries[0].id, applied.id);
    assert_eq!(
        log.entries[0].kind,
        Some(mnemosyne_content::RevisionKind::Proposal)
    );
    assert_eq!(log.entries[0].origin, Some(origin));
    // A retry replays the same revision.
    assert_eq!(
        s.apply(&senior, &p.id, &p.digest, "a1").unwrap().id,
        applied.id
    );
}

#[test]
fn log_answers_not_found_for_an_absent_scope_and_bounds_offset() {
    let (_fx, s, root, _senior, _middle) = fixture();
    s.edit(&root, "r0", vec![put("a.md", "one")], "e1", None, None)
        .unwrap();
    assert_eq!(code(s.log(&root, "nope/", 20, 0)), "invalid_path");
    assert_eq!(code(s.log(&root, "nope", 20, 0)), "not_found");
    assert_eq!(code(s.log(&root, "", 20, 1_000_001)), "invalid_request");
    assert_eq!(code(s.log(&root, "", 20, usize::MAX)), "invalid_request");
    assert!(s.log(&root, "a.md", 20, 1_000_000).is_ok());
}

#[test]
fn stale_proposals_can_be_declined_but_not_approved() {
    let (_fx, s, _root, senior, middle) = fixture();
    let r = s
        .edit(&senior, "r0", vec![put("x.md", "1")], "e1", None, None)
        .unwrap();
    let p = s
        .propose(&middle, &r.id, vec![put("x.md", "2")], "p1", None, None)
        .unwrap();
    let q = s
        .propose(&middle, &r.id, vec![put("x.md", "3")], "p2", None, None)
        .unwrap();
    let a = s
        .propose(&middle, &r.id, vec![put("x.md", "4")], "p3", None, None)
        .unwrap();
    // A senior edit of x.md moves head past every proposal's base.
    let moved = s
        .edit(&senior, &r.id, vec![put("x.md", "o")], "e2", None, None)
        .unwrap();
    assert_ne!(moved.id, p.base_revision);
    // Approve of a stale proposal still answers stale_base (rebase is the path).
    assert_eq!(
        code(s.approve(&senior, &a.id, &a.digest, "a1", None)),
        "stale_base"
    );
    assert_eq!(
        s.review(&senior, &a.id).unwrap().state,
        ProposalState::Pending
    );
    // Reject of a stale proposal is a normal decline.
    let d = s
        .reject(&senior, &p.id, &p.digest, "d1", Some("Not now"))
        .unwrap();
    assert_eq!(d.state, ProposalState::Rejected);
    let last = d.decisions.last().unwrap();
    assert!(!last.approved);
    assert_eq!(last.actor.actor_id, "senior-s");
    assert_eq!(last.reason.as_deref(), Some("Not now"));
    assert!(last.at.is_some());
    // Replay of the same reject answers the stored decision.
    let again = s
        .reject(&senior, &p.id, &p.digest, "d1", Some("Not now"))
        .unwrap();
    assert_eq!(again.decisions.len(), d.decisions.len());
    assert_eq!(again.state, ProposalState::Rejected);
    // Withdraw of a stale proposal is the proposer's normal decline.
    let w = s.withdraw(&middle, &q.id, &q.digest, "w1").unwrap();
    assert_eq!(w.state, ProposalState::Withdrawn);
    assert_eq!(s.head(&senior).unwrap(), moved.id);
}

// ---- stale by touched path (hotfix) ----

/// Root seeds `a.md` and `b.md`; the middle proposes on `a.md` at that head.
fn seeded_proposal(
    s: &Service,
    root: &TrustedIdentity,
    middle: &TrustedIdentity,
    key: &str,
) -> (String, mnemosyne_content::Proposal) {
    let r1 = edit(
        s,
        root,
        "r0",
        vec![put("a.md", "A0\n"), put("b.md", "B0\n")],
        &format!("{key}-seed"),
    );
    let p = s
        .propose(
            middle,
            &r1,
            vec![put("a.md", "A proposed\n")],
            key,
            None,
            None,
        )
        .unwrap();
    (r1, p)
}

#[test]
fn a_head_move_on_other_files_leaves_a_proposal_fresh() {
    let (_fx, s, root, senior, middle) = fixture();
    let (r1, p) = seeded_proposal(&s, &root, &middle, "p1");
    // Approved at head, then the head moves on another file only.
    let q = s
        .propose(&middle, &r1, vec![put("c.md", "C new\n")], "p2", None, None)
        .unwrap();
    s.approve(&senior, &q.id, &q.digest, "aq", None).unwrap();
    let r2 = edit(&s, &senior, &r1, vec![put("b.md", "B senior\n")], "e2");

    let view = s.render(&senior, None, Some(&p.id), None, 0).unwrap();
    assert_eq!((view.stale, view.head_revision.as_deref()), (false, None));
    assert_eq!(
        code(s.rebase_proposal(&senior, &p.id, &p.digest, "fitting", "rb")),
        "not_stale"
    );

    let approved = s.approve(&senior, &p.id, &p.digest, "ap", None).unwrap();
    assert_eq!(approved.state, ProposalState::Approved);
    let applied = s.apply(&senior, &p.id, &p.digest, "xp").unwrap();
    assert_eq!(applied.parent.as_deref(), Some(&*r2));
    assert_eq!(applied.changes.len(), 1);
    assert_eq!(applied.changes[0].path, "a.md");
    assert_eq!(s.read(&root, "a.md", None).unwrap().content, "A proposed\n");
    assert_eq!(s.read(&root, "b.md", None).unwrap().content, "B senior\n");
    // Replay answers the same revision.
    assert_eq!(
        s.apply(&senior, &p.id, &p.digest, "xp").unwrap().id,
        applied.id
    );

    // The proposal approved before the move applies on today's head too.
    let applied_q = s.apply(&senior, &q.id, &q.digest, "xq").unwrap();
    assert_eq!(applied_q.parent.as_deref(), Some(&*applied.id));
    assert_eq!(s.read(&root, "c.md", None).unwrap().content, "C new\n");
    assert_eq!(s.read(&root, "a.md", None).unwrap().content, "A proposed\n");
}

#[test]
fn a_change_to_a_touched_path_keeps_stale_base() {
    let (_fx, s, root, senior, middle) = fixture();
    let (r1, p) = seeded_proposal(&s, &root, &middle, "p1");
    let q = s
        .propose(
            &middle,
            &r1,
            vec![put("b.md", "B proposed\n")],
            "p2",
            None,
            None,
        )
        .unwrap();
    s.approve(&senior, &q.id, &q.digest, "aq", None).unwrap();
    let r2 = edit(&s, &senior, &r1, vec![put("a.md", "A senior\n")], "e2");
    edit(&s, &senior, &r2, vec![put("other.md", "o\n")], "e3");
    assert_eq!(
        code(s.approve(&senior, &p.id, &p.digest, "ap", None)),
        "stale_base"
    );
    let view = s.render(&senior, None, Some(&p.id), None, 0).unwrap();
    assert!(view.stale);
    assert!(view.head_revision.is_some());
    // q (on b.md) is untouched by the moves and applies; p is stale to rebase (its one
    // paragraph changed on both sides, so nothing fits, but it is not `not_stale`).
    s.apply(&senior, &q.id, &q.digest, "xq").unwrap();
    assert_eq!(
        code(s.rebase_proposal(&senior, &p.id, &p.digest, "fitting", "rb")),
        "nothing_fits"
    );
    // Approved, then its path changes: apply answers stale_base.
    let head = s.head(&root).unwrap();
    let w = s
        .propose(
            &middle,
            &head,
            vec![put("b.md", "B again\n")],
            "p3",
            None,
            None,
        )
        .unwrap();
    s.approve(&senior, &w.id, &w.digest, "aw", None).unwrap();
    edit(&s, &senior, &head, vec![put("b.md", "B senior\n")], "e4");
    assert_eq!(code(s.apply(&senior, &w.id, &w.digest, "xw")), "stale_base");
}

#[test]
fn a_rename_of_a_touched_file_makes_the_proposal_stale() {
    // Ruling: renaming the file a proposal touches is a change of that file, even
    // though its identity and text are unchanged; the proposal is stale.
    let (_fx, s, root, senior, middle) = fixture();
    let (r1, p) = seeded_proposal(&s, &root, &middle, "p1");
    edit(
        &s,
        &senior,
        &r1,
        vec![Change::Rename {
            path: "a.md".into(),
            to: "c.md".into(),
        }],
        "mv",
    );
    assert_eq!(
        code(s.approve(&senior, &p.id, &p.digest, "ap", None)),
        "stale_base"
    );
    assert!(s.render(&senior, None, Some(&p.id), None, 0).unwrap().stale);
}

#[test]
fn edit_on_an_old_base_proceeds_when_its_paths_did_not_change() {
    let (_fx, s, root, senior, _middle) = fixture();
    let r1 = edit(
        &s,
        &root,
        "r0",
        vec![put("a.md", "A0\n"), put("b.md", "B0\n")],
        "seed",
    );
    let r2 = edit(&s, &root, &r1, vec![put("a.md", "A root\n")], "e2");
    // An old base, touching only b.md: published on today's head.
    let r3 = s
        .edit(
            &senior,
            &r1,
            vec![put("b.md", "B senior\n")],
            "e3",
            None,
            None,
        )
        .unwrap();
    assert_eq!(r3.parent.as_deref(), Some(&*r2));
    assert_eq!(s.read(&root, "a.md", None).unwrap().content, "A root\n");
    assert_eq!(s.read(&root, "b.md", None).unwrap().content, "B senior\n");
    // Its retry replays.
    assert_eq!(
        s.edit(
            &senior,
            &r1,
            vec![put("b.md", "B senior\n")],
            "e3",
            None,
            None
        )
        .unwrap()
        .id,
        r3.id
    );
    // The same old base touching a.md, which r2 changed, answers stale_base.
    assert_eq!(
        code(s.edit(
            &senior,
            &r1,
            vec![put("a.md", "A senior\n")],
            "e4",
            None,
            None
        )),
        "stale_base"
    );
    // An unknown base is stale.
    assert_eq!(
        code(s.edit(&senior, "rnope", vec![put("b.md", "x\n")], "e5", None, None)),
        "stale_base"
    );
}

#[test]
fn propose_on_an_old_base_proceeds_when_its_paths_did_not_change() {
    let (_fx, s, root, senior, middle) = fixture();
    let r1 = edit(
        &s,
        &root,
        "r0",
        vec![put("a.md", "A0\n\nA1\n"), put("b.md", "B0\n")],
        "seed",
    );
    let r2 = edit(&s, &senior, &r1, vec![put("b.md", "B senior\n")], "e2");
    // An old base touching only a.md: accepted, the base recorded as given.
    let p = s
        .propose(
            &middle,
            &r1,
            vec![put("a.md", "A0\n\nA1 proposed\n")],
            "p1",
            None,
            None,
        )
        .unwrap();
    assert_eq!(p.base_revision, r1);
    assert_eq!(p.diff.len(), 1);
    assert_eq!(p.diff[0].before.as_deref(), Some("A0\n\nA1\n"));
    // Its retry replays.
    assert_eq!(
        s.propose(
            &middle,
            &r1,
            vec![put("a.md", "A0\n\nA1 proposed\n")],
            "p1",
            None,
            None
        )
        .unwrap()
        .id,
        p.id
    );
    let view = s.render(&senior, None, Some(&p.id), None, 0).unwrap();
    assert_eq!((view.stale, view.revision_id.as_str()), (false, &*r1));
    assert_eq!(
        code(s.rebase_proposal(&senior, &p.id, &p.digest, "fitting", "rb")),
        "not_stale"
    );
    // The same old base touching b.md, which r2 changed, answers stale_base.
    assert_eq!(
        code(s.propose(&middle, &r1, vec![put("b.md", "B mid\n")], "p2", None, None)),
        "stale_base"
    );
    // a.md then changes: the proposal is stale, renders against today and rebases.
    edit(
        &s,
        &senior,
        &r2,
        vec![put("a.md", "A0 today\n\nA1\n")],
        "e3",
    );
    let view = s.render(&senior, None, Some(&p.id), None, 0).unwrap();
    assert!(view.stale);
    assert_eq!(view.revision_id, r1);
    let rebased = s
        .rebase_proposal(&senior, &p.id, &p.digest, "fitting", "rb2")
        .unwrap();
    s.approve(&senior, &rebased.id, &rebased.digest, "ap", None)
        .unwrap();
    s.apply(&senior, &rebased.id, &rebased.digest, "xp")
        .unwrap();
    assert_eq!(
        s.read(&root, "a.md", None).unwrap().content,
        "A0 today\n\nA1 proposed\n"
    );
    assert_eq!(s.read(&root, "b.md", None).unwrap().content, "B senior\n");
}
