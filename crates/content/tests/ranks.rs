use mnemosyne_content::{
    auth::{ActorKind, Grant, Member, Permission, Policy, Rungs, TenantPolicy, TrustedIdentity},
    ranks::{effective_colour, effective_seen_from, ColourSource, PathMeta},
    Entry, EntryKind, Service,
};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(std::path::PathBuf);
impl Fixture {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "mnemosyne-ranks-{}-{}",
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
fn ladder() -> Vec<String> {
    ["worker", "junior", "middle", "senior", "administrator"]
        .map(String::from)
        .to_vec()
}
fn policy_with(who: &TrustedIdentity, tenant: &str, section: TenantPolicy) -> Policy {
    Policy {
        grants: vec![grant(who, Permission::Admin, "")],
        tenants: BTreeMap::from([(tenant.to_string(), section)]),
    }
}

#[test]
fn policy_get_shows_default_ladder_and_set_round_trips_tenant_section() {
    let fx = Fixture::new();
    let s = fx.service();
    let admin = identity("t1", "admin", ActorKind::Human);
    s.bootstrap_policy(Policy {
        grants: vec![grant(&admin, Permission::Admin, "")],
        tenants: BTreeMap::new(),
    })
    .unwrap();

    let state = s.policy_state(&admin).unwrap();
    assert_eq!(state.policy.tenants.len(), 1);
    assert_eq!(state.policy.tenants["t1"], TenantPolicy::default());
    let d = TenantPolicy::default();
    assert_eq!(d.roles, ladder());
    assert_eq!(d.rungs.propose_from, "middle");
    assert_eq!(d.rungs.publish_from, "senior");
    assert_eq!(d.rungs.visibility_from, "senior");
    assert_eq!(d.root_seen_from, "junior");
    assert_eq!(d.colour_seed, None);

    let section = TenantPolicy {
        roles: ladder(),
        members: BTreeMap::from([
            (
                "ursula".to_string(),
                Member {
                    role: "senior".into(),
                    kind: ActorKind::Human,
                },
            ),
            (
                "bot".to_string(),
                Member {
                    role: "junior".into(),
                    kind: ActorKind::Agent,
                },
            ),
        ]),
        rungs: Rungs {
            propose_from: "middle".into(),
            publish_from: "senior".into(),
            visibility_from: "senior".into(),
        },
        root_seen_from: "junior".into(),
        colour_seed: Some(7),
        agent_sees_above_rank: false,
    };
    assert_eq!(section.rank("senior"), Some(3));
    assert_eq!(section.rank("nobody"), None);
    assert_eq!(section.member_role("bot").unwrap().role, "junior");
    assert!(section.member_role("ghost").is_none());

    s.replace_policy(
        &admin,
        policy_with(&admin, "t1", section.clone()),
        Some(&state.digest),
    )
    .unwrap();
    let after = s.policy_state(&admin).unwrap();
    assert_eq!(after.policy.tenants["t1"], section);
    assert_ne!(after.digest, state.digest);

    // Omitting tenants resets the caller's section to the default.
    s.replace_policy(
        &admin,
        Policy {
            grants: vec![grant(&admin, Permission::Admin, "")],
            tenants: BTreeMap::new(),
        },
        None,
    )
    .unwrap();
    assert_eq!(
        s.policy(&admin).unwrap().tenants["t1"],
        TenantPolicy::default()
    );
}

#[test]
fn invalid_tenant_policy_is_rejected() {
    let fx = Fixture::new();
    let s = fx.service();
    let admin = identity("t1", "admin", ActorKind::Human);
    s.bootstrap_policy(Policy {
        grants: vec![grant(&admin, Permission::Admin, "")],
        tenants: BTreeMap::new(),
    })
    .unwrap();
    let bad = |edit: &dyn Fn(&mut TenantPolicy)| {
        let mut t = TenantPolicy::default();
        edit(&mut t);
        let e = s
            .replace_policy(&admin, policy_with(&admin, "t1", t), None)
            .unwrap_err();
        assert_eq!(e.code, "invalid_policy");
    };
    bad(&|t| t.roles.push("worker".into()));
    bad(&|t| t.roles.clear());
    bad(&|t| {
        t.members.insert(
            "m".into(),
            Member {
                role: "ghost".into(),
                kind: ActorKind::Human,
            },
        );
    });
    bad(&|t| t.rungs.publish_from = "ghost".into());
    bad(&|t| t.rungs.propose_from = "ghost".into());
    bad(&|t| t.rungs.visibility_from = "ghost".into());
    bad(&|t| t.root_seen_from = "ghost".into());
    bad(&|t| t.roles = (0..17).map(|i| format!("r{i}")).collect());
    bad(&|t| t.roles[0] = "x".repeat(65));
    bad(&|t| t.roles[0] = "".into());
    bad(&|t| t.roles[0] = "a\u{7}b".into());
    bad(&|t| {
        t.members.insert(
            "".into(),
            Member {
                role: "senior".into(),
                kind: ActorKind::Human,
            },
        );
    });

    let e = s
        .replace_policy(
            &admin,
            policy_with(&admin, "other", TenantPolicy::default()),
            None,
        )
        .unwrap_err();
    assert_eq!(e.code, "forbidden");
}

fn entry(id: &str, kind: EntryKind, meta: PathMeta) -> Entry {
    Entry {
        file_id: id.into(),
        kind,
        blob: None,
        original_blob: None,
        meta,
    }
}
fn seen(role: &str) -> PathMeta {
    PathMeta {
        seen_from: Some(role.into()),
        colour: None,
    }
}
fn colour(name: &str) -> PathMeta {
    PathMeta {
        seen_from: None,
        colour: Some(name.into()),
    }
}

#[test]
fn seen_from_inherits_from_nearest_ancestor_and_root() {
    let mut files = BTreeMap::new();
    files.insert("a".into(), entry("f1", EntryKind::Folder, seen("senior")));
    files.insert(
        "a/b".into(),
        entry("f2", EntryKind::Folder, PathMeta::default()),
    );
    files.insert(
        "a/b/c.md".into(),
        entry("f3", EntryKind::Text, PathMeta::default()),
    );
    files.insert(
        "d.md".into(),
        entry("f4", EntryKind::Text, PathMeta::default()),
    );
    let t = TenantPolicy::default();
    assert_eq!(effective_seen_from(&files, &t, "a/b/c.md"), "senior");
    assert_eq!(effective_seen_from(&files, &t, "d.md"), t.root_seen_from);
    assert_eq!(
        effective_seen_from(&files, &t, "missing/x.md"),
        t.root_seen_from
    );
    files.insert(
        "a/b/c.md".into(),
        entry("f3", EntryKind::Text, seen("administrator")),
    );
    assert_eq!(effective_seen_from(&files, &t, "a/b/c.md"), "administrator");
}

#[test]
fn colour_is_explicit_then_inherited_then_seeded_default() {
    let t = TenantPolicy::default();
    let mut files = BTreeMap::new();
    for i in 1..=3 {
        files.insert(
            format!("d{i}"),
            entry(&format!("f{i}"), EntryKind::Folder, PathMeta::default()),
        );
    }
    let names: Vec<String> = (1..=3)
        .map(|i| effective_colour(&files, &t, "t1", &format!("d{i}")).0)
        .collect();
    for n in &names {
        assert_ne!(n, "other");
    }
    assert_eq!(
        names
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        3
    );
    let again: Vec<String> = (1..=3)
        .map(|i| effective_colour(&files, &t, "t1", &format!("d{i}")).0)
        .collect();
    assert_eq!(names, again);
    assert_eq!(
        effective_colour(&files, &t, "t1", "d1").1,
        ColourSource::Default
    );

    files.insert("d1".into(), entry("f1", EntryKind::Folder, colour("cyan")));
    files.insert(
        "d1/x.md".into(),
        entry("f9", EntryKind::Text, PathMeta::default()),
    );
    assert_eq!(
        effective_colour(&files, &t, "t1", "d1"),
        ("cyan".into(), ColourSource::Explicit)
    );
    assert_eq!(
        effective_colour(&files, &t, "t1", "d1/x.md"),
        ("cyan".into(), ColourSource::Inherited)
    );
    files.insert(
        "top.md".into(),
        entry("f8", EntryKind::Text, PathMeta::default()),
    );
    assert_eq!(
        effective_colour(&files, &t, "t1", "top.md"),
        ("other".into(), ColourSource::Default)
    );

    // Seventh and later top-level folders get "other".
    for i in 4..=7 {
        files.insert(
            format!("d{i}"),
            entry(&format!("f{i}"), EntryKind::Folder, PathMeta::default()),
        );
    }
    assert_eq!(
        effective_colour(&files, &t, "t1", "d7"),
        ("other".into(), ColourSource::Default)
    );

    // An explicit seed overrides the tenant-id fallback and is deterministic.
    let seeded = TenantPolicy {
        colour_seed: Some(42),
        ..TenantPolicy::default()
    };
    assert_eq!(
        effective_colour(&files, &seeded, "t1", "d2"),
        effective_colour(&files, &seeded, "other-tenant", "d2")
    );
}

#[test]
fn store_version_moves_to_4_and_entries_keep_meta() {
    let fx = Fixture::new();
    let s = fx.service();
    let admin = identity("t1", "admin", ActorKind::Human);
    s.bootstrap_policy(Policy {
        grants: vec![
            grant(&admin, Permission::Admin, ""),
            grant(&admin, Permission::Edit, ""),
            grant(&admin, Permission::Read, ""),
        ],
        tenants: BTreeMap::new(),
    })
    .unwrap();
    s.edit(
        &admin,
        &s.head(&admin).unwrap(),
        vec![mnemosyne_content::Change::Put {
            path: "a.md".into(),
            content: "x".into(),
        }],
        "k1",
        None,
        None,
    )
    .unwrap();
    let r = fx
        .service()
        .read_binary(&admin, "a.md", None, false, None)
        .unwrap();
    assert!(r.entry.meta.is_empty());
    assert!(!serde_json::to_string(&r.entry).unwrap().contains("meta"));
    let c = rusqlite::Connection::open(fx.0.join("authority.sqlite3")).unwrap();
    let v: String = c
        .query_row("SELECT value FROM meta WHERE key='version'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(v, "4");
}

// ---- one permission function ----

fn member(role: &str, kind: ActorKind) -> Member {
    Member {
        role: role.into(),
        kind,
    }
}
fn ranked_section() -> TenantPolicy {
    TenantPolicy {
        members: BTreeMap::from([
            (
                "root".to_string(),
                member("administrator", ActorKind::Human),
            ),
            ("junior-j".to_string(), member("junior", ActorKind::Human)),
            ("middle-m".to_string(), member("middle", ActorKind::Human)),
            ("senior-s".to_string(), member("senior", ActorKind::Human)),
            ("bot".to_string(), member("senior", ActorKind::Agent)),
            ("agent-a".to_string(), member("senior", ActorKind::Agent)),
        ]),
        ..TenantPolicy::default()
    }
}
fn seen_meta(path: &str, role: &str) -> mnemosyne_content::Change {
    mnemosyne_content::Change::SetMeta {
        path: path.into(),
        seen_from: Some(role.into()),
        colour: None,
    }
}
fn colour_meta(path: &str, name: &str) -> mnemosyne_content::Change {
    mnemosyne_content::Change::SetMeta {
        path: path.into(),
        seen_from: None,
        colour: Some(name.into()),
    }
}
/// The root admin raises `path` to `role` at head with a real `set_meta` edit.
fn hide(
    s: &Service,
    root: &TrustedIdentity,
    path: &str,
    role: &str,
) -> mnemosyne_content::Revision {
    let head = s.head(root).unwrap();
    s.edit(
        root,
        &head,
        vec![seen_meta(path, role)],
        &format!("hide {path}"),
        None,
        None,
    )
    .unwrap()
}
fn put(path: &str, content: &str) -> mnemosyne_content::Change {
    mnemosyne_content::Change::Put {
        path: path.into(),
        content: content.into(),
    }
}
fn folder(path: &str) -> mnemosyne_content::Change {
    mnemosyne_content::Change::CreateFolder { path: path.into() }
}
fn code<T: std::fmt::Debug>(r: Result<T, mnemosyne_content::ContentError>) -> String {
    r.unwrap_err().code
}
/// Tenant "t1" with the ranked members and only root's Admin grant.
fn ranked_fixture() -> (Fixture, Service, TrustedIdentity) {
    let fx = Fixture::new();
    let s = fx.service();
    let root = identity("t1", "root", ActorKind::Human);
    s.bootstrap_policy(policy_with(&root, "t1", ranked_section()))
        .unwrap();
    (fx, s, root)
}

#[test]
fn rank_matrix_governs_read_propose_edit_approve() {
    let (_fx, s, root) = ranked_fixture();
    s.edit(
        &root,
        "r0",
        vec![
            put("public.md", "public"),
            folder("senior-only"),
            put("senior-only/pay.md", "pay"),
        ],
        "seed",
        None,
        None,
    )
    .unwrap();
    hide(&s, &root, "senior-only", "senior");
    let head = s.head(&root).unwrap();
    let junior = identity("t1", "junior-j", ActorKind::Human);
    let middle = identity("t1", "middle-m", ActorKind::Human);
    let senior = identity("t1", "senior-s", ActorKind::Human);
    let bot = TrustedIdentity {
        on_behalf_of: Some("middle-m".into()),
        ..identity("t1", "bot", ActorKind::Agent)
    };

    // junior: reads what the root shows, nothing under senior-only, cannot write.
    assert_eq!(
        s.read(&junior, "public.md", None).unwrap().content,
        "public"
    );
    assert_eq!(
        code(s.read(&junior, "senior-only/pay.md", None)),
        "not_found"
    );
    assert_eq!(s.list(&junior, "").unwrap(), vec!["public.md".to_string()]);
    assert_eq!(
        code(s.propose(
            &junior,
            &head,
            vec![put("public.md", "j")],
            "j1",
            None,
            None
        )),
        "forbidden"
    );
    assert_eq!(
        code(s.edit(
            &junior,
            &head,
            vec![put("public.md", "j")],
            "j2",
            None,
            None
        )),
        "forbidden"
    );

    // middle: proposes, cannot publish, cannot reach hidden paths.
    let p = s
        .propose(
            &middle,
            &head,
            vec![put("public.md", "m")],
            "m1",
            None,
            None,
        )
        .unwrap();
    assert_eq!(p.state, mnemosyne_content::ProposalState::Pending);
    assert_eq!(
        code(s.edit(
            &middle,
            &head,
            vec![put("public.md", "m")],
            "m2",
            None,
            None
        )),
        "forbidden"
    );
    assert_eq!(
        code(s.propose(
            &middle,
            &head,
            vec![put("senior-only/pay.md", "m")],
            "m3",
            None,
            None
        )),
        "not_found"
    );

    // senior: approves, reads hidden paths, publishes.
    let approved = s.approve(&senior, &p.id, &p.digest, "s1", None).unwrap();
    assert_eq!(approved.state, mnemosyne_content::ProposalState::Approved);
    assert_eq!(
        s.read(&senior, "senior-only/pay.md", None).unwrap().content,
        "pay"
    );
    s.edit(
        &senior,
        &head,
        vec![put("public.md", "s")],
        "s2",
        None,
        None,
    )
    .unwrap();
    let head = s.head(&root).unwrap();

    // bot: reads by rank, never writes from rank, bounded by middle-m's reach.
    assert_eq!(s.read(&bot, "public.md", None).unwrap().content, "s");
    assert_eq!(
        code(s.propose(&bot, &head, vec![put("public.md", "b")], "b1", None, None)),
        "forbidden"
    );
    // `edit` refuses agents before any permission check (existing contract).
    assert_eq!(
        code(s.edit(&bot, &head, vec![put("public.md", "b")], "b2", None, None)),
        "human_required"
    );
    assert_eq!(
        code(s.approve(&bot, &p.id, &p.digest, "b3", None)),
        "human_required"
    );
    assert_eq!(code(s.read(&bot, "senior-only/pay.md", None)), "not_found");
    // An agent member without grants or a delegating human never writes from rank.
    let agent_a = identity("t1", "agent-a", ActorKind::Agent);
    assert_eq!(s.read(&agent_a, "public.md", None).unwrap().content, "s");
    assert_eq!(
        code(s.propose(
            &agent_a,
            &head,
            vec![put("public.md", "a")],
            "a1",
            None,
            None
        )),
        "forbidden"
    );

    // An explicit Propose grant lets the bot propose where middle-m may.
    s.replace_policy(
        &root,
        Policy {
            grants: vec![
                grant(&root, Permission::Admin, ""),
                grant(&bot, Permission::Propose, ""),
            ],
            tenants: BTreeMap::from([("t1".to_string(), ranked_section())]),
        },
        None,
    )
    .unwrap();
    let bp = s
        .propose(&bot, &head, vec![put("public.md", "b")], "b4", None, None)
        .unwrap();
    assert_eq!(bp.state, mnemosyne_content::ProposalState::Pending);
    assert_eq!(
        code(s.propose(
            &bot,
            &head,
            vec![put("senior-only/pay.md", "b")],
            "b5",
            None,
            None
        )),
        "not_found"
    );
}

#[test]
fn credential_kind_must_match_membership() {
    let fx = Fixture::new();
    let s = fx.service();
    let root = identity("t1", "root", ActorKind::Human);
    let section = TenantPolicy {
        members: BTreeMap::from([("x".to_string(), member("senior", ActorKind::Human))]),
        ..TenantPolicy::default()
    };
    s.bootstrap_policy(policy_with(&root, "t1", section))
        .unwrap();
    let x = identity("t1", "x", ActorKind::Agent);
    assert_eq!(code(s.head(&x)), "policy_mismatch");
    assert_eq!(code(s.read(&x, "a.md", None)), "policy_mismatch");
    assert_eq!(code(s.list(&x, "")), "policy_mismatch");
    assert_eq!(
        code(s.propose(&x, "r0", vec![put("a.md", "x")], "x1", None, None)),
        "policy_mismatch"
    );
}

#[test]
fn explicit_grant_still_wins_without_membership() {
    let fx = Fixture::new();
    let s = fx.service();
    let root = identity("t1", "root", ActorKind::Human);
    let legacy = identity("t1", "legacy", ActorKind::Human);
    s.bootstrap_policy(Policy {
        grants: vec![
            grant(&root, Permission::Admin, ""),
            grant(&root, Permission::Edit, ""),
            grant(&root, Permission::Read, ""),
            grant(&legacy, Permission::Read, "public"),
        ],
        tenants: BTreeMap::new(),
    })
    .unwrap();
    s.edit(
        &root,
        "r0",
        vec![put("public/a.md", "a"), put("private/b.md", "b")],
        "seed",
        None,
        None,
    )
    .unwrap();
    assert_eq!(s.read(&legacy, "public/a.md", None).unwrap().content, "a");
    assert_eq!(code(s.read(&legacy, "private/b.md", None)), "forbidden");
}

#[test]
fn history_of_a_vanished_path_uses_nearest_ancestor_at_head() {
    let (_fx, s, root) = ranked_fixture();
    s.edit(
        &root,
        "r0",
        vec![folder("senior-only"), put("senior-only/old.md", "old")],
        "h1",
        None,
        None,
    )
    .unwrap();
    let hidden = hide(&s, &root, "senior-only", "senior");
    let r2 = s
        .edit(
            &root,
            &hidden.id,
            vec![mnemosyne_content::Change::Rename {
                path: "senior-only/old.md".into(),
                to: "senior-only/new.md".into(),
            }],
            "h2",
            None,
            None,
        )
        .unwrap();
    let r3 = s
        .edit(&root, &r2.id, vec![put("tmp.md", "tmp")], "h3", None, None)
        .unwrap();
    let r4 = s
        .edit(
            &root,
            &r3.id,
            vec![mnemosyne_content::Change::Delete {
                path: "tmp.md".into(),
            }],
            "h4",
            None,
            None,
        )
        .unwrap();
    let junior = identity("t1", "junior-j", ActorKind::Human);
    let middle = identity("t1", "middle-m", ActorKind::Human);
    let senior = identity("t1", "senior-s", ActorKind::Human);

    assert_eq!(code(s.history(&junior, "senior-only/old.md")), "not_found");
    // Existing and never-existing hidden paths answer alike.
    assert_eq!(code(s.history(&junior, "senior-only/new.md")), "not_found");
    assert_eq!(
        code(s.history(&junior, "senior-only/ghost.md")),
        "not_found"
    );
    assert_eq!(code(s.history(&middle, "senior-only/old.md")), "not_found");
    assert!(!s.history(&senior, "senior-only/old.md").unwrap().is_empty());

    let tmp = s.history(&junior, "tmp.md").unwrap();
    let ids: Vec<&str> = tmp.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, vec![r4.id.as_str(), r3.id.as_str()]);
    assert!(tmp[0].changes[0].deleted);
    assert!(!tmp[1].changes[0].deleted);
}

#[test]
fn hidden_file_in_visible_folder_answers_like_a_missing_one() {
    let (_fx, s, root) = ranked_fixture();
    s.edit(
        &root,
        "r0",
        vec![
            folder("public"),
            put("public/open.md", "open"),
            put("public/secret.md", "secret"),
        ],
        "seed",
        None,
        None,
    )
    .unwrap();
    hide(&s, &root, "public/secret.md", "senior");
    let junior = identity("t1", "junior-j", ActorKind::Human);
    let senior = identity("t1", "senior-s", ActorKind::Human);
    assert_eq!(code(s.read(&junior, "public/secret.md", None)), "not_found");
    assert_eq!(code(s.read(&junior, "public/ghost.md", None)), "not_found");
    assert_eq!(
        code(s.read_binary(&junior, "public/secret.md", None, false, None)),
        "not_found"
    );
    assert_eq!(
        code(s.read_binary(&junior, "public/ghost.md", None, false, None)),
        "not_found"
    );
    assert_eq!(
        s.list(&junior, "public").unwrap(),
        vec!["public".to_string(), "public/open.md".to_string()]
    );
    // Like `public/ghost.md`: the folder is readable, so the history is empty.
    assert!(s.history(&junior, "public/secret.md").unwrap().is_empty());
    assert_eq!(
        s.read(&senior, "public/secret.md", None).unwrap().content,
        "secret"
    );
}

#[test]
fn deleting_a_hidden_file_keeps_its_past_hidden() {
    let (_fx, s, root) = ranked_fixture();
    // Hidden before pay.md exists, so every manifest holding pay.md hides it.
    s.edit(&root, "r0", vec![folder("senior-only")], "d1", None, None)
        .unwrap();
    let hidden = hide(&s, &root, "senior-only", "senior");
    let r1 = s
        .edit(
            &root,
            &hidden.id,
            vec![put("senior-only/pay.md", "pay")],
            "d1b",
            None,
            None,
        )
        .unwrap();
    s.edit(
        &root,
        &r1.id,
        vec![mnemosyne_content::Change::Delete {
            path: "senior-only/pay.md".into(),
        }],
        "d2",
        None,
        None,
    )
    .unwrap();
    let junior = identity("t1", "junior-j", ActorKind::Human);
    let senior = identity("t1", "senior-s", ActorKind::Human);
    assert_eq!(code(s.history(&junior, "senior-only/pay.md")), "not_found");
    assert_eq!(code(s.diff(&junior, &r1.id)), "not_found");
    assert_eq!(
        code(s.read(&junior, "senior-only/pay.md", Some(&r1.id))),
        "not_found"
    );
    assert_eq!(s.history(&senior, "senior-only/pay.md").unwrap().len(), 2);
    assert!(!s.diff(&senior, &r1.id).unwrap().is_empty());
    assert_eq!(
        s.read(&senior, "senior-only/pay.md", Some(&r1.id))
            .unwrap()
            .content,
        "pay"
    );
}

#[test]
fn deleting_a_hidden_folder_keeps_its_past_hidden() {
    let (_fx, s, root) = ranked_fixture();
    // Hidden before pay.md exists, so every manifest holding pay.md hides it.
    s.edit(&root, "r0", vec![folder("senior-only")], "f1", None, None)
        .unwrap();
    let hidden = hide(&s, &root, "senior-only", "senior");
    let r1 = s
        .edit(
            &root,
            &hidden.id,
            vec![put("senior-only/pay.md", "pay")],
            "f1b",
            None,
            None,
        )
        .unwrap();
    let r2 = s
        .edit(
            &root,
            &r1.id,
            vec![mnemosyne_content::Change::Delete {
                path: "senior-only".into(),
            }],
            "f2",
            None,
            None,
        )
        .unwrap();
    let junior = identity("t1", "junior-j", ActorKind::Human);
    let senior = identity("t1", "senior-s", ActorKind::Human);
    // Nothing at head hides the old names any more; the revision-time manifests still do,
    // and the answer matches a name that never existed.
    assert!(s.history(&junior, "senior-only/pay.md").unwrap().is_empty());
    assert!(s
        .history(&junior, "senior-only/ghost.md")
        .unwrap()
        .is_empty());
    assert_eq!(code(s.diff(&junior, &r1.id)), "not_found");
    assert_eq!(code(s.diff(&junior, &r2.id)), "not_found");
    assert_eq!(
        code(s.read(&junior, "senior-only/pay.md", Some(&r1.id))),
        "not_found"
    );
    assert_eq!(
        code(s.read_binary(&junior, "senior-only/pay.md", Some(&r1.id), false, None)),
        "not_found"
    );
    assert_eq!(s.history(&senior, "senior-only/pay.md").unwrap().len(), 2);
    assert_eq!(
        s.read(&senior, "senior-only/pay.md", Some(&r1.id))
            .unwrap()
            .content,
        "pay"
    );
}

#[test]
fn senior_root_hides_the_tenant_from_juniors() {
    let fx = Fixture::new();
    let s = fx.service();
    let root = identity("t1", "root", ActorKind::Human);
    let section = TenantPolicy {
        root_seen_from: "senior".into(),
        ..ranked_section()
    };
    s.bootstrap_policy(policy_with(&root, "t1", section))
        .unwrap();
    let junior = identity("t1", "junior-j", ActorKind::Human);
    assert_eq!(code(s.head(&junior)), "forbidden");
    assert_eq!(code(s.list(&junior, "")), "forbidden");
}

#[test]
fn proposals_on_hidden_paths_are_invisible_to_juniors() {
    let (_fx, s, root) = ranked_fixture();
    s.edit(
        &root,
        "r0",
        vec![
            put("public.md", "public"),
            folder("senior-only"),
            put("senior-only/pay.md", "pay"),
        ],
        "seed",
        None,
        None,
    )
    .unwrap();
    let r1 = hide(&s, &root, "senior-only", "senior");
    let senior = identity("t1", "senior-s", ActorKind::Human);
    let junior = identity("t1", "junior-j", ActorKind::Human);
    let hidden = s
        .propose(
            &senior,
            &r1.id,
            vec![put("senior-only/pay.md", "more")],
            "p1",
            None,
            None,
        )
        .unwrap();
    let open = s
        .propose(
            &senior,
            &r1.id,
            vec![put("public.md", "more")],
            "p2",
            None,
            None,
        )
        .unwrap();
    assert_eq!(code(s.review(&junior, &hidden.id)), "not_found");
    assert_eq!(s.review(&junior, &open.id).unwrap().id, open.id);
    let listed: Vec<String> = s
        .list_proposals(&junior, &Default::default())
        .unwrap()
        .proposals
        .into_iter()
        .map(|p| p.id)
        .collect();
    assert_eq!(listed, vec![open.id.clone()]);
}

fn search_request(
    q: &str,
    snapshot_id: Option<String>,
) -> mnemosyne_content::search::SearchRequest {
    mnemosyne_content::search::SearchRequest {
        query: q.into(),
        mode: Default::default(),
        min_score: None,
        scope: String::new(),
        extensions: vec![],
        limit: 20,
        offset: 0,
        snapshot_id,
        fields: mnemosyne_content::search::SearchField::all(),
        typos: true,
    }
}

#[test]
fn hidden_paths_do_not_leak_through_search_proposals_export_or_extraction() {
    let (_fx, s, root) = ranked_fixture();
    s.edit(
        &root,
        "r0",
        vec![
            put("public.md", "warranty three years"),
            folder("senior-only"),
            put("senior-only/pay.md", "warranty bonus"),
        ],
        "seed",
        None,
        None,
    )
    .unwrap();
    let r1 = hide(&s, &root, "senior-only", "senior");
    let senior = identity("t1", "senior-s", ActorKind::Human);
    let junior = identity("t1", "junior-j", ActorKind::Human);
    s.propose(
        &senior,
        &r1.id,
        vec![put("senior-only/pay.md", "warranty more")],
        "secret proposal summary",
        None,
        None,
    )
    .unwrap();

    let found = s
        .search(&junior, &search_request("warranty", None))
        .unwrap();
    assert_eq!(found.total, 1);
    assert_eq!(found.hits.len(), 1);
    assert_eq!(found.hits[0].path, "public.md");
    assert_eq!(
        s.search(&senior, &search_request("warranty", None))
            .unwrap()
            .total,
        2
    );

    let page = s.list_proposals(&junior, &Default::default()).unwrap();
    assert!(page.proposals.is_empty());
    assert_eq!(page.next_offset, None);

    let export = s.export_snapshot(&junior, "", None, 0, 32, None).unwrap();
    let paths: Vec<_> = export.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, vec!["public.md"]);

    assert_eq!(
        code(s.properties(&junior, "senior-only/pay.md", None)),
        "not_found"
    );
    assert_eq!(
        code(s.outline(&junior, "senior-only/pay.md", None)),
        "not_found"
    );
    assert_eq!(
        code(s.read_section(&junior, "senior-only/pay.md", None, Some("s1"), None)),
        "not_found"
    );
    assert_eq!(
        code(s.extraction_status(&junior, "senior-only/data.pdf")),
        "not_found"
    );
}

#[test]
fn policy_change_takes_effect_on_the_next_request() {
    let (_fx, s, root) = ranked_fixture();
    s.edit(
        &root,
        "r0",
        vec![
            put("public.md", "warranty three years"),
            folder("senior-only"),
            put("senior-only/pay.md", "warranty bonus"),
        ],
        "seed",
        None,
        None,
    )
    .unwrap();
    let r1 = hide(&s, &root, "senior-only", "senior");
    let senior = identity("t1", "senior-s", ActorKind::Human);
    let middle = identity("t1", "middle-m", ActorKind::Human);
    let p = s
        .propose(
            &middle,
            &r1.id,
            vec![put("public.md", "changed")],
            "p",
            None,
            None,
        )
        .unwrap();
    s.approve(&senior, &p.id, &p.digest, "k1", None).unwrap();
    assert!(s
        .list(&senior, "")
        .unwrap()
        .contains(&"senior-only/pay.md".to_string()));
    let before = s
        .search(&senior, &search_request("warranty", None))
        .unwrap();
    assert_eq!(before.total, 2);

    let mut section = ranked_section();
    section
        .members
        .insert("senior-s".into(), member("junior", ActorKind::Human));
    let state = s.policy_state(&root).unwrap();
    s.replace_policy(
        &root,
        policy_with(&root, "t1", section),
        Some(&state.digest),
    )
    .unwrap();

    assert!(!s
        .list(&senior, "")
        .unwrap()
        .iter()
        .any(|p| p.starts_with("senior-only")));
    assert_eq!(
        s.review(&root, &p.id).unwrap().state,
        mnemosyne_content::ProposalState::Pending
    );
    assert_eq!(
        code(s.search(
            &senior,
            &search_request("warranty", Some(before.snapshot_id))
        )),
        "stale_search"
    );
}

#[test]
fn historical_export_hides_what_was_hidden_then() {
    let (_fx, s, root) = ranked_fixture();
    s.edit(
        &root,
        "r0",
        vec![put("public.md", "public"), folder("senior-only")],
        "seed",
        None,
        None,
    )
    .unwrap();
    let hidden = hide(&s, &root, "senior-only", "senior");
    let r1 = s
        .edit(
            &root,
            &hidden.id,
            vec![put("senior-only/pay.md", "pay")],
            "pay",
            None,
            None,
        )
        .unwrap();
    s.edit(
        &root,
        &r1.id,
        vec![mnemosyne_content::Change::Delete {
            path: "senior-only".into(),
        }],
        "drop",
        None,
        None,
    )
    .unwrap();
    let junior = identity("t1", "junior-j", ActorKind::Human);
    let senior = identity("t1", "senior-s", ActorKind::Human);
    let paths = |who: &TrustedIdentity, scope: &str| -> Vec<String> {
        s.export_snapshot(who, scope, Some(&r1.id), 0, 32, None)
            .unwrap()
            .files
            .into_iter()
            .map(|f| f.path)
            .collect()
    };
    assert_eq!(paths(&junior, ""), vec!["public.md"]);
    let all = paths(&senior, "");
    assert!(all.contains(&"senior-only/pay.md".to_string()));
    assert_eq!(
        code(s.export_snapshot(&junior, "senior-only", Some(&r1.id), 0, 32, None)),
        "not_found"
    );
}

#[test]
fn extraction_respects_ranks_for_status_and_retry() {
    let (_fx, s, root) = ranked_fixture();
    s.edit(
        &root,
        "r0",
        vec![
            folder("senior-only"),
            mnemosyne_content::Change::PutBinary {
                path: "senior-only/data.pdf".into(),
                bytes: b"not a pdf".to_vec(),
            },
            mnemosyne_content::Change::PutBinary {
                path: "open.pdf".into(),
                bytes: b"not a pdf".to_vec(),
            },
        ],
        "seed",
        None,
        None,
    )
    .unwrap();
    hide(&s, &root, "senior-only", "senior");
    let junior = identity("t1", "junior-j", ActorKind::Human);
    let middle = identity("t1", "middle-m", ActorKind::Human);
    let senior = identity("t1", "senior-s", ActorKind::Human);
    assert_eq!(
        code(s.extraction_status(&junior, "senior-only/data.pdf")),
        "not_found"
    );
    assert_ne!(
        s.extraction_status(&senior, "senior-only/data.pdf")
            .err()
            .map(|e| e.code),
        Some("not_found".into())
    );
    assert_eq!(code(s.retry_extraction(&middle, "open.pdf")), "forbidden");
    assert_ne!(
        s.retry_extraction(&senior, "open.pdf")
            .err()
            .map(|e| e.code),
        Some("forbidden".into())
    );
}

// ---- the set_meta change ----

fn meta(seen_from: Option<&str>, colour: Option<&str>) -> Option<PathMeta> {
    Some(PathMeta {
        seen_from: seen_from.map(String::from),
        colour: colour.map(String::from),
    })
}
fn rename(path: &str, to: &str) -> mnemosyne_content::Change {
    mnemosyne_content::Change::Rename {
        path: path.into(),
        to: to.into(),
    }
}

#[test]
fn set_meta_sets_and_inherits_visibility_and_colour() {
    let (_fx, s, root) = ranked_fixture();
    let seed = s
        .edit(
            &root,
            "r0",
            vec![
                put("public.md", "public"),
                folder("senior-only"),
                put("senior-only/pay.md", "pay"),
            ],
            "seed",
            None,
            None,
        )
        .unwrap();
    let junior = identity("t1", "junior-j", ActorKind::Human);
    let middle = identity("t1", "middle-m", ActorKind::Human);
    let senior = identity("t1", "senior-s", ActorKind::Human);

    let r = s
        .edit(
            &senior,
            &seed.id,
            vec![seen_meta("senior-only", "senior")],
            "s1",
            None,
            None,
        )
        .unwrap();
    assert_eq!(r.changes.len(), 1);
    assert_eq!(r.changes[0].path, "senior-only");
    assert_eq!(r.changes[0].meta_before, meta(None, None));
    assert_eq!(r.changes[0].meta_after, meta(Some("senior"), None));
    // The child inherits: junior sees neither the folder nor the file.
    assert_eq!(s.list(&junior, "").unwrap(), vec!["public.md".to_string()]);
    assert_eq!(
        code(s.read(&junior, "senior-only/pay.md", None)),
        "not_found"
    );
    assert_eq!(
        s.read(&senior, "senior-only/pay.md", None).unwrap().content,
        "pay"
    );

    let r = s
        .edit(
            &senior,
            &r.id,
            vec![colour_meta("senior-only", "cyan")],
            "s2",
            None,
            None,
        )
        .unwrap();
    assert_eq!(r.changes[0].meta_after, meta(Some("senior"), Some("cyan")));

    // A text file's meta-only revision keeps its text on both sides.
    let r = s
        .edit(
            &senior,
            &r.id,
            vec![colour_meta("public.md", "green")],
            "s3",
            None,
            None,
        )
        .unwrap();
    assert_eq!(r.changes[0].before.as_deref(), Some("public"));
    assert_eq!(r.changes[0].after, "public");
    assert_eq!(r.changes[0].meta_before, meta(None, None));
    assert_eq!(r.changes[0].meta_after, meta(None, Some("green")));
    // Diffs without meta stay as they were on the wire.
    let plain = s
        .edit(
            &senior,
            &r.id,
            vec![put("plain.md", "plain")],
            "s4",
            None,
            None,
        )
        .unwrap();
    let wire = serde_json::to_value(&plain.changes[0]).unwrap();
    assert!(wire.get("meta_before").is_none() && wire.get("meta_after").is_none());

    // middle: below visibility_from and below publish_from.
    let head = s.head(&root).unwrap();
    assert_eq!(
        code(s.edit(
            &middle,
            &head,
            vec![seen_meta("public.md", "middle")],
            "m1",
            None,
            None
        )),
        "forbidden"
    );
    assert_eq!(
        code(s.edit(
            &middle,
            &head,
            vec![colour_meta("public.md", "cyan")],
            "m2",
            None,
            None
        )),
        "forbidden"
    );

    // Lowering below one's own rank is allowed.
    s.edit(
        &senior,
        &head,
        vec![seen_meta("senior-only", "middle")],
        "s5",
        None,
        None,
    )
    .unwrap();
    assert_eq!(
        s.read(&middle, "senior-only/pay.md", None).unwrap().content,
        "pay"
    );
}

#[test]
fn visibility_rung_replaces_the_edit_check_for_seen_from() {
    let fx = Fixture::new();
    let s = fx.service();
    let root = identity("t1", "root", ActorKind::Human);
    let mut section = ranked_section();
    section.rungs.publish_from = "administrator".into();
    s.bootstrap_policy(policy_with(&root, "t1", section))
        .unwrap();
    let senior = identity("t1", "senior-s", ActorKind::Human);
    let seed = s
        .edit(
            &root,
            "r0",
            vec![put("public.md", "public")],
            "seed",
            None,
            None,
        )
        .unwrap();
    let r = s
        .edit(
            &senior,
            &seed.id,
            vec![seen_meta("public.md", "senior")],
            "v1",
            None,
            None,
        )
        .unwrap();
    assert_eq!(r.changes[0].meta_after, meta(Some("senior"), None));
    assert_eq!(
        code(s.edit(
            &senior,
            &r.id,
            vec![colour_meta("public.md", "cyan")],
            "v2",
            None,
            None
        )),
        "forbidden"
    );
}

#[test]
fn set_meta_rules() {
    let (_fx, s, root) = ranked_fixture();
    s.edit(
        &root,
        "r0",
        vec![
            folder("senior-only"),
            put("senior-only/pay.md", "pay"),
            put("open.md", "open"),
            put("pub.md", "pub"),
            put("plain.md", "plain"),
            folder("a"),
            put("a/x.md", "x"),
        ],
        "seed",
        None,
        None,
    )
    .unwrap();
    hide(&s, &root, "senior-only", "senior");
    let junior = identity("t1", "junior-j", ActorKind::Human);
    let middle = identity("t1", "middle-m", ActorKind::Human);
    let senior = identity("t1", "senior-s", ActorKind::Human);
    let head = || s.head(&root).unwrap();

    // Nobody raises a path above their own rank, except an Admin grant holder.
    assert_eq!(
        code(s.edit(
            &senior,
            &head(),
            vec![seen_meta("open.md", "administrator")],
            "k1",
            None,
            None
        )),
        "visibility_above_own_rank"
    );
    s.edit(
        &root,
        &head(),
        vec![seen_meta("open.md", "administrator")],
        "k2",
        None,
        None,
    )
    .unwrap();
    assert_eq!(code(s.read(&senior, "open.md", None)), "not_found");

    assert_eq!(
        code(s.edit(
            &senior,
            &head(),
            vec![seen_meta("pub.md", "ceo")],
            "k3",
            None,
            None
        )),
        "invalid_policy"
    );
    assert_eq!(
        code(s.edit(
            &senior,
            &head(),
            vec![colour_meta("pub.md", "pink")],
            "k4",
            None,
            None
        )),
        "invalid_policy"
    );
    let none = mnemosyne_content::Change::SetMeta {
        path: "pub.md".into(),
        seen_from: None,
        colour: None,
    };
    assert_eq!(
        code(s.edit(&senior, &head(), vec![none], "k5", None, None)),
        "invalid_request"
    );
    assert_eq!(
        code(s.edit(
            &senior,
            &head(),
            vec![seen_meta("ghost.md", "senior")],
            "k6",
            None,
            None
        )),
        "not_found"
    );

    // Never inside a proposal, whoever proposes it.
    assert_eq!(
        code(s.propose(
            &senior,
            &head(),
            vec![seen_meta("pub.md", "senior")],
            "k7",
            None,
            None
        )),
        "set_meta_requires_edit"
    );
    assert_eq!(
        code(s.propose(
            &middle,
            &head(),
            vec![colour_meta("pub.md", "cyan")],
            "k8",
            None,
            None
        )),
        "set_meta_requires_edit"
    );

    // A child is never visible below its folder.
    assert_eq!(
        code(s.edit(
            &senior,
            &head(),
            vec![seen_meta("senior-only/pay.md", "junior")],
            "k9",
            None,
            None
        )),
        "visibility_below_folder"
    );
    s.edit(
        &senior,
        &head(),
        vec![seen_meta("a", "junior")],
        "k10",
        None,
        None,
    )
    .unwrap();
    s.edit(
        &senior,
        &head(),
        vec![seen_meta("a/x.md", "junior")],
        "k11",
        None,
        None,
    )
    .unwrap();
    assert_eq!(
        code(s.edit(
            &senior,
            &head(),
            vec![seen_meta("a", "senior")],
            "k12",
            None,
            None
        )),
        "visibility_below_folder"
    );

    // A move carries explicit meta along and is checked at its destination.
    s.edit(
        &senior,
        &head(),
        vec![seen_meta("pub.md", "junior")],
        "k13",
        None,
        None,
    )
    .unwrap();
    assert_eq!(
        code(s.edit(
            &senior,
            &head(),
            vec![rename("pub.md", "senior-only/pub.md")],
            "k14",
            None,
            None
        )),
        "visibility_below_folder"
    );
    s.edit(
        &senior,
        &head(),
        vec![rename("plain.md", "senior-only/plain.md")],
        "k15",
        None,
        None,
    )
    .unwrap();
    assert_eq!(
        code(s.read(&junior, "senior-only/plain.md", None)),
        "not_found"
    );

    // An Admin grant holder bypasses the rung and the own-rank rule.
    s.replace_policy(
        &root,
        Policy {
            grants: vec![
                grant(&root, Permission::Admin, ""),
                grant(&middle, Permission::Admin, ""),
            ],
            tenants: BTreeMap::from([("t1".to_string(), ranked_section())]),
        },
        None,
    )
    .unwrap();
    s.edit(
        &middle,
        &head(),
        vec![seen_meta("a/x.md", "administrator")],
        "k16",
        None,
        None,
    )
    .unwrap();
    assert_eq!(code(s.read(&senior, "a/x.md", None)), "not_found");

    // v1 callers never see set_meta.
    let response = s.execute(
        &root,
        mnemosyne_content::api::Request {
            version: 1,
            idempotency_key: Some("v1".into()),
            operation: mnemosyne_content::api::Operation::Edit {
                base_revision: head(),
                changes: vec![seen_meta("pub.md", "senior")],
                message: None,
                origin: None,
            },
        },
    );
    assert_eq!(response.error.unwrap().code, "unsupported_version");
    let wire: mnemosyne_content::Change =
        serde_json::from_str(r#"{"type":"set_meta","path":"pub.md","colour":"blue"}"#).unwrap();
    assert_eq!(wire, colour_meta("pub.md", "blue"));
}

// ---- restore restores content and identity only ----

fn restore(path: &str, revision_id: &str) -> mnemosyne_content::Change {
    mnemosyne_content::Change::Restore {
        path: path.into(),
        revision_id: revision_id.into(),
    }
}

#[test]
fn restore_keeps_the_current_stricter_meta() {
    let (_fx, s, root) = ranked_fixture();
    let junior = identity("t1", "junior-j", ActorKind::Human);
    let senior = identity("t1", "senior-s", ActorKind::Human);
    let seed = s
        .edit(
            &root,
            "r0",
            vec![folder("hr"), put("hr/pay.md", "pay")],
            "seed",
            None,
            None,
        )
        .unwrap();
    let r1 = s
        .edit(
            &senior,
            &seed.id,
            vec![seen_meta("hr/pay.md", "junior")],
            "r1",
            None,
            None,
        )
        .unwrap();
    hide(&s, &root, "hr/pay.md", "senior");
    hide(&s, &root, "hr", "senior");
    let head = s.head(&root).unwrap();
    let r = s
        .edit(
            &senior,
            &head,
            vec![restore("hr/pay.md", &r1.id)],
            "restore",
            None,
            None,
        )
        .unwrap();
    assert_eq!(r.changes[0].meta_after, meta(Some("senior"), None));
    assert_eq!(code(s.read(&junior, "hr/pay.md", None)), "not_found");
    assert_eq!(code(s.read(&junior, "hr", None)), "not_found");
}

#[test]
fn restore_by_a_publisher_below_the_visibility_rung_leaves_visibility_alone() {
    let fx = Fixture::new();
    let s = fx.service();
    let root = identity("t1", "root", ActorKind::Human);
    let mut section = ranked_section();
    section.rungs.publish_from = "middle".into();
    s.bootstrap_policy(policy_with(&root, "t1", section))
        .unwrap();
    let junior = identity("t1", "junior-j", ActorKind::Human);
    let middle = identity("t1", "middle-m", ActorKind::Human);
    let r1 = s
        .edit(&root, "r0", vec![put("x.md", "old")], "r1", None, None)
        .unwrap();
    hide(&s, &root, "x.md", "middle");
    let head = s.head(&root).unwrap();
    let r3 = s
        .edit(&root, &head, vec![put("x.md", "new")], "r3", None, None)
        .unwrap();
    let r = s
        .edit(
            &middle,
            &r3.id,
            vec![restore("x.md", &r1.id)],
            "restore",
            None,
            None,
        )
        .unwrap();
    assert_eq!(r.changes[0].meta_after, meta(Some("middle"), None));
    assert_eq!(s.read(&middle, "x.md", None).unwrap().content, "old");
    assert_eq!(code(s.read(&junior, "x.md", None)), "not_found");
}

#[test]
fn restore_inside_a_proposal_leaves_visibility_alone() {
    let (_fx, s, root) = ranked_fixture();
    let junior = identity("t1", "junior-j", ActorKind::Human);
    let middle = identity("t1", "middle-m", ActorKind::Human);
    let senior = identity("t1", "senior-s", ActorKind::Human);
    let r1 = s
        .edit(&root, "r0", vec![put("x.md", "old")], "r1", None, None)
        .unwrap();
    hide(&s, &root, "x.md", "middle");
    let head = s.head(&root).unwrap();
    let r3 = s
        .edit(&root, &head, vec![put("x.md", "new")], "r3", None, None)
        .unwrap();
    let p = s
        .propose(
            &middle,
            &r3.id,
            vec![restore("x.md", &r1.id)],
            "p",
            None,
            None,
        )
        .unwrap();
    assert_eq!(p.diff[0].meta_after, meta(Some("middle"), None));
    s.approve(&senior, &p.id, &p.digest, "a", None).unwrap();
    let r = s.apply(&middle, &p.id, &p.digest, "x").unwrap();
    assert_eq!(r.changes[0].meta_after, meta(Some("middle"), None));
    assert_eq!(s.read(&middle, "x.md", None).unwrap().content, "old");
    assert_eq!(code(s.read(&junior, "x.md", None)), "not_found");
}

#[test]
fn set_meta_ruling_edges() {
    let (_fx, s, root) = ranked_fixture();
    s.edit(
        &root,
        "r0",
        vec![
            folder("senior-only"),
            put("hidden.md", "hidden"),
            folder("top"),
            folder("b"),
            put("b/y.md", "y"),
            put("public.md", "public"),
        ],
        "seed",
        None,
        None,
    )
    .unwrap();
    hide(&s, &root, "senior-only", "senior");
    hide(&s, &root, "hidden.md", "administrator");
    hide(&s, &root, "b/y.md", "junior");
    let middle = identity("t1", "middle-m", ActorKind::Human);
    let senior = identity("t1", "senior-s", ActorKind::Human);
    let head = || s.head(&root).unwrap();

    // A path hidden from the caller answers like a missing one.
    assert_eq!(
        code(s.edit(
            &senior,
            &head(),
            vec![colour_meta("hidden.md", "cyan")],
            "e1",
            None,
            None
        )),
        "not_found"
    );
    assert_eq!(
        code(s.edit(
            &senior,
            &head(),
            vec![seen_meta("hidden.md", "senior")],
            "e2",
            None,
            None
        )),
        "not_found"
    );
    // The tenant root is not a change path; its visibility lives in the policy.
    assert_eq!(
        code(s.edit(
            &root,
            &head(),
            vec![seen_meta("", "senior")],
            "e3",
            None,
            None
        )),
        "invalid_path"
    );
    // A top-level entry is compared against root_seen_from.
    assert_eq!(
        code(s.edit(
            &senior,
            &head(),
            vec![seen_meta("top", "worker")],
            "e4",
            None,
            None
        )),
        "visibility_below_folder"
    );
    // A folder move is checked for every explicit descendant.
    assert_eq!(
        code(s.edit(
            &senior,
            &head(),
            vec![rename("b", "senior-only/b")],
            "e5",
            None,
            None
        )),
        "visibility_below_folder"
    );
    // colour through an Edit grant, without the publish rung.
    assert_eq!(
        code(s.edit(
            &middle,
            &head(),
            vec![colour_meta("public.md", "cyan")],
            "e6",
            None,
            None
        )),
        "forbidden"
    );
    s.replace_policy(
        &root,
        Policy {
            grants: vec![
                grant(&root, Permission::Admin, ""),
                grant(&middle, Permission::Edit, "public.md"),
            ],
            tenants: BTreeMap::from([("t1".to_string(), ranked_section())]),
        },
        None,
    )
    .unwrap();
    let r = s
        .edit(
            &middle,
            &head(),
            vec![colour_meta("public.md", "cyan")],
            "e7",
            None,
            None,
        )
        .unwrap();
    assert_eq!(r.changes[0].meta_after, meta(None, Some("cyan")));
}

// ---- me, permissions, deciders, on-behalf-of approval ----

fn v(
    version: u32,
    operation: mnemosyne_content::api::Operation,
) -> mnemosyne_content::api::Request {
    mnemosyne_content::api::Request {
        version,
        idempotency_key: None,
        operation,
    }
}

#[test]
fn me_and_permissions_describe_the_caller_without_admin() {
    use mnemosyne_content::api::{ApiResult, Operation};
    let (_fx, s, root) = ranked_fixture();
    s.edit(
        &root,
        "r0",
        vec![
            put("public.md", "public"),
            folder("senior-only"),
            put("senior-only/pay.md", "pay"),
        ],
        "seed",
        None,
        None,
    )
    .unwrap();
    hide(&s, &root, "senior-only", "senior");
    let middle = identity("t1", "middle-m", ActorKind::Human);
    let senior = identity("t1", "senior-s", ActorKind::Human);

    let me = s.me(&middle).unwrap();
    assert_eq!(me.actor_id, "middle-m");
    assert_eq!(me.kind, ActorKind::Human);
    assert_eq!(me.role.as_deref(), Some("middle"));
    assert_eq!(me.roles, ladder());
    assert_eq!(me.rungs, TenantPolicy::default().rungs);
    assert_eq!(me.root_seen_from, "junior");

    let p = s.permissions(&middle, "public.md").unwrap();
    assert_eq!(p.path, "public.md");
    assert_eq!(p.seen_from, "junior");
    assert_eq!(p.inherited_from, None);
    assert_eq!(p.colour, "other");
    assert_eq!(p.colour_source, ColourSource::Default);
    assert!(p.can.read && p.can.propose);
    assert!(!p.can.edit && !p.can.approve && !p.can.admin);

    // A hidden path answers like a missing one and never reveals its seen_from.
    assert_eq!(
        code(s.permissions(&middle, "senior-only/pay.md")),
        "not_found"
    );
    let p = s.permissions(&senior, "senior-only/pay.md").unwrap();
    assert_eq!(p.seen_from, "senior");
    assert_eq!(p.inherited_from.as_deref(), Some("senior-only"));
    assert!(p.can.read && p.can.propose && p.can.edit && p.can.approve);
    assert!(!p.can.admin);
    let own = s.permissions(&senior, "senior-only").unwrap();
    assert_eq!(own.seen_from, "senior");
    assert_eq!(own.inherited_from, None);
    assert!(s.permissions(&root, "").unwrap().can.admin);

    // Non-members without grants are refused; a kind mismatch is reported as such.
    let stranger = identity("t1", "stranger", ActorKind::Human);
    assert_eq!(code(s.me(&stranger)), "forbidden");
    let wrong_kind = identity("t1", "senior-s", ActorKind::Agent);
    assert_eq!(code(s.me(&wrong_kind)), "policy_mismatch");
    // An agent member describes itself.
    let bot = identity("t1", "bot", ActorKind::Agent);
    assert_eq!(s.me(&bot).unwrap().kind, ActorKind::Agent);
    let pb = s.permissions(&bot, "public.md").unwrap();
    assert!(pb.can.read && !pb.can.propose && !pb.can.edit && !pb.can.approve);

    // v2 only.
    for op in [
        Operation::Me,
        Operation::Permissions {
            path: "public.md".into(),
        },
        Operation::Deciders {
            proposal_id: "p".into(),
        },
    ] {
        let r = s.execute(&middle, v(1, op));
        assert_eq!(r.error.unwrap().code, "unsupported_version");
    }
    match s.execute(&middle, v(2, Operation::Me)).result {
        Some(ApiResult::Me(me)) => assert_eq!(me.actor_id, "middle-m"),
        other => panic!("unexpected {other:?}"),
    }
    match s
        .execute(
            &middle,
            v(
                2,
                Operation::Permissions {
                    path: "public.md".into(),
                },
            ),
        )
        .result
    {
        Some(ApiResult::Permissions(p)) => assert!(p.can.propose),
        other => panic!("unexpected {other:?}"),
    }
}

fn deciders_section(with_senior_t: bool) -> TenantPolicy {
    let mut members = BTreeMap::from([
        ("middle-m".to_string(), member("middle", ActorKind::Human)),
        ("senior-s".to_string(), member("senior", ActorKind::Human)),
        ("bot".to_string(), member("senior", ActorKind::Agent)),
        ("junior-j".to_string(), member("junior", ActorKind::Human)),
    ]);
    if with_senior_t {
        members.insert("senior-t".into(), member("senior", ActorKind::Human));
    }
    TenantPolicy {
        members,
        ..TenantPolicy::default()
    }
}
fn deciders_policy(owner: &TrustedIdentity, bot: &TrustedIdentity, with_senior_t: bool) -> Policy {
    Policy {
        grants: vec![
            grant(owner, Permission::Admin, ""),
            grant(owner, Permission::Edit, ""),
            grant(owner, Permission::Read, ""),
            grant(bot, Permission::Propose, ""),
        ],
        tenants: BTreeMap::from([("t1".to_string(), deciders_section(with_senior_t))]),
    }
}

#[test]
fn deciders_lists_publishers_minus_proposer_and_on_behalf_of() {
    use mnemosyne_content::api::{ApiResult, Operation};
    let fx = Fixture::new();
    let s = fx.service();
    // owner: a non-member administrator by grant only.
    let owner = identity("t1", "owner", ActorKind::Human);
    let bot = TrustedIdentity {
        on_behalf_of: Some("senior-s".into()),
        ..identity("t1", "bot", ActorKind::Agent)
    };
    s.bootstrap_policy(deciders_policy(&owner, &bot, true))
        .unwrap();
    let head = s
        .edit(
            &owner,
            "r0",
            vec![put("public.md", "public")],
            "seed",
            None,
            None,
        )
        .unwrap()
        .id;
    let middle = identity("t1", "middle-m", ActorKind::Human);
    let senior = identity("t1", "senior-s", ActorKind::Human);
    let senior_t = identity("t1", "senior-t", ActorKind::Human);
    let names = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();

    let pm = s
        .propose(
            &middle,
            &head,
            vec![put("public.md", "m")],
            "m1",
            None,
            None,
        )
        .unwrap();
    let d = s.deciders(&middle, &pm.id).unwrap();
    assert_eq!(d.proposal_id, pm.id);
    assert_eq!(d.deciders, names(&["owner", "senior-s", "senior-t"]));
    assert!(!d.fallback);

    let ps = s
        .propose(
            &senior,
            &head,
            vec![put("public.md", "s")],
            "s1",
            None,
            None,
        )
        .unwrap();
    let d = s.deciders(&senior, &ps.id).unwrap();
    assert_eq!(d.deciders, names(&["owner", "senior-t"]));
    assert!(!d.fallback);

    // Without senior-t only the administrator-by-grant remains.
    s.replace_policy(&owner, deciders_policy(&owner, &bot, false), None)
        .unwrap();
    let d = s.deciders(&senior, &ps.id).unwrap();
    assert_eq!(d.deciders, names(&["owner"]));
    assert!(d.fallback);
    s.replace_policy(&owner, deciders_policy(&owner, &bot, true), None)
        .unwrap();

    // A bot proposing on behalf of senior-s: neither of them may decide.
    let pb = s
        .propose(&bot, &head, vec![put("public.md", "b")], "b1", None, None)
        .unwrap();
    let d = s.deciders(&senior_t, &pb.id).unwrap();
    assert_eq!(d.deciders, names(&["owner", "senior-t"]));
    assert!(!d.fallback);
    assert_eq!(
        code(s.approve(&senior, &pb.id, &pb.digest, "a1", None)),
        "self_approval_denied"
    );
    let approved = s
        .approve(&senior_t, &pb.id, &pb.digest, "a2", None)
        .unwrap();
    assert_eq!(approved.state, mnemosyne_content::ProposalState::Approved);

    // deciders follows review's visibility checks.
    let junior = identity("t1", "junior-j", ActorKind::Human);
    let stranger = identity("t1", "stranger", ActorKind::Human);
    assert_eq!(s.deciders(&junior, &pm.id).unwrap().deciders.len(), 3);
    assert_eq!(code(s.deciders(&stranger, &pm.id)), "forbidden");
    assert_eq!(code(s.deciders(&junior, "missing")), "not_found");

    match s
        .execute(
            &middle,
            v(
                2,
                Operation::Deciders {
                    proposal_id: pm.id.clone(),
                },
            ),
        )
        .result
    {
        Some(ApiResult::Deciders(d)) => assert_eq!(d.deciders.len(), 3),
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(
        s.execute(&middle, v(1, Operation::Me)).error.unwrap().code,
        "unsupported_version"
    );
}

#[test]
fn deciders_need_publish_rank_on_every_path() {
    let (_fx, s, root) = ranked_fixture();
    s.edit(
        &root,
        "r0",
        vec![
            put("public.md", "public"),
            folder("senior-only"),
            put("senior-only/pay.md", "pay"),
        ],
        "seed",
        None,
        None,
    )
    .unwrap();
    let head = hide(&s, &root, "senior-only", "administrator").id;
    // Root (administrator) proposes across a public and an administrator-only path:
    // senior-s cannot see the hidden one, so nobody at the rung remains.
    let p = s
        .propose(
            &root,
            &head,
            vec![put("public.md", "x"), put("senior-only/pay.md", "y")],
            "r1",
            None,
            None,
        )
        .unwrap();
    let d = s.deciders(&root, &p.id).unwrap();
    assert!(d.deciders.is_empty());
    assert!(d.fallback);
}

// ---- root administrators may read and approve everything ----

/// ranked_fixture plus `owner`: a human non-member holding only a root Admin grant,
/// and `scoped`: a non-member holding only an Admin grant on `public`.
fn owner_fixture() -> (Fixture, Service, TrustedIdentity, TrustedIdentity) {
    let (fx, s, root) = ranked_fixture();
    let owner = identity("t1", "owner", ActorKind::Human);
    let scoped = identity("t1", "scoped", ActorKind::Human);
    let ops = identity("t1", "agent-a", ActorKind::Agent);
    s.replace_policy(
        &root,
        Policy {
            grants: vec![
                grant(&root, Permission::Admin, ""),
                grant(&owner, Permission::Admin, ""),
                grant(&scoped, Permission::Admin, "public"),
                grant(&ops, Permission::Admin, ""),
            ],
            tenants: BTreeMap::from([("t1".to_string(), ranked_section())]),
        },
        None,
    )
    .unwrap();
    s.edit(
        &root,
        "r0",
        vec![
            put("public.md", "public"),
            folder("public"),
            put("public/a.md", "a"),
            folder("senior-only"),
            put("senior-only/pay.md", "pay"),
        ],
        "seed",
        None,
        None,
    )
    .unwrap();
    hide(&s, &root, "senior-only", "senior");
    (fx, s, root, owner)
}

#[test]
fn root_admin_grant_reads_and_approves_but_does_not_edit() {
    let (_fx, s, root, owner) = owner_fixture();
    let head = s.head(&root).unwrap();
    let senior = identity("t1", "senior-s", ActorKind::Human);
    assert_eq!(
        s.read(&owner, "senior-only/pay.md", None).unwrap().content,
        "pay"
    );
    let p = s
        .propose(
            &senior,
            &head,
            vec![put("senior-only/pay.md", "more")],
            "p1",
            None,
            None,
        )
        .unwrap();
    let approved = s.approve(&owner, &p.id, &p.digest, "o1", None).unwrap();
    assert_eq!(approved.approver.unwrap().actor_id, "owner");
    // apply re-checks the approver's Approve and Read on every path.
    s.apply(&senior, &p.id, &p.digest, "s1").unwrap();
    assert_eq!(
        s.read(&owner, "senior-only/pay.md", None).unwrap().content,
        "more"
    );
    let head = s.head(&root).unwrap();
    assert_eq!(
        code(s.edit(
            &owner,
            &head,
            vec![put("senior-only/pay.md", "o")],
            "o2",
            None,
            None
        )),
        "forbidden"
    );
    assert_eq!(
        code(s.propose(
            &owner,
            &head,
            vec![put("senior-only/pay.md", "o")],
            "o3",
            None,
            None
        )),
        "forbidden"
    );
    let can = s.permissions(&owner, "senior-only/pay.md").unwrap().can;
    assert!(can.read && can.approve && can.admin && !can.edit && !can.propose);

    // An agent member with a root Admin grant gets nothing beyond its rank from the rule.
    let ops = identity("t1", "agent-a", ActorKind::Agent);
    let can = s.permissions(&ops, "senior-only/pay.md").unwrap().can;
    assert!(can.read && can.admin && !can.approve && !can.edit);
}

#[test]
fn scoped_admin_grant_implies_nothing_elsewhere() {
    let (_fx, s, root, _owner) = owner_fixture();
    let scoped = identity("t1", "scoped", ActorKind::Human);
    assert_eq!(
        code(s.read(&scoped, "senior-only/pay.md", None)),
        "forbidden"
    );
    assert_eq!(code(s.read(&scoped, "public/a.md", None)), "forbidden");
    // The same for a member: the scoped grant adds nothing, the hidden path stays not_found.
    let junior = identity("t1", "junior-j", ActorKind::Human);
    let mut section = ranked_section();
    section
        .members
        .insert("scoped".into(), member("junior", ActorKind::Human));
    s.replace_policy(
        &root,
        Policy {
            grants: vec![
                grant(&root, Permission::Admin, ""),
                grant(&scoped, Permission::Admin, "public"),
                grant(&junior, Permission::Admin, "public"),
            ],
            tenants: BTreeMap::from([("t1".to_string(), section)]),
        },
        None,
    )
    .unwrap();
    assert_eq!(
        code(s.read(&scoped, "senior-only/pay.md", None)),
        "not_found"
    );
    assert_eq!(
        code(s.read(&junior, "senior-only/pay.md", None)),
        "not_found"
    );
}

#[test]
fn deciders_on_a_hidden_path_fall_back_to_the_root_administrator() {
    let (_fx, s, root, owner) = owner_fixture();
    let head = s.head(&root).unwrap();
    let head = s
        .edit(
            &root,
            &head,
            vec![seen_meta("senior-only", "administrator")],
            "raise",
            None,
            None,
        )
        .unwrap()
        .id;
    let p = s
        .propose(
            &root,
            &head,
            vec![put("senior-only/pay.md", "r")],
            "r1",
            None,
            None,
        )
        .unwrap();
    let d = s.deciders(&root, &p.id).unwrap();
    assert_eq!(d.deciders, vec!["owner".to_string()]);
    assert!(d.fallback);
    let approved = s.approve(&owner, &p.id, &p.digest, "o1", None).unwrap();
    assert_eq!(approved.state, mnemosyne_content::ProposalState::Approved);
}

// ---- deciders mirrors decide's checks ----

#[test]
fn deciders_list_only_who_decide_would_accept() {
    let (_fx, s, root) = ranked_fixture();
    s.edit(
        &root,
        "r0",
        vec![
            put("public.md", "public"),
            folder("senior-only"),
            put("senior-only/pay.md", "pay"),
        ],
        "seed",
        None,
        None,
    )
    .unwrap();
    let head = hide(&s, &root, "senior-only", "senior").id;
    let blind = identity("t1", "blind", ActorKind::Human);
    let reviewer = identity("t1", "reviewer", ActorKind::Human);
    let ghost = identity("t1", "ghost", ActorKind::Agent);
    s.replace_policy(
        &root,
        Policy {
            grants: vec![
                grant(&root, Permission::Admin, ""),
                // Approve without Read: decide would refuse, so not a decider.
                grant(&blind, Permission::Approve, ""),
                // Approve and Read on the paths: a decider.
                grant(&reviewer, Permission::Approve, "senior-only"),
                grant(&reviewer, Permission::Read, "senior-only"),
                // A non-member agent credential with a root Admin grant.
                grant(&ghost, Permission::Admin, ""),
            ],
            tenants: BTreeMap::from([("t1".to_string(), ranked_section())]),
        },
        None,
    )
    .unwrap();
    let senior = identity("t1", "senior-s", ActorKind::Human);
    let p = s
        .propose(
            &senior,
            &head,
            vec![put("senior-only/pay.md", "more")],
            "p1",
            None,
            None,
        )
        .unwrap();
    let d = s.deciders(&senior, &p.id).unwrap();
    assert!(!d.deciders.contains(&"blind".to_string()));
    assert!(d.deciders.contains(&"reviewer".to_string()));
    // `ghost` is listed: grants carry no kind, so a non-member is assumed human.
    assert_eq!(
        d.deciders,
        vec![
            "ghost".to_string(),
            "reviewer".to_string(),
            "root".to_string()
        ]
    );
    assert!(!d.fallback);
    assert_eq!(
        code(s.approve(&blind, &p.id, &p.digest, "b1", None)),
        "forbidden"
    );
    // The agent credential gets nothing from the root-admin rule.
    assert_eq!(
        code(s.read(&ghost, "senior-only/pay.md", None)),
        "forbidden"
    );
    assert_eq!(
        code(s.approve(&ghost, &p.id, &p.digest, "g1", None)),
        "human_required"
    );
    let approved = s.approve(&reviewer, &p.id, &p.digest, "r1", None).unwrap();
    assert_eq!(approved.approver.unwrap().actor_id, "reviewer");
}

// ---- Final review: hidden answers like absent, pinned roles, history cost ----

/// `permissions` without its `path` field, for comparing two paths' answers.
fn permissions_answer(s: &Service, who: &TrustedIdentity, path: &str) -> serde_json::Value {
    let mut v = serde_json::to_value(s.permissions(who, path).unwrap()).unwrap();
    v.as_object_mut().unwrap().remove("path");
    v
}

#[test]
fn a_rank_hidden_path_answers_what_an_absent_path_answers() {
    let (_fx, s, root) = ranked_fixture();
    let r1 = s
        .edit(
            &root,
            "r0",
            vec![
                folder("public"),
                put("public/open.md", "open"),
                put("public/secret.md", "secret"),
                folder("senior-only"),
                put("senior-only/pay.md", "pay"),
            ],
            "seed",
            None,
            None,
        )
        .unwrap();
    let hidden = hide(&s, &root, "public/secret.md", "senior");
    hide(&s, &root, "senior-only", "senior");
    let junior = identity("t1", "junior-j", ActorKind::Human);
    let senior = identity("t1", "senior-s", ActorKind::Human);

    // history: empty under a readable folder, not_found under a hidden one.
    assert!(s.history(&junior, "public/secret.md").unwrap().is_empty());
    assert!(s.history(&junior, "public/ghost.md").unwrap().is_empty());
    assert_eq!(code(s.history(&junior, "senior-only/pay.md")), "not_found");
    assert_eq!(
        code(s.history(&junior, "senior-only/ghost.md")),
        "not_found"
    );
    assert_eq!(s.history(&senior, "public/secret.md").unwrap().len(), 2);

    // permissions: the inherited answer a new file would get, never the entry's own meta.
    let secret = permissions_answer(&s, &junior, "public/secret.md");
    assert_eq!(secret, permissions_answer(&s, &junior, "public/ghost.md"));
    assert_eq!(secret["seen_from"], "junior");
    assert_eq!(secret["can"]["read"], true);
    assert_eq!(
        code(s.permissions(&junior, "senior-only/pay.md")),
        "not_found"
    );
    assert_eq!(
        code(s.permissions(&junior, "senior-only/ghost.md")),
        "not_found"
    );
    assert_eq!(
        s.permissions(&senior, "public/secret.md")
            .unwrap()
            .seen_from,
        "senior"
    );

    // diff and historical read.
    assert_eq!(code(s.diff(&junior, "rnope")), "not_found");
    assert_eq!(code(s.diff(&junior, &hidden.id)), "not_found");
    assert_eq!(
        code(s.read(&junior, "public/ghost.md", Some(&r1.id))),
        "not_found"
    );
    assert_eq!(
        code(s.read(&junior, "public/secret.md", Some(&r1.id))),
        "not_found"
    );
}

#[test]
fn ladder_keeps_every_role_an_entry_is_pinned_to() {
    let renamed = || {
        let mut t = ranked_section();
        t.roles = ["worker", "junior", "mid", "senior", "administrator"]
            .map(String::from)
            .to_vec();
        t.rungs.propose_from = "mid".into();
        t.members
            .insert("middle-m".into(), member("mid", ActorKind::Human));
        t
    };
    let (_fx, s, root) = ranked_fixture();
    s.edit(&root, "r0", vec![folder("public")], "seed", None, None)
        .unwrap();
    hide(&s, &root, "public", "middle");
    let e = s
        .set_policy(&root, policy_with(&root, "t1", renamed()))
        .unwrap_err();
    assert_eq!(e.code, "invalid_policy");
    // Reordering keeps every role.
    let mut reordered = ranked_section();
    reordered.roles = ["worker", "junior", "senior", "middle", "administrator"]
        .map(String::from)
        .to_vec();
    s.set_policy(&root, policy_with(&root, "t1", reordered.clone()))
        .unwrap();
    assert_eq!(s.policy(&root).unwrap().tenants["t1"], reordered);

    // Without a pin the same rename is accepted.
    let (_fx2, s2, root2) = ranked_fixture();
    s2.edit(&root2, "r0", vec![folder("public")], "seed", None, None)
        .unwrap();
    s2.set_policy(&root2, policy_with(&root2, "t1", renamed()))
        .unwrap();
}

#[test]
fn moving_out_of_a_hidden_folder_does_not_publish() {
    let (_fx, s, root) = ranked_fixture();
    s.edit(
        &root,
        "r0",
        vec![
            folder("secret"),
            put("secret/x.md", "x"),
            folder("public"),
            put("public/open.md", "open"),
        ],
        "seed",
        None,
        None,
    )
    .unwrap();
    let hidden = hide(&s, &root, "secret", "senior");
    s.edit(
        &root,
        &hidden.id,
        vec![rename("secret/x.md", "public/x.md")],
        "move",
        None,
        None,
    )
    .unwrap();
    let junior = identity("t1", "junior-j", ActorKind::Human);
    let senior = identity("t1", "senior-s", ActorKind::Human);
    assert_eq!(code(s.read(&junior, "public/x.md", None)), "not_found");
    assert_eq!(
        s.list(&junior, "public").unwrap(),
        vec!["public".to_string(), "public/open.md".to_string()]
    );
    let p = s.permissions(&junior, "public/x.md").unwrap();
    assert!(!p.can.read);
    let ghost = s.permissions(&junior, "public/ghost.md").unwrap();
    assert_eq!(p.seen_from, ghost.seen_from);
    assert_eq!(p.inherited_from, ghost.inherited_from);
    assert_eq!(p.colour, ghost.colour);
    assert_eq!(p.colour_source, ghost.colour_source);
    // As for `read`: the old name is still hidden.
    assert_eq!(code(s.history(&junior, "public/x.md")), "not_found");
    assert_eq!(s.read(&senior, "public/x.md", None).unwrap().content, "x");
    assert!(s.permissions(&senior, "public/x.md").unwrap().can.read);
}

#[test]
#[ignore]
fn history_cost_on_a_long_chain() {
    let (_fx, s, root) = ranked_fixture();
    let mut head = "r0".to_string();
    for i in 0..1000 {
        head = s
            .edit(
                &root,
                &head,
                vec![put(&format!("f{i}.md"), "x")],
                &format!("k{i}"),
                None,
                None,
            )
            .unwrap()
            .id;
    }
    let started = std::time::Instant::now();
    let h = s.history(&root, "f500.md").unwrap();
    let took = started.elapsed();
    println!(
        "history on a 1,000-revision chain: {took:?} ({} revisions)",
        h.len()
    );
}

// ---- Hotfix: a delete or move cannot carry entries the caller cannot read ----

/// Tenant "t1" where middles publish, so `middle-m` holds Edit on `plans/`.
fn middle_publishes_fixture() -> (Fixture, Service, TrustedIdentity) {
    let fx = Fixture::new();
    let s = fx.service();
    let root = identity("t1", "root", ActorKind::Human);
    let section = TenantPolicy {
        rungs: Rungs {
            propose_from: "junior".into(),
            publish_from: "middle".into(),
            visibility_from: "senior".into(),
        },
        ..ranked_section()
    };
    s.bootstrap_policy(policy_with(&root, "t1", section)).unwrap();
    (fx, s, root)
}

fn delete(path: &str) -> mnemosyne_content::Change {
    mnemosyne_content::Change::Delete { path: path.into() }
}

#[test]
fn delete_and_rename_refuse_a_folder_holding_a_hidden_entry() {
    let (_fx, s, root) = middle_publishes_fixture();
    s.edit(
        &root,
        "r0",
        vec![folder("plans"), put("plans/pay.md", "pay")],
        "seed",
        None,
        None,
    )
    .unwrap();
    hide(&s, &root, "plans/pay.md", "senior");
    let middle = identity("t1", "middle-m", ActorKind::Human);
    let junior = identity("t1", "junior-j", ActorKind::Human);
    let senior = identity("t1", "senior-s", ActorKind::Human);
    // The middle sees an empty folder.
    assert_eq!(s.list(&middle, "plans").unwrap(), vec!["plans".to_string()]);
    let head = s.head(&root).unwrap();

    assert_eq!(
        code(s.edit(&middle, &head, vec![delete("plans")], "m-del", None, None)),
        "forbidden"
    );
    assert_eq!(
        code(s.edit(&middle, &head, vec![rename("plans", "old-plans")], "m-mv", None, None)),
        "forbidden"
    );
    assert_eq!(
        code(s.propose(&junior, &head, vec![delete("plans")], "j-prop", None, None)),
        "forbidden"
    );
    assert_eq!(
        code(s.propose(&junior, &head, vec![rename("plans", "old")], "j-prop2", None, None)),
        "forbidden"
    );
    assert_eq!(s.head(&root).unwrap(), head);
    assert_eq!(s.read(&senior, "plans/pay.md", None).unwrap().content, "pay");
    // Hidden answers as absent everywhere else: the file itself is still not_found.
    assert_eq!(
        code(s.edit(&middle, &head, vec![delete("plans/pay.md")], "m-del2", None, None)),
        "not_found"
    );

    s.edit(&senior, &head, vec![delete("plans")], "s-del", None, None)
        .unwrap();
    assert!(s.list(&senior, "").unwrap().iter().all(|p| !p.starts_with("plans")));
}

#[test]
fn delete_refuses_a_folder_holding_a_lineage_hidden_entry() {
    let (_fx, s, root) = middle_publishes_fixture();
    s.edit(
        &root,
        "r0",
        vec![folder("vault"), folder("plans"), put("plans/open.md", "open")],
        "seed",
        None,
        None,
    )
    .unwrap();
    let hidden = hide(&s, &root, "vault", "senior");
    let r = s
        .edit(&root, &hidden.id, vec![put("vault/pay.md", "pay")], "put", None, None)
        .unwrap();
    // Moved out of the hidden folder: the old name still bounds who may read it.
    let r = s
        .edit(&root, &r.id, vec![rename("vault/pay.md", "plans/pay.md")], "mv", None, None)
        .unwrap();
    let middle = identity("t1", "middle-m", ActorKind::Human);
    assert_eq!(
        s.list(&middle, "plans").unwrap(),
        vec!["plans".to_string(), "plans/open.md".to_string()]
    );
    assert_eq!(
        code(s.edit(&middle, &r.id, vec![delete("plans")], "m-del", None, None)),
        "forbidden"
    );
    assert_eq!(
        code(s.edit(&middle, &r.id, vec![rename("plans", "p2")], "m-mv", None, None)),
        "forbidden"
    );
    assert_eq!(s.head(&root).unwrap(), r.id);
}
