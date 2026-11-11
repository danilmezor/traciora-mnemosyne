//! The agent's graph: titles, snippets, anchors, and agent visibility.
use mnemosyne_content::{
    api::{Operation, Request},
    auth::{ActorKind, Grant, Member, Permission, Policy, TenantPolicy, TrustedIdentity},
    graph::Anchor,
    links::{extract_links, Link, LinkKind},
    Change, Service,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(std::path::PathBuf);
impl Fixture {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "mnemosyne-agent-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )))
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn who(actor: &str) -> TrustedIdentity {
    TrustedIdentity {
        tenant_id: "t1".into(),
        actor_id: actor.into(),
        kind: ActorKind::Human,
        on_behalf_of: None,
    }
}

fn member(role: &str, kind: ActorKind) -> Member {
    Member {
        role: role.into(),
        kind,
    }
}

/// Tenant "t1" with a root administrator and the given further members.
fn vault_with(members: &[(&str, Member)]) -> (Fixture, Service, TrustedIdentity) {
    let fx = Fixture::new();
    let tick = std::sync::Arc::new(AtomicU64::new(0));
    let s = Service::open_with_clock(
        &fx.0,
        Box::new(move || {
            let n = tick.fetch_add(1, Ordering::Relaxed);
            format!("2026-10-29T10:{:02}:{:02}.000Z", n / 60, n % 60)
        }),
    )
    .unwrap();
    let mut all = BTreeMap::from([(
        "root".to_string(),
        member("administrator", ActorKind::Human),
    )]);
    for (id, m) in members {
        all.insert(id.to_string(), m.clone());
    }
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
                members: all,
                ..TenantPolicy::default()
            },
        )]),
    })
    .unwrap();
    (fx, s, who("root"))
}

fn vault() -> (Fixture, Service, TrustedIdentity) {
    vault_with(&[])
}

fn put(path: &str, content: &str) -> Change {
    Change::Put {
        path: path.into(),
        content: content.into(),
    }
}

fn seed(s: &Service, root: &TrustedIdentity, changes: Vec<Change>) {
    let head = s.head(root).unwrap();
    s.edit(root, &head, changes, &format!("k-{head}"), None, None)
        .unwrap();
}

fn index_node(v: &Value, path: &str) -> Value {
    v["index"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["path"] == path)
        .unwrap_or_else(|| panic!("no node {path}"))
        .clone()
}

// ---- index schema 2 ----

#[test]
fn titles_come_from_frontmatter_then_h1_then_basename() {
    let (_fx, s, root) = vault();
    seed(
        &s,
        &root,
        vec![
            put("fm.md", "---\ntitle: Warranty terms\n---\n# Other"),
            put("ru.md", "# Гарантия\n\ntext"),
            put("setext.md", "Setext\n===\n"),
            put("number.md", "---\ntitle: 5\n---\n# H"),
            put("malformed.md", "---\n: [\n---\n# H"),
            put("notes/plain.md", "just words, no heading"),
            Change::PutBinary {
                path: "photos/before.jpg".into(),
                bytes: vec![0xff, 0xd8, 0xff, 0xe0, 0, 1, 2, 3],
            },
            Change::CreateFolder {
                path: "services".into(),
            },
        ],
    );
    let g = s.graph(&root, "", 0, None).unwrap();
    let title = |p: &str| {
        g.nodes
            .iter()
            .find(|n| n.path == p)
            .unwrap_or_else(|| panic!("no node {p}"))
            .title
            .clone()
    };
    assert_eq!(title("fm.md"), "Warranty terms");
    assert_eq!(title("ru.md"), "Гарантия");
    assert_eq!(title("setext.md"), "Setext");
    assert_eq!(title("number.md"), "H");
    assert_eq!(title("malformed.md"), "H");
    assert_eq!(title("notes/plain.md"), "plain");
    assert_eq!(title("photos/before.jpg"), "before");
    assert_eq!(title("services"), "services");
    // An implicit parent folder is named by its basename too.
    assert_eq!(title("notes"), "notes");
    // At most 200 characters.
    seed(
        &s,
        &root,
        vec![put("long.md", &format!("# {}", "я".repeat(300)))],
    );
    let g = s.graph(&root, "", 0, None).unwrap();
    let long = g.nodes.iter().find(|n| n.path == "long.md").unwrap();
    assert_eq!(long.title.chars().count(), 200);
}

#[test]
fn snippet_is_first_paragraph_bounded() {
    let (_fx, s, root) = vault();
    let words = "abcd ".repeat(200);
    seed(
        &s,
        &root,
        vec![
            put(
                "para.md",
                "# T\n\nFirst **bold** [link](x.md) para.\n\nSecond",
            ),
            put("only.md", "# Only"),
            put("long.md", &format!("---\ntitle: L\n---\n{words}\n")),
            put("empty.md", ""),
            Change::CreateFolder {
                path: "folder".into(),
            },
        ],
    );
    let v = s.graph_index(&root).unwrap();
    let snippet = |p: &str| index_node(&v, p)["snippet"].as_str().unwrap().to_string();
    assert_eq!(snippet("para.md"), "First bold link para.");
    assert_eq!(snippet("only.md"), "Only");
    let long = snippet("long.md");
    assert_eq!(long.chars().count(), 240);
    assert!(long.ends_with("abcd…"), "{long}");
    assert_eq!(snippet("empty.md"), "");
    assert_eq!(snippet("folder"), "");
    assert_eq!(index_node(&v, "para.md")["title"], "T");
}

fn link(target: &str, kind: LinkKind, alias: Option<&str>, heading: Option<&str>) -> Link {
    Link {
        target: target.into(),
        kind,
        alias: alias.map(Into::into),
        heading: heading.map(Into::into),
    }
}

#[test]
fn wikilink_heading_is_kept() {
    assert_eq!(
        extract_links("[[Facts#What]] [[Facts#]] [[Facts# Why |w]] [[Plain]]"),
        vec![
            link("Facts", LinkKind::Wikilink, None, Some("What")),
            link("Facts", LinkKind::Wikilink, None, None),
            link("Facts", LinkKind::Wikilink, Some("w"), Some("Why")),
            link("Plain", LinkKind::Wikilink, None, None),
        ]
    );
    // A Markdown fragment is a slug, not a title: still stripped.
    assert_eq!(
        extract_links("[a](Facts.md#what)"),
        vec![link("Facts.md", LinkKind::Markdown, None, None)]
    );
    // A link without a heading serializes as before.
    let json = serde_json::to_value(link("Plain", LinkKind::Wikilink, None, None)).unwrap();
    assert!(json.get("heading").is_none());
}

#[test]
fn index_v2_rebuilds_old_schema() {
    let (fx, s, root) = vault();
    seed(
        &s,
        &root,
        vec![put("a.md", "# Alpha\n\n[[b]]"), put("b.md", "")],
    );
    let v = s.graph_index(&root).unwrap();
    assert_eq!(v["index"]["schema"], 2);
    let file = std::fs::canonicalize(&fx.0)
        .unwrap()
        .join("graph-v1")
        .join(format!("{:x}.json", Sha256::digest(b"t1")));
    // A schema 1 index: no titles, snippets or headings.
    let mut stored: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    stored["schema"] = 1.into();
    for n in stored["nodes"].as_array_mut().unwrap() {
        let n = n.as_object_mut().unwrap();
        n.remove("title");
        n.remove("snippet");
    }
    std::fs::write(&file, serde_json::to_vec(&stored).unwrap()).unwrap();
    let g = s.graph(&root, "", 0, None).unwrap();
    assert_eq!(serde_json::to_value(&g.index).unwrap()["status"], "rebuilt");
    let a = g.nodes.iter().find(|n| n.path == "a.md").unwrap();
    assert_eq!(a.title, "Alpha");
    let g = s.graph(&root, "", 0, None).unwrap();
    assert_eq!(serde_json::to_value(&g.index).unwrap()["status"], "current");
}

#[test]
fn index_reuses_titles_by_blob_and_renames_retitle() {
    let (_fx, s, root) = vault();
    seed(
        &s,
        &root,
        vec![put("old.md", "no heading here"), put("t.md", "# Kept")],
    );
    s.graph(&root, "", 0, None).unwrap();
    // Same blobs at new names: the content title stays, the basename title follows.
    seed(
        &s,
        &root,
        vec![
            Change::Rename {
                path: "old.md".into(),
                to: "new.md".into(),
            },
            Change::Rename {
                path: "t.md".into(),
                to: "u.md".into(),
            },
        ],
    );
    let v = s.graph_index(&root).unwrap();
    assert_eq!(v["status"], "incremental");
    assert_eq!(index_node(&v, "new.md")["title"], "new");
    assert_eq!(index_node(&v, "new.md")["snippet"], "no heading here");
    assert_eq!(index_node(&v, "u.md")["title"], "Kept");
}

#[test]
fn links_carry_titles_and_anchors() {
    let (_fx, s, root) = vault();
    seed(
        &s,
        &root,
        vec![
            put(
                "index.md",
                "# Home\n\n[[Facts#What]] [[Facts#what]] [[Facts#Nope]] [[Facts#What]] [[Facts]] [[Ghost#X]]",
            ),
            put("Facts.md", "# Facts\n## What\n"),
            put("back.md", "[home](index.md)"),
        ],
    );
    let l = s.links(&root, "index.md").unwrap();
    assert_eq!(l.out, vec!["Facts.md".to_string()]);
    let anchor = |heading: &str, id: Option<&str>| Anchor {
        target: "Facts.md".into(),
        heading: heading.into(),
        section_id: id.map(Into::into),
    };
    assert_eq!(
        l.anchors,
        vec![
            anchor("What", Some("s2")),
            anchor("what", Some("s2")),
            anchor("Nope", None),
        ]
    );
    assert_eq!(l.titles["Facts.md"], "Facts");
    assert_eq!(l.titles["index.md"], "Home");
    assert_eq!(l.titles["back.md"], "back");
    assert_eq!(l.titles.len(), 3);
    // `out` and `in` stay path lists; `section_id` is null when nothing matches.
    let v = serde_json::to_value(&l).unwrap();
    assert_eq!(v["in"], serde_json::json!(["back.md"]));
    assert_eq!(v["anchors"][2]["section_id"], Value::Null);
}

#[test]
fn anchors_and_titles_never_name_hidden_targets() {
    let (_fx, s, root) = vault_with(&[("junior-j", member("junior", ActorKind::Human))]);
    seed(
        &s,
        &root,
        vec![
            put("pub.md", "[[Secret#Plan]] [[Open#Plan]]"),
            put("Open.md", "# Plan"),
            Change::CreateFolder {
                path: "senior-only".into(),
            },
            put("senior-only/Secret.md", "# Plan"),
        ],
    );
    seed(
        &s,
        &root,
        vec![Change::SetMeta {
            path: "senior-only".into(),
            seen_from: Some("senior".into()),
            colour: None,
        }],
    );
    let l = s.links(&who("junior-j"), "pub.md").unwrap();
    assert_eq!(
        l.anchors,
        vec![Anchor {
            target: "Open.md".into(),
            heading: "Plan".into(),
            section_id: Some("s1".into()),
        }]
    );
    assert!(!serde_json::to_string(&l.titles).unwrap().contains("Secret"));
}

#[test]
fn find_hits_carry_title() {
    let (_fx, s, root) = vault();
    seed(
        &s,
        &root,
        vec![
            put(
                "services/warranty.md",
                "---\ntitle: Warranty terms\n---\nbody",
            ),
            put("services/warranty-claims.md", "no heading"),
        ],
    );
    let f = s.find(&root, "warranty", "", 20, true).unwrap();
    let titles: BTreeMap<&str, &str> = f
        .hits
        .iter()
        .map(|h| (h.path.as_str(), h.title.as_str()))
        .collect();
    assert_eq!(titles["services/warranty.md"], "Warranty terms");
    assert_eq!(titles["services/warranty-claims.md"], "warranty-claims");
    let f = s.find(&root, "services", "", 20, true).unwrap();
    assert_eq!(f.hits[0].path, "services");
    assert_eq!(f.hits[0].title, "services");
}

// ---- agent_sees_above_rank ----

fn agent(actor: &str, on_behalf_of: Option<&str>) -> TrustedIdentity {
    TrustedIdentity {
        tenant_id: "t1".into(),
        actor_id: actor.into(),
        kind: ActorKind::Agent,
        on_behalf_of: on_behalf_of.map(Into::into),
    }
}

/// `pub.md` links `[[budget]]`, which lives in `plans/`, seen from senior. Members: the
/// human `junior-j`, the senior agent `bot-s` (delegated by the junior below) and the
/// junior agent `bot-j` (alone).
fn above_rank_vault() -> (Fixture, Service, TrustedIdentity) {
    let (fx, s, root) = vault_with(&[
        ("junior-j", member("junior", ActorKind::Human)),
        ("bot-s", member("senior", ActorKind::Agent)),
        ("bot-j", member("junior", ActorKind::Agent)),
        ("senior-h", member("senior", ActorKind::Human)),
    ]);
    seed(
        &s,
        &root,
        vec![
            put("pub.md", "See [[budget]]."),
            Change::CreateFolder {
                path: "plans".into(),
            },
            put("plans/budget.md", "# Budget"),
        ],
    );
    seed(
        &s,
        &root,
        vec![Change::SetMeta {
            path: "plans".into(),
            seen_from: Some("senior".into()),
            colour: None,
        }],
    );
    (fx, s, root)
}

fn above_of(s: &Service, who: &TrustedIdentity) -> Vec<(String, String)> {
    s.links(who, "pub.md")
        .unwrap()
        .above_rank
        .into_iter()
        .map(|a| (a.target, a.seen_from))
        .collect()
}

fn budget() -> Vec<(String, String)> {
    vec![("budget".into(), "senior".into())]
}

fn set_flag(s: &Service, root: &TrustedIdentity, on: bool) -> Value {
    let state = s.policy_state(root).unwrap();
    let mut policy = serde_json::to_value(&state.policy).unwrap();
    policy["tenants"]["t1"]["agent_sees_above_rank"] = on.into();
    let request: Request = serde_json::from_value(serde_json::json!({
        "version": 2,
        "operation": {"type": "policy_set", "policy": policy, "expected_digest": state.digest}
    }))
    .unwrap();
    let r = s.execute(root, request);
    assert!(r.error.is_none(), "{:?}", r.error);
    serde_json::to_value(s.policy_state(root).unwrap()).unwrap()
}

#[test]
fn agents_never_see_above_rank_by_default() {
    let (_fx, s, _root) = above_rank_vault();
    assert_eq!(above_of(&s, &who("junior-j")), budget());
    assert_eq!(above_of(&s, &agent("bot-s", Some("junior-j"))), vec![]);
    assert_eq!(above_of(&s, &agent("bot-j", None)), vec![]);
    // The rest of the answer is unchanged for the agent.
    let l = s
        .links(&agent("bot-s", Some("junior-j")), "pub.md")
        .unwrap();
    assert!(l.out.is_empty() && l.r#in.is_empty());
    assert!(!serde_json::to_string(&l).unwrap().contains("budget"));
}

#[test]
fn flag_true_lets_delegated_agents_see_it() {
    let (_fx, s, root) = above_rank_vault();
    let v = set_flag(&s, &root, true);
    assert_eq!(v["policy"]["tenants"]["t1"]["agent_sees_above_rank"], true);
    let human = above_of(&s, &who("junior-j"));
    assert_eq!(human, budget());
    // Computed with the delegated (intersected, junior) visibility, not the agent's own.
    assert_eq!(above_of(&s, &agent("bot-s", Some("junior-j"))), human);
    assert_eq!(above_of(&s, &agent("bot-j", None)), vec![]);
    // A senior agent alone reads the target: nothing is above its rank.
    assert_eq!(above_of(&s, &agent("bot-s", None)), vec![]);
    let l = s.links(&agent("bot-s", None), "pub.md").unwrap();
    assert_eq!(l.out, vec!["plans/budget.md".to_string()]);
}

#[test]
fn flag_false_keeps_existing_policy_digest() {
    // Omitted while false: an older policy serializes, and so digests, as before.
    let json = serde_json::to_value(TenantPolicy::default()).unwrap();
    assert!(json.get("agent_sees_above_rank").is_none(), "{json}");
    let m14: TenantPolicy = serde_json::from_value(json.clone()).unwrap();
    assert!(!m14.agent_sees_above_rank);

    let (_fx, s, root) = above_rank_vault();
    let before = s.policy_state(&root).unwrap().digest;
    let on = set_flag(&s, &root, true);
    assert_ne!(on["digest"], before.as_str());
    let off = set_flag(&s, &root, false);
    assert_eq!(off["digest"], before.as_str());
    assert!(off["policy"]["tenants"]["t1"]
        .get("agent_sees_above_rank")
        .is_none());

    // policy_set is v2 only, so v1 never carries the flag.
    let state = s.policy_state(&root).unwrap();
    let r = s.execute(
        &root,
        Request {
            version: 1,
            idempotency_key: None,
            operation: Operation::PolicySet {
                policy: state.policy,
                expected_digest: state.digest,
            },
        },
    );
    assert_eq!(r.error.unwrap().code, "unsupported_version");
}

#[test]
fn delegated_agents_get_exactly_their_humans_list() {
    let (_fx, s, root) = above_rank_vault();
    set_flag(&s, &root, true);
    // A junior agent acting for a senior: the senior reads `budget`, so nothing is above
    // rank for the pair, although the agent's own rank cannot read it.
    assert_eq!(above_of(&s, &who("senior-h")), vec![]);
    let l = s
        .links(&agent("bot-j", Some("senior-h")), "pub.md")
        .unwrap();
    assert_eq!(l.above_rank, vec![]);
    assert!(l.out.is_empty());
    assert!(!serde_json::to_string(&l).unwrap().contains("budget"));
    // A senior agent acting for a junior: the junior's list.
    assert_eq!(
        above_of(&s, &agent("bot-s", Some("junior-j"))),
        above_of(&s, &who("junior-j"))
    );
    assert_eq!(above_of(&s, &agent("bot-s", Some("junior-j"))), budget());
    // An agent alone still gets none.
    assert_eq!(above_of(&s, &agent("bot-j", None)), vec![]);
}

#[test]
fn find_falls_back_to_basename_titles_when_the_index_cannot_reconcile() {
    let (fx, s, root) = vault();
    seed(&s, &root, vec![put("warranty.md", "# Warranty terms")]);
    let f = s.find(&root, "warranty", "", 20, true).unwrap();
    assert_eq!(f.hits[0].title, "Warranty terms");
    // A stale index whose changed text cannot be read: find still answers, with the
    // basename title from the manifest.
    seed(&s, &root, vec![put("warranty.md", "# New warranty terms")]);
    let blob = std::fs::canonicalize(&fx.0)
        .unwrap()
        .join("blobs")
        .join(format!("{:x}", Sha256::digest(b"# New warranty terms")));
    std::fs::remove_file(&blob).unwrap();
    let f = s.find(&root, "warranty", "", 20, true).unwrap();
    assert_eq!(f.hits.len(), 1);
    assert_eq!(f.hits[0].title, "warranty");
    // `graph` itself still says the index is unavailable.
    assert_eq!(
        s.graph(&root, "", 0, None).unwrap_err().code,
        "graph_unavailable"
    );
}

// ---- neighbourhood and graph paging ----

fn execute(s: &Service, who: &TrustedIdentity, version: u32, operation: Value) -> Value {
    let request: Request = serde_json::from_value(serde_json::json!({
        "version": version,
        "operation": operation,
    }))
    .unwrap();
    serde_json::to_value(s.execute(who, request)).unwrap()
}

fn error_code(v: &Value) -> String {
    v["error"]["code"].as_str().unwrap_or("").to_string()
}

fn hops(n: &mnemosyne_content::graph::NeighbourhoodResult) -> Vec<(String, u8)> {
    n.nodes.iter().map(|n| (n.path.clone(), n.hop)).collect()
}

fn hop(path: &str, hop: u8) -> (String, u8) {
    (path.to_string(), hop)
}

fn edges_of(edges: &[mnemosyne_content::graph::GraphEdge]) -> Vec<(String, String, String)> {
    edges
        .iter()
        .map(|e| {
            (
                e.from.clone(),
                e.to.clone(),
                serde_json::to_value(e.kind)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_string(),
            )
        })
        .collect()
}

fn edge(from: &str, to: &str, kind: &str) -> (String, String, String) {
    (from.into(), to.into(), kind.into())
}

#[test]
fn neighbourhood_walks_links_both_ways_with_titles_and_snippets() {
    let (_fx, s, root) = vault();
    seed(
        &s,
        &root,
        vec![
            put("hub.md", "# Hub\n\nThe centre.\n\n[[a]]"),
            put("a.md", "# Alpha\n\nAlpha text."),
            put("b.md", "Beta first.\n\n[[hub]]"),
            put("far.md", "[[b]]"),
        ],
    );
    let n = s.neighbourhood(&root, "hub.md", 1, 32).unwrap();
    assert_eq!(n.centre, "hub.md");
    assert_eq!(n.revision_id, s.head(&root).unwrap());
    assert_eq!(
        hops(&n),
        vec![hop("hub.md", 0), hop("a.md", 1), hop("b.md", 1)]
    );
    let node = |p: &str| n.nodes.iter().find(|x| x.path == p).unwrap();
    assert_eq!(node("hub.md").title, "Hub");
    assert_eq!(node("hub.md").snippet, "The centre.");
    assert_eq!(node("a.md").title, "Alpha");
    assert_eq!(node("a.md").snippet, "Alpha text.");
    assert_eq!(node("b.md").title, "b");
    assert_eq!(node("b.md").snippet, "Beta first.");
    assert!(!node("a.md").file_id.is_empty());
    assert_eq!(node("a.md").seen_from, "junior");
    assert_eq!(
        edges_of(&n.edges),
        vec![
            edge("b.md", "hub.md", "links"),
            edge("hub.md", "a.md", "links")
        ]
    );
    assert!(!n.truncated);
    assert!(n.unresolved.is_empty() && n.above_rank.is_empty());
    // The neighbourhood JSON shape.
    let v = serde_json::to_value(&n).unwrap();
    for key in [
        "revision_id",
        "index",
        "centre",
        "nodes",
        "edges",
        "unresolved",
        "above_rank",
        "truncated",
    ] {
        assert!(v.get(key).is_some(), "{key}");
    }
    assert_eq!(v["nodes"][0]["hop"], 0);
    assert_eq!(v["nodes"][1]["colour_source"], "default");
    // The centre's unresolved links, as `links` gives them.
    seed(&s, &root, vec![put("hub.md", "# Hub\n\n[[a]] [[Ghost]]")]);
    let n = s.neighbourhood(&root, "hub.md", 1, 32).unwrap();
    assert_eq!(n.unresolved.len(), 1);
    assert_eq!(n.unresolved[0].target, "Ghost");
}

#[test]
fn neighbourhood_depth_two_and_limit() {
    let (_fx, s, root) = vault();
    seed(
        &s,
        &root,
        vec![
            put("c1.md", "[[c2]]"),
            put("c2.md", "[[c3]]"),
            put("c3.md", "[[c4]]"),
            put("c4.md", ""),
        ],
    );
    let n = s.neighbourhood(&root, "c1.md", 2, 32).unwrap();
    assert_eq!(
        hops(&n),
        vec![hop("c1.md", 0), hop("c2.md", 1), hop("c3.md", 2)]
    );
    assert!(!n.truncated);
    assert_eq!(
        edges_of(&n.edges),
        vec![
            edge("c1.md", "c2.md", "links"),
            edge("c2.md", "c3.md", "links")
        ]
    );
    // Depth 1 from the middle: both directions.
    let n = s.neighbourhood(&root, "c2.md", 1, 32).unwrap();
    assert_eq!(
        hops(&n),
        vec![hop("c2.md", 0), hop("c3.md", 1), hop("c1.md", 1)]
    );
    // The cut at `limit`: in_links descending, then path.
    let n = s.neighbourhood(&root, "c2.md", 1, 1).unwrap();
    assert_eq!(hops(&n), vec![hop("c2.md", 0), hop("c3.md", 1)]);
    assert!(n.truncated);
    let n = s.neighbourhood(&root, "c1.md", 2, 1).unwrap();
    assert_eq!(hops(&n), vec![hop("c1.md", 0), hop("c2.md", 1)]);
    assert!(n.truncated);
    // Exactly at the limit is not truncated.
    let n = s.neighbourhood(&root, "c1.md", 2, 2).unwrap();
    assert_eq!(n.nodes.len(), 3);
    assert!(!n.truncated);
}

#[test]
fn neighbourhood_of_folder_starts_with_members() {
    let (_fx, s, root) = vault();
    seed(
        &s,
        &root,
        vec![
            put("f/x.md", "[[out]]"),
            put("f/y.md", ""),
            put("f/sub/z.md", ""),
            put("out.md", "[[far]]"),
            put("far.md", ""),
            put("in.md", "[[y]]"),
        ],
    );
    let n = s.neighbourhood(&root, "f", 1, 32).unwrap();
    assert_eq!(
        hops(&n),
        vec![
            hop("f", 0),
            hop("f/y.md", 1),
            hop("f/sub", 1),
            hop("f/x.md", 1)
        ]
    );
    assert_eq!(n.nodes[0].kind, mnemosyne_content::EntryKind::Folder);
    assert_eq!(n.nodes[0].snippet, "");
    assert_eq!(
        edges_of(&n.edges),
        vec![
            edge("f", "f/sub", "contains"),
            edge("f", "f/x.md", "contains"),
            edge("f", "f/y.md", "contains"),
        ]
    );
    // Hop 2: the links of the members, not the members of members.
    let n = s.neighbourhood(&root, "f", 2, 32).unwrap();
    assert_eq!(
        hops(&n),
        vec![
            hop("f", 0),
            hop("f/y.md", 1),
            hop("f/sub", 1),
            hop("f/x.md", 1),
            hop("out.md", 2),
            hop("in.md", 2),
        ]
    );
    assert!(edges_of(&n.edges).contains(&edge("f/x.md", "out.md", "links")));
    assert!(edges_of(&n.edges).contains(&edge("in.md", "f/y.md", "links")));
    assert!(!n
        .nodes
        .iter()
        .any(|x| x.path == "f/sub/z.md" || x.path == "far.md"));
}

/// Review Focus 1: `a.md → secret.md → b.md` with `secret.md` senior-only.
fn hidden_walk_vault() -> (Fixture, Service, TrustedIdentity) {
    let (fx, s, root) = vault_with(&[
        ("junior-j", member("junior", ActorKind::Human)),
        ("bot-j", member("junior", ActorKind::Agent)),
        ("bot-s", member("senior", ActorKind::Agent)),
    ]);
    seed(
        &s,
        &root,
        vec![
            put("a.md", "# A\n\nIntro.\n\n[[secret]]"),
            put("b.md", "# B\n\nBee."),
            Change::CreateFolder {
                path: "senior-only".into(),
            },
            put("senior-only/secret.md", "# Secret\n\nHush. [[b]]"),
        ],
    );
    seed(
        &s,
        &root,
        vec![Change::SetMeta {
            path: "senior-only".into(),
            seen_from: Some("senior".into()),
            colour: None,
        }],
    );
    (fx, s, root)
}

#[test]
fn neighbourhood_never_walks_through_hidden() {
    let (_fx, s, root) = hidden_walk_vault();
    // A root administrator walks a → secret → b.
    let n = s.neighbourhood(&root, "a.md", 2, 32).unwrap();
    assert_eq!(
        hops(&n),
        vec![
            hop("a.md", 0),
            hop("senior-only/secret.md", 1),
            hop("b.md", 2)
        ]
    );
    // The junior human: b.md is not reached; secret only as above_rank.
    let junior = who("junior-j");
    let n = s.neighbourhood(&junior, "a.md", 2, 32).unwrap();
    assert_eq!(hops(&n), vec![hop("a.md", 0)]);
    assert!(n.edges.is_empty());
    let above: Vec<(String, String)> = n
        .above_rank
        .iter()
        .map(|a| (a.target.clone(), a.seen_from.clone()))
        .collect();
    assert_eq!(above, vec![("secret".to_string(), "senior".to_string())]);
    assert_eq!(
        serde_json::to_value(&n.above_rank).unwrap(),
        serde_json::to_value(s.links(&junior, "a.md").unwrap().above_rank).unwrap()
    );
    // b.md's only neighbour is hidden.
    let n = s.neighbourhood(&junior, "b.md", 2, 32).unwrap();
    assert_eq!(hops(&n), vec![hop("b.md", 0)]);
    assert!(n.above_rank.is_empty());
    // Agents: nothing about the secret at all, alone or on behalf of the junior.
    for bot in [
        agent("bot-j", None),
        agent("bot-s", Some("junior-j")),
        agent("bot-j", Some("junior-j")),
    ] {
        for (centre, depth) in [("a.md", 2), ("b.md", 2), ("a.md", 1)] {
            let n = s.neighbourhood(&bot, centre, depth, 32).unwrap();
            assert_eq!(n.nodes.len(), 1, "{bot:?} {centre}");
            let json = serde_json::to_string(&n).unwrap();
            assert!(!json.contains("ecret") && !json.contains("Hush"), "{json}");
        }
        assert_eq!(
            s.neighbourhood(&bot, "senior-only/secret.md", 1, 32)
                .unwrap_err()
                .code,
            "not_found"
        );
    }
    // A senior agent alone reads it all.
    let n = s
        .neighbourhood(&agent("bot-s", None), "a.md", 2, 32)
        .unwrap();
    assert_eq!(n.nodes.len(), 3);
}

#[test]
fn neighbourhood_bounds() {
    let (_fx, s, root) = hidden_walk_vault();
    let junior = who("junior-j");
    let op = |extra: Value| {
        let mut op = serde_json::json!({"type": "neighbourhood", "path": "a.md"});
        for (k, v) in extra.as_object().unwrap() {
            op[k] = v.clone();
        }
        op
    };
    for extra in [
        serde_json::json!({"depth": 0}),
        serde_json::json!({"depth": 3}),
        serde_json::json!({"limit": 0}),
        serde_json::json!({"limit": 65}),
    ] {
        let v = execute(&s, &junior, 2, op(extra.clone()));
        assert_eq!(error_code(&v), "invalid_request", "{extra}");
    }
    // Defaults: depth 1, limit 32.
    let v = execute(&s, &root, 2, op(serde_json::json!({})));
    assert!(v["error"].is_null(), "{v}");
    assert_eq!(v["result"]["centre"], "a.md");
    assert_eq!(v["result"]["nodes"].as_array().unwrap().len(), 2);
    let v = execute(
        &s,
        &root,
        2,
        op(serde_json::json!({"depth": 2, "limit": 64})),
    );
    assert_eq!(v["result"]["nodes"].as_array().unwrap().len(), 3);
    // Hidden and absent alike.
    let hidden = s
        .neighbourhood(&junior, "senior-only/secret.md", 1, 32)
        .unwrap_err();
    let absent = s.neighbourhood(&junior, "nope.md", 1, 32).unwrap_err();
    assert_eq!(hidden.code, "not_found");
    assert_eq!(
        serde_json::to_value(&hidden).unwrap(),
        serde_json::to_value(&absent).unwrap()
    );
    assert_eq!(
        s.neighbourhood(&junior, "senior-only", 1, 32)
            .unwrap_err()
            .code,
        "not_found"
    );
    assert_eq!(
        s.neighbourhood(&junior, "../x", 1, 32).unwrap_err().code,
        "invalid_path"
    );
    let v = execute(&s, &junior, 1, op(serde_json::json!({})));
    assert_eq!(error_code(&v), "unsupported_version");
}

fn paging_vault() -> (Fixture, Service, TrustedIdentity) {
    let (fx, s, root) = vault();
    seed(
        &s,
        &root,
        vec![
            put("a.md", "[[c]] [[Ghost]]"),
            put("b.md", "[[a]] [[e]]"),
            put("c.md", "[[Nope]]"),
            put("d/e.md", "[[a]] [[Missing]]"),
        ],
    );
    (fx, s, root)
}

#[test]
fn graph_pages_union_equals_whole() {
    let (_fx, s, root) = paging_vault();
    let whole = s.graph(&root, "", 0, None).unwrap();
    assert_eq!(whole.nodes.len(), 5);
    assert_eq!(whole.next_offset, None);
    let (mut nodes, mut edges, mut unresolved) = (vec![], vec![], vec![]);
    let mut offsets = vec![];
    let mut offset = 0;
    loop {
        let page = s.graph(&root, "", offset, Some(2)).unwrap();
        assert_eq!(page.revision_id, whole.revision_id);
        assert!(page.nodes.len() <= 2);
        // Every edge and unresolved link of a page starts on that page.
        let on_page: Vec<&str> = page.nodes.iter().map(|n| n.path.as_str()).collect();
        assert!(page
            .edges
            .iter()
            .all(|e| on_page.contains(&e.from.as_str())));
        assert!(page
            .unresolved
            .iter()
            .all(|u| on_page.contains(&u.from.as_str())));
        nodes.extend(page.nodes);
        edges.extend(page.edges);
        unresolved.extend(page.unresolved);
        offsets.push(page.next_offset);
        match page.next_offset {
            Some(next) => offset = next,
            None => break,
        }
    }
    assert_eq!(offsets, vec![Some(2), Some(4), None]);
    // In path byte order, each exactly once.
    assert_eq!(
        nodes.iter().map(|n| n.path.as_str()).collect::<Vec<_>>(),
        vec!["a.md", "b.md", "c.md", "d", "d/e.md"]
    );
    assert_eq!(
        serde_json::to_value(&nodes).unwrap(),
        serde_json::to_value(&whole.nodes).unwrap()
    );
    let sorted = |mut v: Vec<(String, String, String)>| {
        v.sort();
        v
    };
    assert_eq!(sorted(edges_of(&edges)), sorted(edges_of(&whole.edges)));
    assert_eq!(
        serde_json::to_value(&unresolved).unwrap(),
        serde_json::to_value(&whole.unresolved).unwrap()
    );
    assert_eq!(unresolved.len(), 3);
    // Past the end: an empty last page.
    let page = s.graph(&root, "", 99, Some(2)).unwrap();
    assert!(page.nodes.is_empty() && page.edges.is_empty() && page.unresolved.is_empty());
    assert_eq!(page.next_offset, None);
    // A page that ends exactly at the end is the last.
    assert_eq!(s.graph(&root, "", 3, Some(2)).unwrap().next_offset, None);
    for limit in [0, 2001] {
        let v = execute(
            &s,
            &root,
            2,
            serde_json::json!({"type": "graph", "limit": limit}),
        );
        assert_eq!(error_code(&v), "invalid_request", "{limit}");
    }
    let v = execute(
        &s,
        &root,
        2,
        serde_json::json!({"type": "graph", "offset": 1, "limit": 2000}),
    );
    assert_eq!(v["result"]["nodes"].as_array().unwrap().len(), 4);
    assert_eq!(v["result"]["next_offset"], Value::Null);
}

#[test]
fn graph_unpaged_keeps_the_original_shape() {
    let (_fx, s, root) = hidden_walk_vault();
    let junior = who("junior-j");
    let g = s.graph(&junior, "", 0, None).unwrap();
    assert_eq!(g.next_offset, None);
    assert_eq!(
        g.nodes.iter().map(|n| n.path.as_str()).collect::<Vec<_>>(),
        vec!["a.md", "b.md"]
    );
    // The JSON answer without `limit` carries `next_offset: null`; a page of everything
    // equals it.
    let v = execute(&s, &junior, 2, serde_json::json!({"type": "graph"}));
    assert_eq!(v["result"]["next_offset"], Value::Null);
    let paged = s.graph(&junior, "", 0, Some(2000)).unwrap();
    assert_eq!(
        serde_json::to_value((&paged.nodes, &paged.edges, &paged.unresolved)).unwrap(),
        serde_json::to_value((&g.nodes, &g.edges, &g.unresolved)).unwrap()
    );
    assert_eq!(paged.next_offset, None);
    // A scope pages within itself.
    let g = s.graph(&root, "senior-only", 0, Some(1)).unwrap();
    assert_eq!(g.nodes[0].path, "senior-only");
    assert_eq!(g.next_offset, Some(1));
    let v = execute(
        &s,
        &junior,
        1,
        serde_json::json!({"type": "graph", "limit": 1}),
    );
    assert_eq!(error_code(&v), "unsupported_version");
}

// ---- read_many, sections by title, sections on search hits ----

use mnemosyne_content::extraction::{
    ExtractionRecord, ExtractionSegment, ExtractionStatus, SourceLocation, VERSION,
};

fn location(section: Option<&str>, slide: Option<u32>) -> SourceLocation {
    SourceLocation {
        page: None,
        paragraph: None,
        section: section.map(Into::into),
        slide,
    }
}

/// Stores an extraction record for `bytes` in the derived cache, as the worker would.
fn cache_extraction(
    fx: &Fixture,
    bytes: &[u8],
    format: &str,
    status: ExtractionStatus,
    segments: Vec<ExtractionSegment>,
) {
    let blob = format!("{:x}", Sha256::digest(bytes));
    let record = ExtractionRecord {
        source_blob: blob.clone(),
        extractor: format!("mnemosyne-local-{format}"),
        version: VERSION.into(),
        status,
        segments,
        limits: vec![],
    };
    let dir = fx.0.join("extraction-v1");
    std::fs::create_dir_all(&dir).unwrap();
    let c = rusqlite::Connection::open(dir.join("cache.sqlite3")).unwrap();
    c.execute_batch("CREATE TABLE IF NOT EXISTS extraction(blob TEXT NOT NULL, format TEXT NOT NULL, version TEXT NOT NULL, data TEXT NOT NULL, checksum TEXT NOT NULL, PRIMARY KEY(blob,format,version));").unwrap();
    let data = serde_json::to_string(&record).unwrap();
    c.execute(
        "INSERT OR REPLACE INTO extraction(blob,format,version,data,checksum) VALUES (?1,?2,?3,?4,?5)",
        rusqlite::params![blob, format, VERSION, data, format!("{:x}", Sha256::digest(data.as_bytes()))],
    )
    .unwrap();
}

fn segment(text: &str, location: SourceLocation) -> ExtractionSegment {
    ExtractionSegment {
        text: text.into(),
        location,
    }
}

fn binary(path: &str, bytes: &[u8]) -> Change {
    Change::PutBinary {
        path: path.into(),
        bytes: bytes.to_vec(),
    }
}

fn paths(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn item_code(item: &mnemosyne_content::read_many::ReadItem) -> &str {
    item.error.as_ref().map_or("", |e| e.code.as_str())
}

#[test]
fn read_many_reads_text_and_extracted_text_per_item() {
    let (fx, s, root) = vault();
    seed(
        &s,
        &root,
        vec![
            put("note.md", "# Note\n\nhello"),
            binary("docs/spec.docx", b"docx bytes"),
            binary("docs/broken.docx", b"broken docx bytes"),
            binary("docs/deck.pptx", b"pptx bytes"),
            binary("pics/a.png", b"png bytes"),
            binary("docs/fresh.pdf", b"pdf bytes"),
            binary("archive.zip", b"zip bytes"),
            Change::CreateFolder {
                path: "services".into(),
            },
        ],
    );
    cache_extraction(
        &fx,
        b"docx bytes",
        "docx",
        ExtractionStatus::Succeeded,
        vec![
            segment("First paragraph.", location(None, None)),
            segment("Second paragraph.", location(None, None)),
        ],
    );
    cache_extraction(
        &fx,
        b"broken docx bytes",
        "docx",
        ExtractionStatus::Failed,
        vec![],
    );
    cache_extraction(
        &fx,
        b"pptx bytes",
        "pptx",
        ExtractionStatus::Partial,
        vec![segment("Slide one", location(Some("One"), Some(1)))],
    );
    let r = s
        .read_many(
            &root,
            &paths(&[
                "note.md",
                "docs/spec.docx",
                "docs/deck.pptx",
                "pics/a.png",
                "docs/fresh.pdf",
                "docs/broken.docx",
                "archive.zip",
                "services",
                "docs",
                "../x",
                "absent.md",
            ]),
            262_144,
        )
        .unwrap();
    assert_eq!(r.revision_id, s.head(&root).unwrap());
    let got: Vec<(&str, bool, Option<&str>, Option<&str>, &str)> = r
        .items
        .iter()
        .map(|i| {
            (
                i.path.as_str(),
                i.ok,
                i.kind.as_deref(),
                i.content.as_deref(),
                item_code(i),
            )
        })
        .collect();
    assert_eq!(
        got,
        vec![
            ("note.md", true, Some("text"), Some("# Note\n\nhello"), ""),
            (
                "docs/spec.docx",
                true,
                Some("extracted"),
                Some("First paragraph.\n\nSecond paragraph."),
                ""
            ),
            (
                "docs/deck.pptx",
                true,
                Some("extracted"),
                Some("Slide one"),
                ""
            ),
            ("pics/a.png", false, None, None, "not_text"),
            ("docs/fresh.pdf", false, None, None, "extraction_pending"),
            ("docs/broken.docx", false, None, None, "not_text"),
            ("archive.zip", false, None, None, "not_text"),
            ("services", false, None, None, "not_text"),
            ("docs", false, None, None, "not_text"),
            ("../x", false, None, None, "invalid_path"),
            ("absent.md", false, None, None, "not_found"),
        ]
    );
    assert_eq!(
        r.used_bytes,
        "# Note\n\nhello".len() + "First paragraph.\n\nSecond paragraph.".len() + "Slide one".len()
    );
    assert!(r.items.iter().all(|i| !i.truncated));
    // The JSON item shape: absent fields are omitted.
    let v = serde_json::to_value(&r).unwrap();
    assert_eq!(
        v["items"][0],
        serde_json::json!({"path": "note.md", "ok": true, "kind": "text", "content": "# Note\n\nhello"})
    );
    assert_eq!(v["items"][4]["error"]["code"], "extraction_pending");
    assert!(v["items"][4].get("content").is_none());
    // read_many never runs the extractor.
    assert_eq!(
        s.extraction_status(&root, "docs/fresh.pdf")
            .unwrap()
            .extraction
            .status,
        ExtractionStatus::Pending
    );
}

#[test]
fn read_many_hidden_equals_absent_and_budget_truncates() {
    let (_fx, s, root) = vault_with(&[
        ("junior-j", member("junior", ActorKind::Human)),
        ("bot-j", member("junior", ActorKind::Agent)),
    ]);
    let big = "Гарантия ".repeat(300 * 1024 / "Гарантия ".len() + 1);
    assert!(big.len() >= 300 * 1024);
    seed(
        &s,
        &root,
        vec![
            put("big.md", &big),
            put("next.md", "after"),
            Change::CreateFolder {
                path: "senior-only".into(),
            },
            put("senior-only/secret.md", "hush"),
            put("senior-only/deeper/x.md", "hush"),
        ],
    );
    seed(
        &s,
        &root,
        vec![Change::SetMeta {
            path: "senior-only".into(),
            seen_from: Some("senior".into()),
            colour: None,
        }],
    );
    for reader in [who("junior-j"), agent("bot-j", None)] {
        let r = s
            .read_many(
                &reader,
                &paths(&[
                    "big.md",
                    "next.md",
                    "senior-only/secret.md",
                    "absent.md",
                    "senior-only/deeper",
                    "senior-only",
                ]),
                262_144,
            )
            .unwrap();
        let first = &r.items[0];
        assert!(first.ok && first.truncated);
        let content = first.content.as_deref().unwrap();
        assert!(content.len() <= 262_144 && content.len() > 262_144 - 4);
        assert!(big.starts_with(content));
        assert_eq!(r.used_bytes, content.len());
        assert!(!r.items[1].ok);
        assert_eq!(item_code(&r.items[1]), "max_bytes");
        assert!(r.items[1].content.is_none());
        // Hidden and absent are identical but for the path, budget or not.
        let mut hidden = serde_json::to_value(&r.items[2]).unwrap();
        let absent = serde_json::to_value(&r.items[3]).unwrap();
        assert_eq!(absent["error"]["code"], "not_found");
        hidden["path"] = absent["path"].clone();
        assert_eq!(hidden, absent);
        for i in [4, 5] {
            let mut folder = serde_json::to_value(&r.items[i]).unwrap();
            folder["path"] = absent["path"].clone();
            assert_eq!(folder, absent, "{}", r.items[i].path);
        }
        let json = serde_json::to_string(&r).unwrap();
        assert!(!json.contains("hush"));
    }
    // A small budget: the first item whole, the second cut, the rest spent.
    seed(
        &s,
        &root,
        vec![put("abc.md", "abc"), put("defg.md", "defg")],
    );
    let r = s
        .read_many(
            &root,
            &paths(&["abc.md", "defg.md", "next.md", "absent.md"]),
            5,
        )
        .unwrap();
    assert_eq!(r.items[0].content.as_deref(), Some("abc"));
    assert!(!r.items[0].truncated);
    assert_eq!(r.items[1].content.as_deref(), Some("de"));
    assert!(r.items[1].truncated);
    assert_eq!(item_code(&r.items[2]), "max_bytes");
    // An item that would fail anyway keeps its own code.
    assert_eq!(item_code(&r.items[3]), "not_found");
    assert_eq!(r.used_bytes, 5);
    // A cut never splits a character: one byte of a two-byte letter is no content.
    seed(&s, &root, vec![put("ru.md", "Гарантия")]);
    let r = s.read_many(&root, &paths(&["ru.md"]), 3).unwrap();
    assert_eq!(r.items[0].content.as_deref(), Some("Г"));
    assert!(r.items[0].truncated);
    assert_eq!(r.used_bytes, 2);
}

#[test]
fn read_many_bounds() {
    let (_fx, s, root) = vault();
    seed(&s, &root, vec![put("a.md", "a")]);
    let many: Vec<String> = (0..65).map(|i| format!("f{i}.md")).collect();
    for (paths, max_bytes) in [
        (serde_json::json!([]), serde_json::json!(262_144)),
        (serde_json::json!(many), serde_json::json!(262_144)),
        (
            serde_json::json!(["a.md", "a.md"]),
            serde_json::json!(262_144),
        ),
        (serde_json::json!(["a.md"]), serde_json::json!(0)),
        (serde_json::json!(["a.md"]), serde_json::json!(8_388_609)),
    ] {
        let v = execute(
            &s,
            &root,
            2,
            serde_json::json!({"type": "read_many", "paths": paths, "max_bytes": max_bytes}),
        );
        assert_eq!(error_code(&v), "invalid_request", "{paths} {max_bytes}");
    }
    let v = execute(
        &s,
        &root,
        2,
        serde_json::json!({"type": "read_many", "paths": many[..64], "max_bytes": 8_388_608}),
    );
    assert!(v["error"].is_null(), "{v}");
    assert_eq!(v["result"]["items"].as_array().unwrap().len(), 64);
    // max_bytes defaults to a quarter MiB.
    let v = execute(
        &s,
        &root,
        2,
        serde_json::json!({"type": "read_many", "paths": ["a.md"]}),
    );
    assert_eq!(v["result"]["items"][0]["content"], "a");
    assert_eq!(v["result"]["used_bytes"], 1);
    let v = execute(
        &s,
        &root,
        1,
        serde_json::json!({"type": "read_many", "paths": ["a.md"]}),
    );
    assert_eq!(error_code(&v), "unsupported_version");
}

#[test]
fn read_section_by_title_or_id() {
    let (_fx, s, root) = vault();
    let text = "intro\n\n# Title\n\n## What\n\nwhat body\n\n## Notes\n\nfirst notes\n\n## Notes\n\nsecond notes\n";
    seed(&s, &root, vec![put("doc.md", text)]);
    let by =
        |id: Option<&str>, title: Option<&str>| s.read_section(&root, "doc.md", None, id, title);
    let s2 = by(Some("s2"), None).unwrap();
    assert_eq!(s2.content, "## What\n\nwhat body\n\n");
    assert_eq!(s2.section_id, "s2");
    let what = by(None, Some("What")).unwrap();
    assert_eq!(what.content, s2.content);
    assert_eq!(what.section_id, "s2");
    assert_eq!(by(None, Some("s2")).unwrap().content, s2.content);
    // A shared title selects the first in document order.
    let notes = by(None, Some("Notes")).unwrap();
    assert_eq!(notes.section_id, "s3");
    assert_eq!(notes.content, "## Notes\n\nfirst notes\n\n");
    assert_eq!(by(None, Some("s0")).unwrap().content, "intro\n\n");
    assert_eq!(by(Some("s0"), None).unwrap().content, "intro\n\n");
    let long = "x".repeat(1025);
    for (id, title) in [
        (None, None),
        (Some("s2"), Some("What")),
        (None, Some("")),
        (Some(""), None),
        (None, Some(long.as_str())),
    ] {
        assert_eq!(
            by(id, title).unwrap_err().code,
            "invalid_section",
            "{id:?} {title:?}"
        );
    }
    // Exactly 1,024 bytes is a selector, which matches nothing.
    assert_eq!(
        by(None, Some(&"x".repeat(1024))).unwrap_err().code,
        "section_not_found"
    );
    assert_eq!(
        by(None, Some("Nope")).unwrap_err().code,
        "section_not_found"
    );
    assert_eq!(
        by(None, Some("what")).unwrap_err().code,
        "section_not_found"
    );
    assert_eq!(
        by(Some("What"), None).unwrap_err().code,
        "section_not_found"
    );
    // Over the API: the old field alone still works, and `section` is new.
    let op = |extra: Value| {
        let mut op = serde_json::json!({"type": "read_section", "path": "doc.md"});
        for (k, v) in extra.as_object().unwrap() {
            op[k] = v.clone();
        }
        execute(&s, &root, 2, op)
    };
    assert_eq!(
        op(serde_json::json!({"section_id": "s2"}))["result"]["content"],
        s2.content
    );
    assert_eq!(
        op(serde_json::json!({"section": "What"}))["result"]["content"],
        s2.content
    );
    assert_eq!(error_code(&op(serde_json::json!({}))), "invalid_section");
    assert_eq!(
        error_code(&op(
            serde_json::json!({"section_id": "s2", "section": "What"})
        )),
        "invalid_section"
    );
    // Hidden stays not_found.
    let (_fx2, s, _root) = hidden_walk_vault();
    assert_eq!(
        s.read_section(
            &who("junior-j"),
            "senior-only/secret.md",
            None,
            None,
            Some("Secret")
        )
        .unwrap_err()
        .code,
        "not_found"
    );
}

fn search(s: &Service, who: &TrustedIdentity, query: &str) -> Value {
    let v = execute(
        s,
        who,
        2,
        serde_json::json!({"type": "search", "query": query}),
    );
    assert!(v["error"].is_null(), "{v}");
    v["result"].clone()
}

fn hit<'a>(result: &'a Value, path: &str) -> &'a Value {
    result["hits"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["path"] == path)
        .unwrap_or_else(|| panic!("no hit {path}: {result}"))
}

#[test]
fn search_hits_carry_section() {
    let (fx, s, root) = vault();
    seed(
        &s,
        &root,
        vec![
            put(
                "terms.md",
                "# Terms\n\nGeneral intro.\n\n## Warranty\n\nThe warranty lasts two years.\n",
            ),
            put(
                "nested.md",
                "# Top\n\n## Mid\n\n### Deep\n\nwarranty here\n\n## After\n",
            ),
            put(
                "intro.md",
                "Warranty is in the preamble.\n\n# Later\n\ntext\n",
            ),
            put("plain.txt", "a plain warranty"),
            binary("deck.pptx", b"deck bytes"),
        ],
    );
    cache_extraction(
        &fx,
        b"deck bytes",
        "pptx",
        ExtractionStatus::Succeeded,
        vec![segment(
            "Warranty slide",
            location(Some("Warranty"), Some(1)),
        )],
    );
    let r = search(&s, &root, "warranty");
    assert_eq!(
        hit(&r, "terms.md")["section"],
        serde_json::json!({"section_id": "s2", "title": "Warranty"})
    );
    // The deepest heading whose range holds the first matching word.
    assert_eq!(
        hit(&r, "nested.md")["section"],
        serde_json::json!({"section_id": "s3", "title": "Deep"})
    );
    assert!(hit(&r, "intro.md").get("section").is_none());
    assert!(hit(&r, "plain.txt").get("section").is_none());
    let deck = hit(&r, "deck.pptx");
    assert!(deck.get("section").is_none());
    assert_eq!(deck["source"]["location"]["section"], "Warranty");
    // A name-only hit has no matching word in the body, so no section.
    seed(&s, &root, vec![put("guarantee.md", "# Head\n\nnothing")]);
    let r = execute(
        &s,
        &root,
        2,
        serde_json::json!({"type": "search", "query": "guarantee", "fields": ["name"]}),
    );
    assert!(hit(&r["result"], "guarantee.md").get("section").is_none());
}

// ---- Review fixes (Tasks 3–4) ----

#[test]
fn neighbourhood_of_a_large_folder_is_linear() {
    let (_fx, s, root) = vault();
    for batch in 0..50 {
        seed(
            &s,
            &root,
            (0..100)
                .map(|i| put(&format!("big/m{batch:02}-{i:03}.md"), ""))
                .collect(),
        );
    }
    s.neighbourhood(&root, "big", 1, 64).unwrap();
    let started = std::time::Instant::now();
    let n = s.neighbourhood(&root, "big", 2, 64).unwrap();
    let took = started.elapsed();
    assert_eq!(n.nodes.len(), 65);
    assert!(n.truncated);
    assert_eq!(n.nodes[1].path, "big/m00-000.md");
    assert!(took < std::time::Duration::from_millis(1000), "{took:?}");
}

#[test]
fn neighbourhood_above_rank_follows_the_tenant_flag() {
    let (_fx, s, root) = above_rank_vault();
    let above = |w: &TrustedIdentity| -> Vec<(String, String)> {
        s.neighbourhood(w, "pub.md", 1, 32)
            .unwrap()
            .above_rank
            .into_iter()
            .map(|a| (a.target, a.seen_from))
            .collect()
    };
    assert_eq!(above(&agent("bot-s", Some("junior-j"))), vec![]);
    set_flag(&s, &root, true);
    assert_eq!(above(&who("junior-j")), budget());
    assert_eq!(above(&agent("bot-s", Some("junior-j"))), budget());
    assert_eq!(above(&agent("bot-j", Some("senior-h"))), vec![]);
    assert_eq!(above(&agent("bot-j", None)), vec![]);
}

#[test]
fn malformed_selector_answers_alike_for_hidden_and_absent() {
    let (_fx, s, _root) = hidden_walk_vault();
    let junior = who("junior-j");
    let long = "x".repeat(1025);
    for (id, title) in [
        (None, None),
        (Some("s1"), Some("Secret")),
        (None, Some("")),
        (None, Some(long.as_str())),
    ] {
        let hidden = s
            .read_section(&junior, "senior-only/secret.md", None, id, title)
            .unwrap_err();
        let absent = s
            .read_section(&junior, "absent.md", None, id, title)
            .unwrap_err();
        assert_eq!(hidden.code, "invalid_section");
        assert_eq!(
            serde_json::to_value(&hidden).unwrap(),
            serde_json::to_value(&absent).unwrap()
        );
    }
}

// ---- one typo forgiven ----

/// `(path, matched, fuzzy)` of each hit, in order.
fn found_typos(
    s: &Service,
    who: &TrustedIdentity,
    query: &str,
    typos: bool,
) -> Vec<(String, String, bool)> {
    s.find(who, query, "", 50, typos)
        .unwrap()
        .hits
        .into_iter()
        .map(|h| (h.path, h.matched, h.fuzzy))
        .collect()
}

fn typo_vault() -> (Fixture, Service, TrustedIdentity) {
    let (fx, s, root) = vault();
    seed(
        &s,
        &root,
        vec![
            put("Warranty.md", "# Warranty"),
            put("services/warranty-claims.md", "claims"),
            put("docs/warranty/terms.md", "terms"),
            put("waranty-notes.md", "a misspelt name"),
            put("old/waranty/list.md", "a misspelt folder"),
            put("Price list.md", "prices"),
            put("car.md", "a car"),
        ],
    );
    (fx, s, root)
}

#[test]
fn find_forgives_one_typo_after_exact_hits() {
    let (_fx, s, root) = typo_vault();
    let hits = found_typos(&s, &root, "warranty", true);
    // Name-exact, path-exact, name-fuzzy, path-fuzzy; newest and path within each.
    let group = |(_, matched, fuzzy): &(String, String, bool)| {
        (*fuzzy as u8) * 2 + (matched == "path") as u8
    };
    let groups: Vec<u8> = hits.iter().map(group).collect();
    assert!(groups.windows(2).all(|w| w[0] <= w[1]), "{hits:?}");
    let of = |g: u8| -> std::collections::BTreeSet<&str> {
        hits.iter()
            .filter(|h| group(h) == g)
            .map(|h| h.0.as_str())
            .collect()
    };
    assert_eq!(
        of(0),
        [
            "Warranty.md",
            "docs/warranty",
            "services/warranty-claims.md"
        ]
        .into()
    );
    assert_eq!(of(1), ["docs/warranty/terms.md"].into());
    assert_eq!(of(2), ["old/waranty", "waranty-notes.md"].into());
    assert_eq!(of(3), ["old/waranty/list.md"].into());
    assert_eq!(hits.len(), 7);
    // A transposition (8 chars) finds the exact names as fuzzy hits.
    let hits = found_typos(&s, &root, "warrnaty", true);
    assert!(
        hits.contains(&("Warranty.md".into(), "name".into(), true)),
        "{hits:?}"
    );
    assert!(hits.contains(&("services/warranty-claims.md".into(), "name".into(), true)));
    assert!(hits.iter().all(|h| h.2));
    // 5 chars, one substitution.
    assert_eq!(
        found_typos(&s, &root, "prise", true),
        vec![("Price list.md".into(), "name".into(), true)]
    );
    // Below 5 chars only exact prefixes match.
    assert!(found_typos(&s, &root, "cat", true).is_empty());
    assert!(found_typos(&s, &root, "carr", true).is_empty());
    // An exact hit carries no `fuzzy` key at all.
    let json = execute(
        &s,
        &root,
        2,
        serde_json::json!({"type": "find", "query": "warranty"}),
    );
    let hits = json["result"]["hits"].as_array().unwrap();
    assert!(hits[0].get("fuzzy").is_none(), "{json}");
    assert_eq!(hits.last().unwrap()["fuzzy"], true);
}

#[test]
fn find_typos_false_is_exact_search() {
    let (_fx, s, root) = typo_vault();
    assert!(found_typos(&s, &root, "warrnaty", false).is_empty());
    assert!(found_typos(&s, &root, "prise", false).is_empty());
    let exact = found_typos(&s, &root, "warranty", false);
    assert_eq!(exact.len(), 4);
    assert!(exact.iter().all(|h| !h.2));
    let v = execute(
        &s,
        &root,
        2,
        serde_json::json!({"type": "find", "query": "warrnaty", "typos": false}),
    );
    assert_eq!(v["result"]["hits"], serde_json::json!([]));
}

#[test]
fn find_typos_survive_an_index_that_cannot_reconcile() {
    let (fx, s, root) = vault();
    seed(&s, &root, vec![put("warranty.md", "# Warranty terms")]);
    seed(&s, &root, vec![put("warranty.md", "# New warranty terms")]);
    let blob = std::fs::canonicalize(&fx.0)
        .unwrap()
        .join("blobs")
        .join(format!("{:x}", Sha256::digest(b"# New warranty terms")));
    std::fs::remove_file(&blob).unwrap();
    assert_eq!(
        found_typos(&s, &root, "warrnaty", true),
        vec![("warranty.md".into(), "name".into(), true)]
    );
}

fn search_with(s: &Service, who: &TrustedIdentity, operation: Value) -> Value {
    let v = execute(s, who, 2, operation);
    assert!(v["error"].is_null(), "{v}");
    v["result"].clone()
}

fn hit_list(result: &Value) -> Vec<(String, bool)> {
    result["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| {
            (
                h["path"].as_str().unwrap().to_string(),
                h.get("fuzzy").is_some_and(|f| f == true),
            )
        })
        .collect()
}

#[test]
fn search_forgives_one_typo_and_ranks_exact_first() {
    let (_fx, s, root) = vault();
    seed(
        &s,
        &root,
        vec![
            put(
                "terms.md",
                "# Terms\n\nIntro.\n\n## Warranty\n\nThe warranty lasts two years.\n",
            ),
            put(
                "other.md",
                "A warrnaty typo lives here, and a warrnaty again.",
            ),
            put("cats.md", "the cat sat on the mat"),
        ],
    );
    let r = search(&s, &root, "warrnaty");
    assert_eq!(
        hit_list(&r),
        vec![("other.md".into(), false), ("terms.md".into(), true)]
    );
    // A fuzzy hit's snippet and section follow the near word.
    let terms = hit(&r, "terms.md");
    assert!(
        terms["snippet"]
            .as_str()
            .unwrap()
            .contains("warranty lasts"),
        "{terms}"
    );
    assert_eq!(
        terms["section"],
        serde_json::json!({"section_id": "s2", "title": "Warranty"})
    );
    let r = search(&s, &root, "warranty");
    assert_eq!(
        hit_list(&r),
        vec![("terms.md".into(), false), ("other.md".into(), true)]
    );
    // Below 5 chars, no edit is forgiven.
    assert_eq!(search(&s, &root, "cta")["hits"], serde_json::json!([]));
    assert_eq!(
        search(&s, &root, "cat")["hits"].as_array().unwrap().len(),
        1
    );
    // `typos: false` is the exact search.
    let exact = search_with(
        &s,
        &root,
        serde_json::json!({"type": "search", "query": "warranty", "typos": false}),
    );
    assert_eq!(hit_list(&exact), vec![("terms.md".into(), false)]);
    // `typos` is part of the snapshot.
    let snapshot = r["snapshot_id"].as_str().unwrap();
    assert_ne!(exact["snapshot_id"], r["snapshot_id"]);
    let stale = execute(
        &s,
        &root,
        2,
        serde_json::json!({"type": "search", "query": "warranty", "typos": false,
                           "snapshot_id": snapshot}),
    );
    assert_eq!(error_code(&stale), "stale_search");
    let same = search_with(
        &s,
        &root,
        serde_json::json!({"type": "search", "query": "warranty", "typos": true,
                           "snapshot_id": snapshot}),
    );
    assert_eq!(same["snapshot_id"], r["snapshot_id"]);
}

#[test]
fn typos_under_v1_refused() {
    let (_fx, s, root) = vault();
    for operation in [
        serde_json::json!({"type": "find", "query": "w", "typos": false}),
        serde_json::json!({"type": "search", "query": "w", "typos": true}),
    ] {
        assert_eq!(
            error_code(&execute(&s, &root, 1, operation.clone())),
            "unsupported_version"
        );
        assert!(execute(&s, &root, 2, operation)["error"].is_null());
    }
}

/// A hidden name one edit from the query never appears for a junior; a senior finds it.
#[test]
fn typos_never_reach_hidden_names() {
    let (_fx, s, root) = vault_with(&[
        ("junior-j", member("junior", ActorKind::Human)),
        ("senior-s", member("senior", ActorKind::Human)),
        ("bot-j", member("junior", ActorKind::Agent)),
    ]);
    let senior = who("senior-s");
    seed(
        &s,
        &root,
        vec![
            put("notes.md", "# Notes\n\nNothing here."),
            Change::CreateFolder {
                path: "senior-only".into(),
            },
            put(
                "senior-only/Warranty.md",
                "# Warranty\n\nThe warranty terms.",
            ),
        ],
    );
    seed(
        &s,
        &root,
        vec![Change::SetMeta {
            path: "senior-only".into(),
            seen_from: Some("senior".into()),
            colour: None,
        }],
    );
    let found = found_typos(&s, &senior, "warrnaty", true);
    assert!(
        found.contains(&("senior-only/Warranty.md".into(), "name".into(), true)),
        "{found:?}"
    );
    let found = search(&s, &senior, "warrnaty");
    assert!(
        found.to_string().contains("senior-only/Warranty.md"),
        "{found}"
    );
    for reader in [
        who("junior-j"),
        agent("bot-j", None),
        agent("bot-j", Some("junior-j")),
    ] {
        assert!(
            found_typos(&s, &reader, "warrnaty", true).is_empty(),
            "{reader:?}"
        );
        let json =
            serde_json::to_string(&s.find(&reader, "warrnaty", "", 50, true).unwrap()).unwrap();
        assert!(
            !json.contains("arrant") && !json.contains("senior-only"),
            "{json}"
        );
        let found = search(&s, &reader, "warrnaty");
        assert!(
            !found.to_string().contains("arrant") && !found.to_string().contains("senior-only"),
            "{found}"
        );
    }
}

// ---- Final review I1: the parsed graph index is cached per tenant ----

#[test]
fn find_parses_the_graph_index_once_while_it_is_unchanged() {
    let (fx, s, root) = vault();
    seed(&s, &root, vec![put("warranty.md", "# Warranty terms")]);
    // `s` writes the index file; a second service over the same store starts cold.
    s.find(&root, "warranty", "", 20, true).unwrap();
    let cold = Service::open(&fx.0).unwrap();
    assert_eq!(cold.graph_index_parses(), 0);
    let first = cold.find(&root, "warranty", "", 20, true).unwrap();
    assert_eq!(first.hits[0].title, "Warranty terms");
    assert_eq!(cold.graph_index_parses(), 1);
    let second = cold.find(&root, "warrant", "", 20, true).unwrap();
    assert_eq!(second.hits[0].title, "Warranty terms");
    assert_eq!(cold.graph_index_parses(), 1);
    // `graph`, `links` and `neighbourhood` read through the same entry.
    cold.graph(&root, "", 0, None).unwrap();
    cold.links(&root, "warranty.md").unwrap();
    cold.neighbourhood(&root, "warranty.md", 1, 10).unwrap();
    assert_eq!(cold.graph_index_parses(), 1);
    // An edit through this service rewrites the file and refreshes the entry itself.
    seed(&cold, &root, vec![put("warranty.md", "# Warranty, edited")]);
    let f = cold.find(&root, "warranty", "", 20, true).unwrap();
    assert_eq!(f.hits[0].title, "Warranty, edited");
    cold.find(&root, "warranty", "", 20, true).unwrap();
    assert_eq!(cold.graph_index_parses(), 1);
    // A rewrite by another process (here `s`) is seen by the file's metadata.
    seed(&s, &root, vec![put("warranty.md", "# Warranty, elsewhere")]);
    s.graph(&root, "", 0, None).unwrap();
    let f = cold.find(&root, "warranty", "", 20, true).unwrap();
    assert_eq!(f.hits[0].title, "Warranty, elsewhere");
    assert_eq!(cold.graph_index_parses(), 2);
    cold.find(&root, "warranty", "", 20, true).unwrap();
    assert_eq!(cold.graph_index_parses(), 2);
}

#[test]
fn find_on_an_unchanged_index_is_faster_than_the_first_parse() {
    let (fx, s, root) = vault();
    let body: String = (0..40)
        .map(|i| format!("## Heading {i}\n\nParagraph {i} with [[note-{i}]] in it.\n\n"))
        .collect();
    // 1,500 texts, 100 per edit (the changes limit).
    for batch in 0..15 {
        let changes = (batch * 100..batch * 100 + 100)
            .map(|i| {
                let path = format!("notes/n{}/note-{i}.md", i % 30);
                put(&path, &format!("# Note {i}\n\n{body}"))
            })
            .collect();
        seed(&s, &root, changes);
    }
    s.graph(&root, "", 0, Some(1)).unwrap();
    let cold = Service::open(&fx.0).unwrap();
    // A five-letter query that matches nothing: the index is read for its words only.
    let t = std::time::Instant::now();
    cold.find(&root, "qwxyz", "", 20, true).unwrap();
    let first = t.elapsed();
    // The rest of a call is the metadata transaction, the same both times; the best of
    // three cached calls is compared, to keep scheduler noise out.
    let second = (0..3)
        .map(|_| {
            let t = std::time::Instant::now();
            cold.find(&root, "qwxyz", "", 20, true).unwrap();
            t.elapsed()
        })
        .min()
        .unwrap();
    assert_eq!(cold.graph_index_parses(), 1);
    // Observed: 42 ms then 7 ms (release), 321 ms then 74 ms (debug).
    assert!(second * 2 < first, "first {first:?}, second {second:?}");
}
