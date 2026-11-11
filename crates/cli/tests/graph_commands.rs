use base64::{engine::general_purpose::STANDARD, Engine as _};
use mnemosyne::{graph_commands, registry::frame_output};
use mnemosyne_engine::{Engine, FileTarget, OpenOptions};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn fixture(phase: &Value) -> Fixture {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "mn-graph-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&path).unwrap();
    for (file, bytes) in phase["files_base64"].as_object().unwrap() {
        let destination = path.join(file);
        std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
        std::fs::write(
            destination,
            STANDARD.decode(bytes.as_str().unwrap()).unwrap(),
        )
        .unwrap();
    }
    Fixture(path)
}
#[test]
fn graph_handlers_match_109_native_pipe_outputs_over_saved_cache() {
    let manifest: Value =
        serde_json::from_str(include_str!("fixtures/graph_commands.json")).unwrap();
    let mut failures = Vec::new();
    for (phase_name, phase) in manifest["phases"].as_object().unwrap() {
        let fixture = fixture(phase);
        let mut engine = Engine::open(&fixture.0).unwrap();
        let cache_order: Vec<String> =
            serde_json::from_value(phase["file_cache_order"].clone()).unwrap();
        engine.import_cache_order(&cache_order).unwrap();
        engine
            .import_collation_locale(manifest["collation_locale"].as_str().unwrap())
            .unwrap();
        if let Some(active) = phase["active"].as_str() {
            engine
                .open_file(FileTarget::path(active), OpenOptions::default())
                .unwrap();
        }
        let sources: Vec<String> = serde_json::from_value(phase["resolved_order"].clone()).unwrap();
        assert_eq!(
            engine.resolved_link_paths(),
            sources.iter().map(String::as_str).collect::<Vec<_>>()
        );
        for case in manifest["cases"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["phase"] == phase_name.as_str())
        {
            let argv: Vec<String> = serde_json::from_value(case["args"].clone()).unwrap();
            let (command, mut params) = mnemosyne::args::parse_argv(&argv);
            let command = mnemosyne::args::prepare_command(&command, &mut params);
            let result = graph_commands::execute(&engine, &params, &command);
            let actual = match result {
                Ok(text) => frame_output(&text),
                Err(error) => frame_output(&format!("Error: {error}")),
            };
            let expected = case["expected"]["stdout"].as_str().unwrap();
            if actual != expected {
                failures.push(format!(
                    "{} {:?}\nexpected {expected:?}\nactual {actual:?}",
                    case["id"], argv
                ));
            }
            assert_eq!(case["expected"]["stderr"], "");
            assert_eq!(case["expected"]["exit_code"], 0);
        }
    }
    assert!(
        failures.is_empty(),
        "{} differences:\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

#[test]
fn core_graph_indexes_counts_and_deadends_match_native_cache_maps() {
    let manifest: Value =
        serde_json::from_str(include_str!("fixtures/graph_commands.json")).unwrap();
    for (name, phase) in manifest["phases"].as_object().unwrap() {
        let fixture = fixture(phase);
        let vault = mnemosyne_core::Vault::open(&fixture.0).unwrap();
        let graph = vault.graph();
        let mut resolved = vault
            .notes
            .keys()
            .map(|path| (path.clone(), BTreeMap::new()))
            .collect::<BTreeMap<String, BTreeMap<String, usize>>>();
        for edge in &graph.edges {
            resolved
                .get_mut(&edge.from)
                .unwrap()
                .insert(edge.to.clone(), edge.weight);
        }
        assert_eq!(
            serde_json::to_value(&resolved).unwrap(),
            phase["resolved"],
            "{name} resolved"
        );
        let mut unresolved = BTreeMap::<String, usize>::new();
        let mut unresolved_sources = BTreeMap::<String, Vec<String>>::new();
        for (source, links) in phase["unresolved"].as_object().unwrap() {
            for (link, count) in links.as_object().unwrap() {
                *unresolved.entry(link.clone()).or_default() += count.as_u64().unwrap() as usize;
                unresolved_sources
                    .entry(link.clone())
                    .or_default()
                    .push(source.clone());
            }
        }
        for sources in unresolved_sources.values_mut() {
            sources.sort();
        }
        assert_eq!(graph.unresolved, unresolved, "{name} unresolved");
        // Vault's public graph stores sorted sources; the retained Engine/CLI
        // preserves the separately captured metadata-cache insertion order.
        assert_eq!(
            graph.unresolved_sources, unresolved_sources,
            "{name} sources"
        );
        let incoming = resolved
            .values()
            .flat_map(|links| links.keys())
            .collect::<BTreeSet<_>>();
        let orphans = vault
            .files()
            .iter()
            .filter(|file| !incoming.contains(file))
            .cloned()
            .collect::<Vec<_>>();
        let deadends = vault
            .files()
            .iter()
            .filter(|file| resolved.get(*file).is_none_or(BTreeMap::is_empty))
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(vault.orphans(), orphans, "{name} orphans");
        assert_eq!(vault.deadends(), deadends, "{name} deadends");
        for file in vault.files() {
            let counts = resolved
                .iter()
                .filter_map(|(source, links)| links.get(file).map(|count| (source.clone(), *count)))
                .collect::<Vec<_>>();
            assert_eq!(vault.backlink_counts(file), counts, "{name} {file} counts");
            assert_eq!(
                vault.backlinks_to(file),
                counts
                    .iter()
                    .map(|(source, _)| source.clone())
                    .collect::<Vec<_>>(),
                "{name} {file} backlinks"
            );
        }
    }
}
