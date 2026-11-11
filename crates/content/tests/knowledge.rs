use mnemosyne_content::{
    auth::{ActorKind, Grant, Permission, Policy, TrustedIdentity},
    knowledge::{MAX_FRONTMATTER_BYTES, MAX_MARKDOWN_BYTES},
    proposals::ProposalListRequest,
    Change, ProposalState, Service,
};
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
fn actor(name: &str) -> TrustedIdentity {
    TrustedIdentity {
        tenant_id: "t".into(),
        actor_id: name.into(),
        kind: if name == "agent" {
            ActorKind::Agent
        } else {
            ActorKind::Human
        },
        on_behalf_of: None,
    }
}
fn policy() -> Policy {
    let mut grants = vec![];
    for (actor, scope) in [("human", ""), ("agent", ""), ("reader", "public")] {
        for permission in [
            Permission::Read,
            Permission::Edit,
            Permission::Propose,
            Permission::Approve,
            Permission::Admin,
        ] {
            grants.push(Grant {
                tenant_id: "t".into(),
                actor_id: actor.into(),
                permission,
                scope: scope.into(),
            });
        }
    }
    Policy {
        grants,
        tenants: Default::default(),
    }
}
struct Fixture {
    root: PathBuf,
    s: Service,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "mnemosyne-knowledge-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let s = Service::open(&root).unwrap();
        s.bootstrap_policy(policy()).unwrap();
        Self { root, s }
    }
    fn put(&self, path: &str, content: &str, key: &str) -> String {
        self.s
            .edit(
                &actor("human"),
                &self.s.head(&actor("human")).unwrap(),
                vec![put(path, content)],
                key,
                None,
                None,
            )
            .unwrap()
            .id
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
fn put(path: &str, content: &str) -> Change {
    Change::Put {
        path: path.into(),
        content: content.into(),
    }
}

#[test]
fn markdown_properties_duplicate_sections_code_and_revision_sources() {
    let f = Fixture::new();
    let body = "---\ntitle: Café\ntags: [one, two]\nmeta: {active: true}\n---\nintro\n# Same *title*\nα\n## Child\nchild\n```md\n# hidden\n```\n# Same *title*\nlast\n\nSetext\n======\nend\n";
    let rev = f.put("public/a.md", body, "seed");
    let p =
        f.s.properties(&actor("reader"), "public/a.md", None)
            .unwrap();
    assert_eq!(p.properties["title"], "Café");
    assert_eq!(p.properties["meta"]["active"], true);
    assert_eq!(p.frontmatter.unwrap().end_line, 5);
    let outline = f.s.outline(&actor("reader"), "public/a.md", None).unwrap();
    assert_eq!(outline.headings.len(), 4);
    assert_eq!(
        outline
            .headings
            .iter()
            .map(|h| h.title.as_str())
            .collect::<Vec<_>>(),
        ["Same title", "Child", "Same title", "Setext"]
    );
    assert_eq!(outline.headings[0].section_id, "s1");
    assert_eq!(outline.headings[2].section_id, "s3");
    assert_eq!(outline.headings[0].source.start_line, 7);
    let first =
        f.s.read_section(&actor("reader"), "public/a.md", None, Some("s1"), None)
            .unwrap();
    assert!(first.content.contains("## Child"));
    assert!(!first.content.contains("last"));
    assert_eq!(
        &body[first.source.start_byte..first.source.end_byte],
        first.content
    );
    assert_eq!(
        f.s.read_section(&actor("reader"), "public/a.md", None, Some("s0"), None)
            .unwrap()
            .content,
        "intro\n"
    );
    f.put("public/a.md", "# Changed\nnew", "update");
    let old =
        f.s.read_section(
            &actor("reader"),
            "public/a.md",
            Some(&rev),
            Some("s3"),
            None,
        )
        .unwrap();
    assert_eq!(old.source.revision_id, rev);
    assert!(old.content.contains("last"));
    assert_eq!(
        f.s.read_section(&actor("reader"), "public/a.md", None, Some("s3"), None)
            .unwrap_err()
            .code,
        "section_not_found"
    );
}

#[test]
fn knowledge_uses_current_read_and_lineage_authorization_for_history() {
    let f = Fixture::new();
    let rev = f.put("public/a.md", "---\nsecret: value\n---\n# Private", "seed");
    f.s.edit(
        &actor("human"),
        &rev,
        vec![Change::Rename {
            path: "public/a.md".into(),
            to: "private/a.md".into(),
        }],
        "move",
        None,
        None,
    )
    .unwrap();
    for error in [
        f.s.properties(&actor("reader"), "public/a.md", Some(&rev))
            .unwrap_err(),
        f.s.outline(&actor("reader"), "public/a.md", Some(&rev))
            .unwrap_err(),
        f.s.read_section(
            &actor("reader"),
            "public/a.md",
            Some(&rev),
            Some("s1"),
            None,
        )
        .unwrap_err(),
    ] {
        assert_eq!(error.code, "not_found");
    }
    assert_eq!(
        f.s.properties(&actor("reader"), "private/a.md", None)
            .unwrap_err()
            .code,
        "forbidden"
    );
    let other = TrustedIdentity {
        tenant_id: "other".into(),
        ..actor("human")
    };
    assert_eq!(
        f.s.outline(&other, "public/a.md", Some(&rev))
            .unwrap_err()
            .code,
        "forbidden"
    );
}

#[test]
fn malformed_and_bounded_frontmatter_and_nonmarkdown_text() {
    let f = Fixture::new();
    for (i, bad) in [
        "---\nmissing: close",
        "---\n- list\n---\n",
        "---\na: 1\na: 2\n---\n",
        "---\na: .nan\n---\n",
        "---\nx: [\n---\n",
    ]
    .iter()
    .enumerate()
    {
        let path = format!("public/bad{i}");
        f.put(&path, bad, &format!("bad{i}"));
        assert_eq!(
            f.s.properties(&actor("reader"), &path, None)
                .unwrap_err()
                .code,
            "invalid_frontmatter"
        );
    }
    f.put("public/big", &"x".repeat(MAX_MARKDOWN_BYTES + 1), "big");
    assert_eq!(
        f.s.outline(&actor("reader"), "public/big", None)
            .unwrap_err()
            .code,
        "knowledge_limit"
    );
    f.put(
        "public/fm",
        &format!("---\na: {}\n---\n", "x".repeat(MAX_FRONTMATTER_BYTES + 1)),
        "fm",
    );
    assert_eq!(
        f.s.properties(&actor("reader"), "public/fm", None)
            .unwrap_err()
            .code,
        "knowledge_limit"
    );
    f.put("public/plain.txt", "\u{feff}# Valid\nbody", "plain");
    assert!(f
        .s
        .properties(&actor("reader"), "public/plain.txt", None)
        .unwrap()
        .frontmatter
        .is_none());
    assert_eq!(
        f.s.outline(&actor("reader"), "public/plain.txt", None)
            .unwrap()
            .headings
            .len(),
        1
    );
    let deep = format!("---\nx: {}0{}\n---\n", "[".repeat(40), "]".repeat(40));
    f.put("public/deep", &deep, "deep");
    assert_eq!(
        f.s.properties(&actor("reader"), "public/deep", None)
            .unwrap_err()
            .code,
        "invalid_frontmatter"
    );
}

#[test]
fn discovery_filters_before_pagination_and_matches_review_across_restart() {
    let f = Fixture::new();
    let visible =
        f.s.propose(
            &actor("agent"),
            "r0",
            vec![put("public/a", "visible")],
            "visible",
            None,
            None,
        )
        .unwrap();
    let hidden =
        f.s.propose(
            &actor("agent"),
            "r0",
            vec![put("private/a", "SECRET")],
            "hidden",
            None,
            None,
        )
        .unwrap();
    let mixed =
        f.s.propose(
            &actor("agent"),
            "r0",
            vec![put("public/a", "one"), put("private/b", "SECRET")],
            "mixed",
            None,
            None,
        )
        .unwrap();
    f.s.reject(
        &actor("human"),
        &visible.id,
        &visible.digest,
        "reject",
        None,
    )
    .unwrap();
    let reopened = Service::open(&f.root).unwrap();
    let list = reopened
        .list_proposals(
            &actor("reader"),
            &ProposalListRequest {
                limit: 1,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(list.proposals.len(), 1);
    assert_eq!(list.proposals[0].id, visible.id);
    assert!(list.next_offset.is_none());
    let json = serde_json::to_string(&list).unwrap();
    assert!(!json.contains("SECRET"));
    assert!(!json.contains("changes"));
    assert!(!json.contains("diff"));
    // Summaries now name the readable paths of visible proposals, never hidden ones.
    assert!(!json.contains("private/a"));
    assert!(!json.contains("private/b"));
    assert_eq!(list.proposals[0].paths, ["public/a"]);
    assert_eq!(list.proposals[0].path_count, 1);
    assert!(!json.contains(&hidden.id));
    assert!(!json.contains(&mixed.id));
    let pending = reopened
        .list_proposals(
            &actor("reader"),
            &ProposalListRequest {
                states: vec![ProposalState::Pending],
                ..Default::default()
            },
        )
        .unwrap();
    assert!(pending.proposals.is_empty());
    assert!(pending.next_offset.is_none());
    let first = reopened
        .list_proposals(
            &actor("human"),
            &ProposalListRequest {
                limit: 1,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(first.next_offset, Some(1));
    let second = reopened
        .list_proposals(
            &actor("human"),
            &ProposalListRequest {
                limit: 2,
                offset: 1,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(second.proposals.len(), 2);
    assert!(second.next_offset.is_none());
    assert!(second
        .proposals
        .iter()
        .all(|p| p.id != first.proposals[0].id));
    let delegated = TrustedIdentity {
        kind: ActorKind::Agent,
        on_behalf_of: Some("reader".into()),
        ..actor("agent")
    };
    assert_eq!(
        reopened
            .list_proposals(&delegated, &Default::default())
            .unwrap()
            .proposals
            .len(),
        1
    );
    assert_eq!(
        reopened
            .list_proposals(&actor("nobody"), &Default::default())
            .unwrap_err()
            .code,
        "forbidden"
    );
    assert_eq!(
        reopened
            .list_proposals(
                &actor("human"),
                &ProposalListRequest {
                    limit: 101,
                    ..Default::default()
                }
            )
            .unwrap_err()
            .code,
        "invalid_request"
    );
}

#[test]
fn discovery_rechecks_current_lineage_and_read_policy() {
    let f = Fixture::new();
    let rev = f.put("public/a.md", "# Before", "seed");
    let proposal =
        f.s.propose(
            &actor("agent"),
            &rev,
            vec![put("public/a.md", "# Proposed")],
            "proposal",
            None,
            None,
        )
        .unwrap();
    assert_eq!(
        f.s.list_proposals(&actor("reader"), &Default::default())
            .unwrap()
            .proposals
            .len(),
        1
    );
    f.s.edit(
        &actor("human"),
        &rev,
        vec![Change::Rename {
            path: "public/a.md".into(),
            to: "private/a.md".into(),
        }],
        "move",
        None,
        None,
    )
    .unwrap();
    assert_eq!(
        f.s.review(&actor("reader"), &proposal.id).unwrap_err().code,
        "not_found"
    );
    assert!(f
        .s
        .list_proposals(&actor("reader"), &Default::default())
        .unwrap()
        .proposals
        .is_empty());
    let mut revoked = policy();
    revoked.grants.retain(|g| g.actor_id != "reader");
    f.s.set_policy(&actor("human"), revoked).unwrap();
    assert_eq!(
        f.s.list_proposals(&actor("reader"), &Default::default())
            .unwrap_err()
            .code,
        "forbidden"
    );
}

#[test]
fn yaml_alias_expansion_and_heading_count_are_bounded() {
    let f = Fixture::new();
    let mut yaml = String::from("---\na: &a [0,0,0,0,0,0,0,0,0,0]\n");
    for (name, prev) in [('b', 'a'), ('c', 'b'), ('d', 'c'), ('e', 'd')] {
        yaml.push_str(&format!(
            "{name}: &{name} [{}]\n",
            vec![format!("*{prev}"); 10].join(",")
        ));
    }
    yaml.push_str("---\n# Bounded\n");
    f.put("public/aliases", &yaml, "aliases");
    assert_eq!(
        f.s.properties(&actor("reader"), "public/aliases", None)
            .unwrap_err()
            .code,
        "invalid_frontmatter"
    );
    let strings = format!(
        "---\na: &a {}\nb: [*a, *a, *a, *a]\n---\n",
        "x".repeat(20_000)
    );
    f.put("public/strings", &strings, "strings");
    assert_eq!(
        f.s.properties(&actor("reader"), "public/strings", None)
            .unwrap_err()
            .code,
        "invalid_frontmatter"
    );
    f.put("public/headings", &"# h\n".repeat(4097), "headings");
    assert_eq!(
        f.s.outline(&actor("reader"), "public/headings", None)
            .unwrap_err()
            .code,
        "knowledge_limit"
    );
}
