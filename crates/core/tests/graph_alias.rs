use mnemosyne_core::Vault;

#[test]
fn filename_graph_resolution_does_not_fall_back_to_frontmatter_aliases() {
    // AliasOnly and Target are the same authored notes used by the isolated
    // 1.14.4 graph command fixture: Nickname remains unresolved.
    let root = std::env::temp_dir().join(format!("mn-graph-alias-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    struct Fixture(std::path::PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let fixture = Fixture(root);
    std::fs::write(
        fixture.0.join("Target.md"),
        "---\naliases: [Nickname, shared]\n---\n# Heading\nTarget.\n",
    )
    .unwrap();
    std::fs::write(fixture.0.join("AliasOnly.md"), "[[Nickname]]\n").unwrap();
    let vault = Vault::open(&fixture.0).unwrap();
    let graph = vault.graph();
    assert!(graph.edges.is_empty());
    assert_eq!(graph.unresolved.get("Nickname"), Some(&1));
    assert_eq!(vault.backlinks_to("Target.md"), &[] as &[String]);
    assert!(vault.deadends().contains(&"AliasOnly.md".into()));
}
