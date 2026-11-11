use mnemosyne_content::{
    api::{Operation, Request},
    auth::{ActorKind, Grant, Member, Permission, Policy, TenantPolicy, TrustedIdentity},
    find::FindResult,
    graph::{AboveRank, EdgeKind, GraphResult, GraphUnresolved, IndexStatus},
    links::{extract_links, resolve, Link, LinkKind, Resolution},
    ranks::ColourSource,
    Change, Entry, EntryKind, Service,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

fn link(target: &str, kind: LinkKind, alias: Option<&str>) -> Link {
    Link {
        target: target.into(),
        kind,
        alias: alias.map(Into::into),
        heading: None,
    }
}

#[test]
fn extracts_markdown_and_wikilinks_and_ignores_code_and_external() {
    let text = "# Notes\n\n\
[a](services/warranty.md) and [b](../pricing/Price%20list.md#rates)\n\n\
![img](photos/before.jpg)\n\n\
See [[Aftercare|see]] and [[Facts#What]].\n\n\
[x](https://example.com) [y](/abs) `[[not a link]]`\n\n\
```\n[z](hidden.md)\n[[hidden]]\n```\n";
    assert_eq!(
        extract_links(text),
        vec![
            link("services/warranty.md", LinkKind::Markdown, None),
            link("../pricing/Price list.md", LinkKind::Markdown, None),
            link("photos/before.jpg", LinkKind::Image, None),
            link("Aftercare", LinkKind::Wikilink, Some("see")),
            Link {
                heading: Some("What".into()),
                ..link("Facts", LinkKind::Wikilink, None)
            },
        ]
    );
}

#[test]
fn extraction_is_bounded() {
    assert!(extract_links(&"x".repeat(2 * 1024 * 1024 + 1)).is_empty());
    let many = "[[a]] ".repeat(5000);
    assert_eq!(extract_links(&many).len(), 4096);
}

fn manifest(files: &[&str], folders: &[&str]) -> BTreeMap<String, Entry> {
    let mut m = BTreeMap::new();
    for (paths, kind) in [(files, EntryKind::Text), (folders, EntryKind::Folder)] {
        for p in paths {
            m.insert(
                p.to_string(),
                Entry {
                    file_id: format!("id-{p}"),
                    kind: kind.clone(),
                    blob: None,
                    original_blob: None,
                    meta: Default::default(),
                },
            );
        }
    }
    m
}

fn resolved(p: &str) -> Resolution {
    Resolution::Resolved { path: p.into() }
}

fn unresolved(t: &str, c: &[&str]) -> Resolution {
    Resolution::Unresolved {
        target: t.into(),
        candidates: c.iter().map(|s| s.to_string()).collect(),
    }
}

#[test]
fn resolves_relative_paths_and_basenames() {
    let m = manifest(
        &[
            "services/intro.md",
            "services/warranty.md",
            "pricing/Price list.md",
            "a/Warranty.md",
            "b/Warranty.md",
            "Aftercare.md",
            "renamed-new.md",
        ],
        &["services", "pricing", "a", "b"],
    );
    let md = |t: &str| link(t, LinkKind::Markdown, None);
    let wiki = |t: &str| link(t, LinkKind::Wikilink, None);

    // relative, ../, and the .md fallback
    assert_eq!(
        resolve("services/intro.md", &md("warranty.md"), &m),
        resolved("services/warranty.md")
    );
    assert_eq!(
        resolve("services/intro.md", &md("warranty"), &m),
        resolved("services/warranty.md")
    );
    assert_eq!(
        resolve("services/intro.md", &md("./warranty.md"), &m),
        resolved("services/warranty.md")
    );
    assert_eq!(
        resolve("services/intro.md", &md("../pricing/Price list.md"), &m),
        resolved("pricing/Price list.md")
    );
    assert_eq!(
        resolve("root.md", &md("services/warranty.md"), &m),
        resolved("services/warranty.md")
    );

    // Review Focus 1: the old name of a renamed file is unresolved
    assert_eq!(
        resolve("root.md", &md("renamed-old.md"), &m),
        unresolved("renamed-old.md", &[])
    );
    assert_eq!(
        resolve("root.md", &wiki("renamed-old"), &m),
        unresolved("renamed-old", &[])
    );

    // basename wikilinks, case-sensitive, .md optional
    assert_eq!(
        resolve("services/intro.md", &wiki("Aftercare"), &m),
        resolved("Aftercare.md")
    );
    assert_eq!(
        resolve("services/intro.md", &wiki("Aftercare.md"), &m),
        resolved("Aftercare.md")
    );
    assert_eq!(
        resolve("services/intro.md", &wiki("aftercare"), &m),
        unresolved("aftercare", &[])
    );

    // Review Focus 2: ambiguous basename
    assert_eq!(
        resolve("root.md", &wiki("Warranty"), &m),
        unresolved("Warranty", &["a/Warranty.md", "b/Warranty.md"])
    );

    // above the tenant root
    assert_eq!(
        resolve("services/intro.md", &md("../../x.md"), &m),
        unresolved("../../x.md", &[])
    );
    assert_eq!(
        resolve("root.md", &md("../Aftercare.md"), &m),
        unresolved("../Aftercare.md", &[])
    );

    // folders are not link targets
    assert_eq!(
        resolve("root.md", &md("services"), &m),
        unresolved("services", &[])
    );
    assert_eq!(
        resolve("root.md", &md("services/"), &m),
        unresolved("services/", &[])
    );
    assert_eq!(
        resolve("root.md", &wiki("services"), &m),
        unresolved("services", &[])
    );
}

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(std::path::PathBuf);
impl Fixture {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "mnemosyne-graph-{}-{}",
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

/// Tenant "t1" with a root administrator.
fn vault() -> (Fixture, Service, TrustedIdentity) {
    let fx = Fixture::new();
    let s =
        Service::open_with_clock(&fx.0, Box::new(|| "2026-10-28T22:00:00.000Z".into())).unwrap();
    let root = TrustedIdentity {
        tenant_id: "t1".into(),
        actor_id: "root".into(),
        kind: ActorKind::Human,
        on_behalf_of: None,
    };
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
                    Member {
                        role: "administrator".into(),
                        kind: ActorKind::Human,
                    },
                )]),
                ..TenantPolicy::default()
            },
        )]),
    })
    .unwrap();
    (fx, s, root)
}

fn put(path: &str, content: &str) -> Change {
    Change::Put {
        path: path.into(),
        content: content.into(),
    }
}

fn status(v: &Value) -> &str {
    v["status"].as_str().unwrap()
}
fn node<'a>(v: &'a Value, path: &str) -> &'a Value {
    v["index"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["path"] == path)
        .unwrap_or_else(|| panic!("no node {path}"))
}
fn edges(v: &Value, kind: &str) -> Vec<(String, String)> {
    v["index"]["edges"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == kind)
        .map(|e| {
            (
                e["from"].as_str().unwrap().to_string(),
                e["to"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}
fn pair(a: &str, b: &str) -> (String, String) {
    (a.into(), b.into())
}

#[test]
fn index_builds_and_reconciles() {
    let (fx, s, root) = vault();
    let r1 = s
        .edit(
            &root,
            "r0",
            vec![
                put(
                    "a.md",
                    "See [b](notes/deep/b.md), [[b]] twice, and [[Missing]].",
                ),
                put("notes/deep/b.md", "no links yet"),
                put("notes/c.md", "[[a]] ![x](../img/x.png)"),
                Change::PutBinary {
                    path: "img/x.png".into(),
                    bytes: b"[[a]] not parsed".to_vec(),
                },
            ],
            "k1",
            None,
            None,
        )
        .unwrap();

    // First call: built from scratch.
    let v = s.graph_index(&root).unwrap();
    assert_eq!(status(&v), "rebuilt");
    assert_eq!(v["index"]["revision_id"], r1.id.as_str());
    // Schema 2 adds titles, snippets and headings.
    assert_eq!(v["index"]["schema"], 2);
    assert_eq!(
        edges(&v, "links"),
        vec![
            pair("a.md", "notes/deep/b.md"),
            pair("notes/c.md", "a.md"),
            pair("notes/c.md", "img/x.png"),
        ]
    );
    // Implicit parent folders are nodes with contains edges to direct children.
    assert_eq!(
        edges(&v, "contains"),
        vec![
            pair("img", "img/x.png"),
            pair("notes", "notes/c.md"),
            pair("notes", "notes/deep"),
            pair("notes/deep", "notes/deep/b.md"),
        ]
    );
    assert_eq!(node(&v, "notes")["kind"], "folder");
    assert_eq!(node(&v, "notes")["members"], 2);
    assert_eq!(node(&v, "notes/deep")["members"], 1);
    assert_eq!(node(&v, "a.md")["out_links"], 1);
    assert_eq!(node(&v, "a.md")["in_links"], 1);
    assert_eq!(node(&v, "notes/deep/b.md")["in_links"], 1);
    assert_eq!(node(&v, "img/x.png")["kind"], "binary");
    assert_eq!(node(&v, "img/x.png")["out_links"], 0);
    assert_eq!(node(&v, "img/x.png")["in_links"], 1);
    assert_eq!(
        v["index"]["unresolved"],
        serde_json::json!([{"from": "a.md", "target": "Missing", "candidates": []}])
    );
    // Stored under graph-v1/<sha256(tenant)>.json.
    let store = std::fs::canonicalize(&fx.0).unwrap().join("graph-v1");
    let file = store.join(format!("{:x}.json", Sha256::digest(b"t1")));
    assert!(file.exists());

    // Second call: nothing changed.
    assert_eq!(status(&s.graph_index(&root).unwrap()), "current");

    // Review Focus 5: an edit that adds a link is never served as `current`.
    let r2 = s
        .edit(
            &root,
            &r1.id,
            vec![put("notes/deep/b.md", "back to [a](../../a.md)")],
            "k2",
            None,
            None,
        )
        .unwrap();
    // Only changed blobs are re-parsed: break the unchanged a.md blob on disk; an
    // incremental reconcile must not read it.
    let a_blob = std::fs::canonicalize(&fx.0)
        .unwrap()
        .join("blobs")
        .join(format!(
            "{:x}",
            Sha256::digest(b"See [b](notes/deep/b.md), [[b]] twice, and [[Missing]].")
        ));
    let a_bytes = std::fs::read(&a_blob).unwrap();
    std::fs::write(&a_blob, b"garbage").unwrap();
    let v = s.graph_index(&root).unwrap();
    std::fs::write(&a_blob, &a_bytes).unwrap();
    assert_eq!(status(&v), "incremental");
    assert_eq!(v["index"]["revision_id"], r2.id.as_str());
    assert!(edges(&v, "links").contains(&pair("notes/deep/b.md", "a.md")));
    assert_eq!(node(&v, "a.md")["in_links"], 2);
    assert_eq!(node(&v, "notes/deep/b.md")["out_links"], 1);
    assert_eq!(status(&s.graph_index(&root).unwrap()), "current");

    // A removed file leaves the index; its edges and backlinks go with it.
    s.edit(
        &root,
        &r2.id,
        vec![Change::Delete {
            path: "notes/c.md".into(),
        }],
        "k3",
        None,
        None,
    )
    .unwrap();
    let v = s.graph_index(&root).unwrap();
    assert_eq!(status(&v), "incremental");
    assert_eq!(
        edges(&v, "links"),
        vec![
            pair("a.md", "notes/deep/b.md"),
            pair("notes/deep/b.md", "a.md")
        ]
    );
    assert_eq!(node(&v, "notes")["members"], 1);
    assert_eq!(node(&v, "img/x.png")["in_links"], 0);

    // A corrupt cache file is rebuilt, with the same result.
    std::fs::write(&file, b"{not json").unwrap();
    let rebuilt = s.graph_index(&root).unwrap();
    assert_eq!(status(&rebuilt), "rebuilt");
    assert_eq!(rebuilt["index"], v["index"]);
    assert_eq!(status(&s.graph_index(&root).unwrap()), "current");
}

fn links_of(v: &Value) -> Vec<(String, String)> {
    edges(v, "links")
}

#[test]
fn index_reresolves_after_a_move() {
    let (_fx, s, root) = vault();
    let r1 = s
        .edit(
            &root,
            "r0",
            vec![
                put("a/x.md", "[y](y.md)"),
                put("a/y.md", "in a"),
                put("b/y.md", "in b"),
            ],
            "k1",
            None,
            None,
        )
        .unwrap();
    let v = s.graph_index(&root).unwrap();
    assert_eq!(links_of(&v), vec![pair("a/x.md", "a/y.md")]);
    s.edit(
        &root,
        &r1.id,
        vec![Change::Rename {
            path: "a/x.md".into(),
            to: "b/x.md".into(),
        }],
        "k2",
        None,
        None,
    )
    .unwrap();
    // The blob is unchanged and its links are reused, but they resolve from the new place.
    let v = s.graph_index(&root).unwrap();
    assert_eq!(status(&v), "incremental");
    assert_eq!(links_of(&v), vec![pair("b/x.md", "b/y.md")]);
    assert_eq!(node(&v, "a/y.md")["in_links"], 0);
}

#[test]
fn wikilink_turns_ambiguous_when_a_namesake_appears() {
    let (_fx, s, root) = vault();
    let r1 = s
        .edit(
            &root,
            "r0",
            vec![put("a.md", "[[W]]"), put("n/W.md", "one")],
            "k1",
            None,
            None,
        )
        .unwrap();
    let v = s.graph_index(&root).unwrap();
    assert_eq!(links_of(&v), vec![pair("a.md", "n/W.md")]);
    s.edit(&root, &r1.id, vec![put("m/W.md", "two")], "k2", None, None)
        .unwrap();
    let v = s.graph_index(&root).unwrap();
    assert_eq!(status(&v), "incremental");
    assert!(links_of(&v).is_empty());
    assert_eq!(
        v["index"]["unresolved"],
        serde_json::json!([{"from": "a.md", "target": "W", "candidates": ["m/W.md", "n/W.md"]}])
    );
}

#[test]
fn index_of_another_grammar_is_rebuilt() {
    let (fx, s, root) = vault();
    s.edit(&root, "r0", vec![put("a.md", "[[b]]")], "k1", None, None)
        .unwrap();
    let v = s.graph_index(&root).unwrap();
    assert_eq!(
        v["index"]["grammar"],
        mnemosyne_content::links::GRAMMAR_VERSION
    );
    let file = std::fs::canonicalize(&fx.0)
        .unwrap()
        .join("graph-v1")
        .join(format!("{:x}.json", Sha256::digest(b"t1")));
    let mut stored: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    stored["grammar"] = (mnemosyne_content::links::GRAMMAR_VERSION + 1).into();
    std::fs::write(&file, serde_json::to_vec(&stored).unwrap()).unwrap();
    assert_eq!(status(&s.graph_index(&root).unwrap()), "rebuilt");
    assert_eq!(status(&s.graph_index(&root).unwrap()), "current");
}

// ---- graph and links operations over a rank ladder ----

fn who(actor: &str) -> TrustedIdentity {
    TrustedIdentity {
        tenant_id: "t1".into(),
        actor_id: actor.into(),
        kind: ActorKind::Human,
        on_behalf_of: None,
    }
}

/// Tenant "t1": root administrator, `junior-j` and `senior-s`; `senior-only/` is seen
/// from senior.
fn ranked() -> (Fixture, Service, TrustedIdentity) {
    let fx = Fixture::new();
    // Each commit is stamped one second after the last, so `changed.at` orders edits.
    let tick = std::sync::Arc::new(AtomicU64::new(0));
    let s = Service::open_with_clock(
        &fx.0,
        Box::new(move || {
            let n = tick.fetch_add(1, Ordering::Relaxed);
            format!("2026-10-28T22:{:02}:{:02}.000Z", n / 60, n % 60)
        }),
    )
    .unwrap();
    let root = who("root");
    let member = |role: &str| Member {
        role: role.into(),
        kind: ActorKind::Human,
    };
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
                members: BTreeMap::from([
                    ("root".to_string(), member("administrator")),
                    ("junior-j".to_string(), member("junior")),
                    ("senior-s".to_string(), member("senior")),
                ]),
                ..TenantPolicy::default()
            },
        )]),
    })
    .unwrap();
    let r1 = s
        .edit(
            &root,
            "r0",
            vec![
                put("pub.md", "[pay](senior-only/pay.md), [[Ghost]] and [[W]]"),
                Change::CreateFolder {
                    path: "senior-only".into(),
                },
                put("senior-only/pay.md", "[back](../pub.md)"),
                put("senior-only/W.md", "hidden namesake"),
                put("senior-only/secret.md", "[[Q3 layoffs]]"),
                put("notes/a.md", "[p](../pub.md)"),
                put("notes/W.md", "visible namesake"),
            ],
            "seed",
            None,
            None,
        )
        .unwrap();
    s.edit(
        &root,
        &r1.id,
        vec![Change::SetMeta {
            path: "senior-only".into(),
            seen_from: Some("senior".into()),
            colour: None,
        }],
        "hide",
        None,
        None,
    )
    .unwrap();
    (fx, s, root)
}

fn code<T: std::fmt::Debug>(r: Result<T, mnemosyne_content::ContentError>) -> String {
    r.unwrap_err().code
}

fn graph_paths(g: &GraphResult) -> Vec<&str> {
    g.nodes.iter().map(|n| n.path.as_str()).collect()
}
fn graph_edges(g: &GraphResult, kind: EdgeKind) -> Vec<(String, String)> {
    g.edges
        .iter()
        .filter(|e| e.kind == kind)
        .map(|e| pair(&e.from, &e.to))
        .collect()
}
fn unresolved_of(u: &[GraphUnresolved]) -> Vec<(String, String, Vec<String>)> {
    u.iter()
        .map(|u| (u.from.clone(), u.target.clone(), u.candidates.clone()))
        .collect()
}
fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

#[test]
fn graph_is_filtered_by_rank_and_scope() {
    let (_fx, s, root) = ranked();
    let head = s.head(&root).unwrap();
    let junior = who("junior-j");
    let senior = who("senior-s");

    // Review Focus 3: a junior sees no hidden node and no edge to or from one.
    let g = s.graph(&junior, "", 0, None).unwrap();
    assert_eq!(g.revision_id, head);
    assert_eq!(g.index.revision_id, head);
    assert_eq!(g.index.status, IndexStatus::Rebuilt);
    assert_eq!(
        graph_paths(&g),
        vec!["notes", "notes/W.md", "notes/a.md", "pub.md"]
    );
    assert!(g
        .edges
        .iter()
        .all(|e| !e.from.starts_with("senior-only") && !e.to.starts_with("senior-only")));
    assert_eq!(
        graph_edges(&g, EdgeKind::Links),
        vec![pair("notes/a.md", "pub.md")]
    );
    assert_eq!(
        graph_edges(&g, EdgeKind::Contains),
        vec![pair("notes", "notes/W.md"), pair("notes", "notes/a.md")]
    );
    // The hidden namesake is not a candidate; the link stays unresolved all the same.
    assert_eq!(
        unresolved_of(&g.unresolved),
        vec![
            ("pub.md".into(), "Ghost".into(), vec![]),
            ("pub.md".into(), "W".into(), strings(&["notes/W.md"])),
        ]
    );
    assert!(g.unresolved.iter().all(|u| u.target != "Q3 layoffs"));
    // Degrees are the full counts: one hidden backlink and one hidden target.
    let pub_md = g.nodes.iter().find(|n| n.path == "pub.md").unwrap();
    assert_eq!((pub_md.in_links, pub_md.out_links), (2, 1));
    assert_eq!(pub_md.kind, EntryKind::Text);
    assert!(!pub_md.file_id.is_empty());
    assert_eq!(pub_md.seen_from, "junior");
    assert_eq!(pub_md.colour, "other");
    assert_eq!(pub_md.colour_source, ColourSource::Default);
    let notes = g.nodes.iter().find(|n| n.path == "notes").unwrap();
    assert_eq!((notes.kind.clone(), notes.members), (EntryKind::Folder, 2));
    assert_eq!(
        s.graph(&junior, "", 0, None).unwrap().index.status,
        IndexStatus::Current
    );

    // A senior sees the whole graph.
    let g = s.graph(&senior, "", 0, None).unwrap();
    assert_eq!(
        graph_paths(&g),
        vec![
            "notes",
            "notes/W.md",
            "notes/a.md",
            "pub.md",
            "senior-only",
            "senior-only/W.md",
            "senior-only/pay.md",
            "senior-only/secret.md"
        ]
    );
    assert_eq!(
        graph_edges(&g, EdgeKind::Links),
        vec![
            pair("notes/a.md", "pub.md"),
            pair("pub.md", "senior-only/pay.md"),
            pair("senior-only/pay.md", "pub.md"),
        ]
    );
    let sf = g
        .nodes
        .iter()
        .find(|n| n.path == "senior-only/pay.md")
        .unwrap();
    assert_eq!(sf.seen_from, "senior");
    assert_eq!(
        unresolved_of(&g.unresolved)[1].2,
        strings(&["notes/W.md", "senior-only/W.md"])
    );

    // Scope: the path itself and its descendants; edges need both ends inside.
    let g = s.graph(&junior, "notes", 0, None).unwrap();
    assert_eq!(graph_paths(&g), vec!["notes", "notes/W.md", "notes/a.md"]);
    assert!(graph_edges(&g, EdgeKind::Links).is_empty());
    assert!(g.unresolved.is_empty());
    let g = s.graph(&senior, "senior-only", 0, None).unwrap();
    assert_eq!(
        graph_paths(&g),
        vec![
            "senior-only",
            "senior-only/W.md",
            "senior-only/pay.md",
            "senior-only/secret.md"
        ]
    );
    assert_eq!(
        unresolved_of(&g.unresolved),
        vec![("senior-only/secret.md".into(), "Q3 layoffs".into(), vec![])]
    );
    let g = s.graph(&junior, "pub.md", 0, None).unwrap();
    assert_eq!(graph_paths(&g), vec!["pub.md"]);
    assert_eq!(unresolved_of(&g.unresolved).len(), 2);
}

#[test]
fn graph_scope_hidden_is_not_found() {
    let (_fx, s, _root) = ranked();
    let junior = who("junior-j");
    for scope in [
        "senior-only",
        "senior-only/pay.md",
        "absent",
        "notes/absent.md",
    ] {
        assert_eq!(
            code(s.graph(&junior, scope, 0, None)),
            "not_found",
            "{scope}"
        );
    }
    assert_eq!(code(s.graph(&junior, "../x", 0, None)), "invalid_path");
    assert!(s.graph(&who("senior-s"), "senior-only", 0, None).is_ok());
    assert_eq!(
        code(s.graph(&who("senior-s"), "absent", 0, None)),
        "not_found"
    );
}

#[test]
fn links_reports_above_rank_targets_but_hides_backlinks_from_hidden_files() {
    let (_fx, s, root) = ranked();
    let head = s.head(&root).unwrap();
    let junior = who("junior-j");
    let senior = who("senior-s");

    // Review Focus 3: the hidden target is named with its seen_from; the hidden
    // backlink is dropped without a trace.
    let l = s.links(&junior, "pub.md").unwrap();
    assert_eq!(l.path, "pub.md");
    assert_eq!(l.revision_id, head);
    assert!(l.out.is_empty());
    assert_eq!(l.r#in, strings(&["notes/a.md"]));
    assert_eq!(
        l.above_rank,
        vec![AboveRank {
            target: "senior-only/pay.md".into(),
            seen_from: "senior".into()
        }]
    );
    assert_eq!(
        unresolved_of(&l.unresolved),
        vec![
            ("pub.md".into(), "Ghost".into(), vec![]),
            ("pub.md".into(), "W".into(), strings(&["notes/W.md"])),
        ]
    );
    let json = serde_json::to_value(&l).unwrap();
    assert_eq!(json["in"], serde_json::json!(["notes/a.md"]));
    assert!(!json.to_string().contains("senior-only/W.md"));

    let l = s.links(&junior, "notes/a.md").unwrap();
    assert_eq!(l.out, strings(&["pub.md"]));
    assert!(l.r#in.is_empty() && l.above_rank.is_empty() && l.unresolved.is_empty());

    // A hidden or absent path answers not_found alike.
    assert_eq!(code(s.links(&junior, "senior-only/pay.md")), "not_found");
    assert_eq!(code(s.links(&junior, "absent.md")), "not_found");
    assert_eq!(code(s.links(&junior, "")), "invalid_path");

    // A senior sees both directions and nothing above rank.
    let l = s.links(&senior, "pub.md").unwrap();
    assert_eq!(l.out, strings(&["senior-only/pay.md"]));
    assert_eq!(l.r#in, strings(&["notes/a.md", "senior-only/pay.md"]));
    assert!(l.above_rank.is_empty());
    let l = s.links(&senior, "senior-only/pay.md").unwrap();
    assert_eq!(l.out, strings(&["pub.md"]));
    assert_eq!(l.r#in, strings(&["pub.md"]));
}

#[test]
fn v1_refused() {
    let (_fx, s, _root) = ranked();
    let junior = who("junior-j");
    let request = |version, operation| Request {
        version,
        idempotency_key: None,
        operation,
    };
    let graph = || Operation::Graph {
        scope: String::new(),
        offset: 0,
        limit: None,
    };
    let links = || Operation::Links {
        path: "pub.md".into(),
    };
    for op in [graph(), links()] {
        let r = s.execute(&junior, request(1, op));
        assert_eq!(r.error.unwrap().code, "unsupported_version");
    }
    let r = s.execute(&junior, request(2, graph()));
    assert!(r.error.is_none(), "{:?}", r.error);
    let v = serde_json::to_value(&r).unwrap();
    assert_eq!(v["result"]["index"]["status"], "rebuilt");
    assert!(v["result"]["nodes"].as_array().unwrap().len() == 4);
    let r = s.execute(&junior, request(2, links()));
    let v = serde_json::to_value(&r).unwrap();
    assert_eq!(v["result"]["above_rank"][0]["seen_from"], "senior");
    // The operation parses from JSON with `scope` defaulted.
    let parsed: Request =
        serde_json::from_str(r#"{"version":2,"operation":{"type":"graph"}}"#).unwrap();
    assert!(matches!(parsed.operation, Operation::Graph { ref scope, .. } if scope.is_empty()));
}

#[test]
fn links_omits_lineage_hidden_targets_from_above_rank() {
    let (_fx, s, root) = ranked();
    let head = s.head(&root).unwrap();
    // Moved out of the senior-only folder: readable by rank at its new name, but its
    // identity once lived under a hidden one.
    s.edit(
        &root,
        &head,
        vec![
            Change::Rename {
                path: "senior-only/W.md".into(),
                to: "moved.md".into(),
            },
            put("link.md", "[m](moved.md)"),
        ],
        "move",
        None,
        None,
    )
    .unwrap();
    let l = s.links(&who("junior-j"), "link.md").unwrap();
    assert!(l.out.is_empty(), "{:?}", l.out);
    assert!(l.above_rank.is_empty(), "{:?}", l.above_rank);
    let l = s.links(&who("senior-s"), "link.md").unwrap();
    assert_eq!(l.out, strings(&["moved.md"]));
    assert!(l.above_rank.is_empty());
}

#[test]
fn graph_hides_folders_without_visible_children() {
    let (_fx, s, root) = ranked();
    let head = s.head(&root).unwrap();
    // `plans` is an implicit folder whose only file is seen from senior; `newdir` an
    // implicit folder whose only file is lineage-hidden (it came from senior-only).
    let r = s
        .edit(
            &root,
            &head,
            vec![
                put("plans/layoffs.md", "q3"),
                Change::Rename {
                    path: "senior-only/W.md".into(),
                    to: "newdir/W.md".into(),
                },
            ],
            "k-plans",
            None,
            None,
        )
        .unwrap();
    s.edit(
        &root,
        &r.id,
        vec![Change::SetMeta {
            path: "plans/layoffs.md".into(),
            seen_from: Some("senior".into()),
            colour: None,
        }],
        "k-hide",
        None,
        None,
    )
    .unwrap();
    let junior = who("junior-j");
    let g = s.graph(&junior, "", 0, None).unwrap();
    let paths = graph_paths(&g);
    for hidden in ["plans", "plans/layoffs.md", "newdir", "newdir/W.md"] {
        assert!(!paths.contains(&hidden), "{hidden} in {paths:?}");
    }
    // Scopes the upfront rank check does not answer: an implicit folder with only
    // hidden children, and a lineage-hidden file and its implicit folder.
    for scope in ["plans", "newdir", "newdir/W.md"] {
        assert_eq!(
            code(s.graph(&junior, scope, 0, None)),
            "not_found",
            "{scope}"
        );
        assert_eq!(code(s.links(&junior, scope)), "not_found", "{scope}");
    }
    let senior = who("senior-s");
    let paths: Vec<String> = s
        .graph(&senior, "", 0, None)
        .unwrap()
        .nodes
        .into_iter()
        .map(|n| n.path)
        .collect();
    for shown in ["plans", "plans/layoffs.md", "newdir", "newdir/W.md"] {
        assert!(paths.iter().any(|p| p == shown), "{shown} missing");
    }
    assert_eq!(
        graph_paths(&s.graph(&senior, "plans", 0, None).unwrap()),
        vec!["plans", "plans/layoffs.md"]
    );
    assert!(s.graph(&senior, "newdir", 0, None).is_ok());
}

#[test]
fn above_rank_reports_the_link_text_not_the_hidden_path() {
    let (_fx, s, root) = ranked();
    let head = s.head(&root).unwrap();
    s.edit(
        &root,
        &head,
        vec![put("wl.md", "[[pay]]")],
        "k-wl",
        None,
        None,
    )
    .unwrap();
    let l = s.links(&who("junior-j"), "wl.md").unwrap();
    assert_eq!(
        l.above_rank,
        vec![AboveRank {
            target: "pay".into(),
            seen_from: "senior".into()
        }]
    );
    let json = serde_json::to_string(&l).unwrap();
    assert!(!json.contains("senior-only"), "{json}");
    let g = serde_json::to_string(&s.graph(&who("junior-j"), "wl.md", 0, None).unwrap()).unwrap();
    assert!(!g.contains("senior-only"), "{g}");
    // A senior reads the target and gets the resolved path.
    let l = s.links(&who("senior-s"), "wl.md").unwrap();
    assert_eq!(l.out, strings(&["senior-only/pay.md"]));
}

fn found(f: &FindResult) -> Vec<(&str, &str)> {
    f.hits
        .iter()
        .map(|h| (h.path.as_str(), h.matched.as_str()))
        .collect()
}

#[test]
fn find_matches_names_then_paths_in_order() {
    let (_fx, s, root) = ranked();
    let head = s.head(&root).unwrap();
    let r = s
        .edit(
            &root,
            &head,
            vec![
                put("Warranty.md", "terms"),
                put("services/warranty-claims.md", "claims"),
                put("software.md", "soft"),
                put("warranty/terms.md", "t"),
                put("notes/\u{c9}ch\u{e9}ancier.md", "dates"),
                put("senior-only/warranty-secret.md", "hidden"),
            ],
            "k-find",
            None,
            None,
        )
        .unwrap();
    let junior = who("junior-j");

    // Review Focus 4: a basename prefix matches, `software.md` does not; names rank
    // before paths; the implicit `warranty` folder has no stamp and comes last of names.
    let f = s.find(&junior, "war", "", 20, true).unwrap();
    assert_eq!(f.revision_id, r.id);
    assert_eq!(
        found(&f),
        vec![
            ("Warranty.md", "name"),
            ("services/warranty-claims.md", "name"),
            ("warranty", "name"),
            ("warranty/terms.md", "path"),
        ]
    );
    let hit = &f.hits[0];
    assert_eq!(hit.kind, EntryKind::Text);
    assert!(!hit.file_id.is_empty());
    assert!(hit.changed_at.is_some());
    assert!(f.hits[2].file_id.is_empty() && f.hits[2].changed_at.is_none());
    let g = s.graph(&junior, "", 0, None).unwrap();
    for h in &f.hits {
        let n = g.nodes.iter().find(|n| n.path == h.path).unwrap();
        assert_eq!(
            (&h.colour, &h.seen_from),
            (&n.colour, &n.seen_from),
            "{}",
            h.path
        );
    }

    // Ties among names break by `changed.at`, newest first.
    s.edit(
        &root,
        &r.id,
        vec![put("services/warranty-claims.md", "claims v2")],
        "k-touch",
        None,
        None,
    )
    .unwrap();
    let f = s.find(&junior, "WAR", "", 20, true).unwrap();
    assert_eq!(
        found(&f)[..2],
        [
            ("services/warranty-claims.md", "name"),
            ("Warranty.md", "name")
        ]
    );
    assert!(f.hits[0].changed_at > f.hits[1].changed_at);

    // Every token must prefix a token; one that only the path holds makes a path hit.
    assert_eq!(
        found(&s.find(&junior, "serv war", "", 20, true).unwrap()),
        vec![("services/warranty-claims.md", "path")]
    );
    assert_eq!(
        found(&s.find(&junior, "claims warr", "", 20, true).unwrap()),
        vec![("services/warranty-claims.md", "name")]
    );
    assert_eq!(
        found(&s.find(&junior, "\u{e9}CH", "", 20, true).unwrap()),
        vec![("notes/\u{c9}ch\u{e9}ancier.md", "name")]
    );
    assert!(s
        .find(&junior, "arranty", "", 20, false)
        .unwrap()
        .hits
        .is_empty());

    // Scope, limit.
    assert_eq!(
        found(&s.find(&junior, "war", "services", 20, true).unwrap()),
        vec![("services/warranty-claims.md", "name")]
    );
    // Every candidate is stamped before the cut, so a tie wider than `limit` keeps the
    // most recently changed name, not the first by path.
    assert_eq!(
        found(&s.find(&junior, "war", "", 1, true).unwrap()),
        vec![("services/warranty-claims.md", "name")]
    );

    // A hidden or absent scope answers not_found alike.
    for scope in ["senior-only", "senior-only/warranty-secret.md", "absent"] {
        assert_eq!(
            code(s.find(&junior, "war", scope, 20, true)),
            "not_found",
            "{scope}"
        );
    }
    assert_eq!(
        found(
            &s.find(&who("senior-s"), "war", "senior-only", 20, true)
                .unwrap()
        ),
        vec![("senior-only/warranty-secret.md", "name")]
    );
}

#[test]
fn find_keeps_the_newest_name_hit_when_the_limit_cuts() {
    let (_fx, s, root) = ranked();
    let head = s.head(&root).unwrap();
    let r = s
        .edit(
            &root,
            &head,
            vec![
                put("plan-a.md", "a"),
                put("plan-b.md", "b"),
                put("plan-c.md", "c"),
            ],
            "k-plans",
            None,
            None,
        )
        .unwrap();
    let r = s
        .edit(
            &root,
            &r.id,
            vec![put("plan-c.md", "c2")],
            "k-c",
            None,
            None,
        )
        .unwrap();
    s.edit(
        &root,
        &r.id,
        vec![put("plan-b.md", "b2")],
        "k-b",
        None,
        None,
    )
    .unwrap();
    let junior = who("junior-j");
    // Three name hits for one prefix: the most recently changed wins the single slot.
    assert_eq!(
        found(&s.find(&junior, "plan", "", 1, true).unwrap()),
        vec![("plan-b.md", "name")]
    );
    assert_eq!(
        found(&s.find(&junior, "plan", "", 2, true).unwrap()),
        vec![("plan-b.md", "name"), ("plan-c.md", "name")]
    );
    let all = s.find(&junior, "plan", "", 3, true).unwrap();
    assert_eq!(
        found(&all),
        vec![
            ("plan-b.md", "name"),
            ("plan-c.md", "name"),
            ("plan-a.md", "name")
        ]
    );
    // Each stamp is the one `history` reports first for that file.
    for h in &all.hits {
        let newest = s.history(&junior, &h.path).unwrap()[0].at.clone();
        assert_eq!(h.changed_at, newest, "{}", h.path);
    }
}

#[test]
fn find_needs_no_stored_text() {
    let (fx, s, root) = ranked();
    let mut at = s.head(&root).unwrap();
    for i in 0..4 {
        at = s
            .edit(
                &root,
                &at,
                vec![put("log-book.md", &format!("entry {i}"))],
                &format!("k-log-{i}"),
                None,
                None,
            )
            .unwrap()
            .id;
    }
    let junior = who("junior-j");
    let before = s.find(&junior, "log", "", 20, true).unwrap();
    // Remove every stored text, head versions included: names, paths and stamps come
    // from revision metadata and manifests alone.
    for blob in std::fs::read_dir(fx.0.join("blobs")).unwrap() {
        std::fs::remove_file(blob.unwrap().path()).unwrap();
    }
    // The graph index too: titles fall back to basenames, never an error.
    std::fs::remove_dir_all(fx.0.join("graph-v1")).unwrap();
    let after = s.find(&junior, "log", "", 20, true).unwrap();
    assert_eq!(after, before);
    assert_eq!(found(&after), vec![("log-book.md", "name")]);
    assert_eq!(
        after.hits[0].changed_at.as_deref(),
        Some(
            s.history(&root, "log-book.md").unwrap()[0]
                .at
                .as_deref()
                .unwrap()
        )
    );
}

#[test]
fn find_stamps_with_the_newest_revision_readable_then() {
    let (_fx, s, root) = ranked();
    let head = s.head(&root).unwrap();
    let meta = |seen: &str| Change::SetMeta {
        path: "vault".into(),
        seen_from: Some(seen.into()),
        colour: None,
    };
    let open = s
        .edit(
            &root,
            &head,
            vec![
                Change::CreateFolder {
                    path: "vault".into(),
                },
                put("vault/memo-open.md", "v1"),
            ],
            "k-open",
            None,
            None,
        )
        .unwrap();
    let hidden = s
        .edit(&root, &open.id, vec![meta("senior")], "k-hide", None, None)
        .unwrap();
    let edited = s
        .edit(
            &root,
            &hidden.id,
            vec![
                put("vault/memo-open.md", "v2 while hidden"),
                put("vault/memo-born-hidden.md", "only ever hidden"),
            ],
            "k-edit",
            None,
            None,
        )
        .unwrap();
    s.edit(
        &root,
        &edited.id,
        vec![meta("junior")],
        "k-reopen",
        None,
        None,
    )
    .unwrap();

    let junior = who("junior-j");
    let senior = who("senior-s");
    let stamp = |who: &TrustedIdentity, path: &str| {
        s.find(who, "memo", "", 20, true)
            .unwrap()
            .hits
            .into_iter()
            .find(|h| h.path == path)
            .unwrap_or_else(|| panic!("{path}"))
            .changed_at
    };
    // The junior reads both files now, but the edit was made while `vault` was hidden
    // from them: the stamp is the newest revision they could read then.
    assert_eq!(stamp(&junior, "vault/memo-open.md"), open.at);
    assert_eq!(stamp(&junior, "vault/memo-born-hidden.md"), None);
    assert_eq!(stamp(&senior, "vault/memo-open.md"), edited.at);
    assert_eq!(stamp(&senior, "vault/memo-born-hidden.md"), edited.at);
    // The same answer `history` gives, revision for revision.
    for (who, path) in [
        (&junior, "vault/memo-open.md"),
        (&junior, "vault/memo-born-hidden.md"),
        (&senior, "vault/memo-open.md"),
    ] {
        let newest = s
            .history(who, path)
            .unwrap()
            .first()
            .and_then(|r| r.at.clone());
        assert_eq!(stamp(who, path), newest, "{path}");
    }
}

#[test]
fn find_bounds_query_and_limit() {
    let (_fx, s, _root) = ranked();
    let junior = who("junior-j");
    for q in ["", "   ", "--/."] {
        assert_eq!(
            code(s.find(&junior, q, "", 20, true)),
            "invalid_search",
            "{q:?}"
        );
    }
    assert_eq!(
        code(s.find(&junior, &"a".repeat(4097), "", 20, true)),
        "invalid_search"
    );
    for limit in [0, 51] {
        assert_eq!(
            code(s.find(&junior, "w", "", limit, true)),
            "invalid_request"
        );
    }
    assert!(s.find(&junior, "w", "", 50, true).is_ok());
    assert_eq!(code(s.find(&junior, "w", "../x", 20, true)), "invalid_path");
}

#[test]
fn find_never_shows_hidden_paths() {
    let (_fx, s, root) = ranked();
    let head = s.head(&root).unwrap();
    // A lineage-hidden file: readable by rank at its new name, but its identity once
    // lived under a hidden one.
    s.edit(
        &root,
        &head,
        vec![Change::Rename {
            path: "senior-only/secret.md".into(),
            to: "moved-secret.md".into(),
        }],
        "k-move",
        None,
        None,
    )
    .unwrap();
    let junior = who("junior-j");
    assert_eq!(
        found(&s.find(&junior, "w", "", 50, true).unwrap()),
        vec![("notes/W.md", "name")]
    );
    for q in ["senior", "pay", "secret", "only"] {
        let f = s.find(&junior, q, "", 50, true).unwrap();
        assert!(f.hits.is_empty(), "{q}: {:?}", found(&f));
    }
    let json = serde_json::to_string(&s.find(&junior, "md", "", 50, true).unwrap()).unwrap();
    assert!(
        !json.contains("senior-only") && !json.contains("secret"),
        "{json}"
    );

    let senior = who("senior-s");
    assert_eq!(
        found(&s.find(&senior, "w", "", 50, true).unwrap()),
        vec![("notes/W.md", "name"), ("senior-only/W.md", "name")]
    );
    assert_eq!(
        found(&s.find(&senior, "secret", "", 50, true).unwrap()),
        vec![("moved-secret.md", "name")]
    );
}

#[test]
fn find_v1_refused() {
    let (_fx, s, _root) = ranked();
    let junior = who("junior-j");
    let find = || Operation::Find {
        query: "w".into(),
        scope: String::new(),
        limit: 20,
        typos: true,
    };
    let request = |version| Request {
        version,
        idempotency_key: None,
        operation: find(),
    };
    let r = s.execute(&junior, request(1));
    assert_eq!(r.error.unwrap().code, "unsupported_version");
    let r = s.execute(&junior, request(2));
    assert!(r.error.is_none(), "{:?}", r.error);
    let v = serde_json::to_value(&r).unwrap();
    assert_eq!(v["result"]["hits"][0]["path"], "notes/W.md");
    assert_eq!(v["result"]["hits"][0]["matched"], "name");
    // `scope`, `limit` and `typos` default.
    let parsed: Request =
        serde_json::from_str(r#"{"version":2,"operation":{"type":"find","query":"w"}}"#).unwrap();
    assert!(matches!(
        parsed.operation,
        Operation::Find { ref scope, limit: 20, typos: true, .. } if scope.is_empty()
    ));
}
