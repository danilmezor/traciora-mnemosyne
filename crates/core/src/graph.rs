//! The link graph: nodes, weighted edges, orphans, and unresolved links
//! (the data model behind a graph view).

use std::collections::BTreeMap;

use crate::vault::Vault;

/// A directed, weighted link between two notes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct GraphEdge {
    pub from: String,
    pub to: String,
    /// Number of link instances from `from` to `to`.
    pub weight: usize,
}

/// The computed link graph of a vault.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct Graph {
    /// Note paths present in the graph.
    pub nodes: Vec<String>,
    /// Weighted directed edges.
    pub edges: Vec<GraphEdge>,
    /// Note path -> total degree (link instances in and out).
    pub degrees: BTreeMap<String, usize>,
    /// Note paths with degree zero.
    pub orphans: Vec<String>,
    /// Unresolved link targets -> number of occurrences.
    pub unresolved: BTreeMap<String, usize>,
    /// Unresolved link targets -> source note paths (deduplicated).
    pub unresolved_sources: BTreeMap<String, Vec<String>>,
}

impl Vault {
    /// Computes the link graph over all notes.
    pub fn graph(&self) -> Graph {
        let mut edge_weights: BTreeMap<(String, String), usize> = BTreeMap::new();
        let mut unresolved: BTreeMap<String, usize> = BTreeMap::new();
        let mut unresolved_sources: BTreeMap<String, Vec<String>> = BTreeMap::new();

        for (source, note) in &self.notes {
            for target in crate::graph_refs::link_targets(note) {
                match self.resolve_path(note, &target) {
                    Some(dest) => {
                        *edge_weights.entry((source.clone(), dest)).or_default() += 1;
                    }
                    None => {
                        let key = crate::path::strip_md_extension(&target);
                        *unresolved.entry(key.clone()).or_default() += 1;
                        let src = unresolved_sources.entry(key).or_default();
                        if !src.contains(source) {
                            src.push(source.clone());
                        }
                    }
                }
            }
        }

        let mut degrees: BTreeMap<String, usize> = BTreeMap::new();
        for ((from, to), w) in &edge_weights {
            *degrees.entry(from.clone()).or_default() += w;
            *degrees.entry(to.clone()).or_default() += w;
        }

        let orphans = self
            .notes
            .keys()
            .filter(|k| !degrees.contains_key(*k))
            .cloned()
            .collect();

        let edges = edge_weights
            .into_iter()
            .map(|((from, to), weight)| GraphEdge { from, to, weight })
            .collect();

        Graph {
            nodes: self.notes.keys().cloned().collect(),
            edges,
            degrees,
            orphans,
            unresolved,
            unresolved_sources,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new() -> Self {
            let mut p = std::env::temp_dir();
            p.push(format!("mnemosyne-graph-{}", std::process::id()));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn builds_graph() {
        let t = TempDir::new();
        fs::write(t.0.join("A.md"), "[[B]] [[B]] [[Missing]]\n").unwrap();
        fs::write(t.0.join("B.md"), "[[A]]\n").unwrap();
        fs::write(t.0.join("Orphan.md"), "no links here\n").unwrap();

        let vault = Vault::open(&t.0).unwrap();
        let graph = vault.graph();

        assert_eq!(graph.nodes.len(), 3);
        assert_eq!(graph.orphans, vec!["Orphan.md"]);

        let ab = graph
            .edges
            .iter()
            .find(|e| e.from == "A.md" && e.to == "B.md")
            .unwrap();
        assert_eq!(ab.weight, 2);
        assert_eq!(graph.degrees["B.md"], 3); // A->B twice + B->A once

        assert_eq!(graph.unresolved.get("Missing"), Some(&1));
    }
}
