//! Reference lifecycle: FileManager mutations, extension classification
//! and persisted widget reload. The fixture restarted the same isolated vault.
use mnemosyne_core::CONFIG_DIR;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use mnemosyne::app;
use mnemosyne_core::{graph_refs, Vault};
use mnemosyne_engine::Engine;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
};
struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn data() -> Value {
    serde_json::from_str(include_str!("fixtures/file_property.json")).unwrap()
}
fn fixture(before: &Value, plugins: &Value) -> Fixture {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let dir = Fixture(std::env::temp_dir().join(format!(
        "mn-files-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )));
    std::fs::create_dir_all(dir.0.join(CONFIG_DIR)).unwrap();
    for (path, bytes) in before["tree"]["files"].as_object().unwrap() {
        let p = dir.0.join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, STANDARD.decode(bytes.as_str().unwrap()).unwrap()).unwrap();
    }
    for path in before["tree"]["directories"].as_array().unwrap() {
        std::fs::create_dir_all(dir.0.join(path.as_str().unwrap())).unwrap();
    }
    std::fs::write(
        dir.0.join(CONFIG_DIR).join("app.json"),
        r#"{"alwaysUpdateLinks":true,"newLinkFormat":"shortest"}"#,
    )
    .unwrap();
    std::fs::write(
        dir.0.join(CONFIG_DIR).join("core-plugins.json"),
        serde_json::to_vec(plugins).unwrap(),
    )
    .unwrap();
    if let Some(bytes) = before["types_bytes_base64"].as_str() {
        std::fs::write(
            dir.0.join(CONFIG_DIR).join("types.json"),
            STANDARD.decode(bytes).unwrap(),
        )
        .unwrap();
    }
    dir
}
fn tree(root: &Path) -> Value {
    fn walk(
        root: &Path,
        path: &Path,
        files: &mut BTreeMap<String, String>,
        dirs: &mut Vec<String>,
    ) {
        for e in std::fs::read_dir(path).unwrap().flatten() {
            let p = e.path();
            let name = p
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if name == CONFIG_DIR || name == ".mnemosyne" {
                continue;
            }
            if p.is_dir() {
                dirs.push(name);
                walk(root, &p, files, dirs)
            } else {
                files.insert(name, STANDARD.encode(std::fs::read(p).unwrap()));
            }
        }
    }
    let mut files = BTreeMap::new();
    let mut dirs = vec![];
    walk(root, root, &mut files, &mut dirs);
    dirs.sort();
    json!({"files":files,"directories":dirs,"vault_exists":root.is_dir()})
}
fn invoke(engine: &mut Engine, args: &Value) -> String {
    app::execute(
        engine,
        &serde_json::from_value::<Vec<String>>(args.clone()).unwrap(),
    )
}
fn maps(engine: &Engine) -> Value {
    let mut resolved = BTreeMap::<String, BTreeMap<String, usize>>::new();
    let mut unresolved = BTreeMap::<String, BTreeMap<String, usize>>::new();
    for source in engine.resolved_link_paths() {
        let cache = engine.file_cache(source).unwrap();
        let targets = resolved.entry(source.into()).or_default();
        let missing = unresolved.entry(source.into()).or_default();
        for r in graph_refs::references(cache) {
            let target = graph_refs::link_path(r);
            if let Some(dest) = engine.resolve_cached_link(source, target) {
                *targets.entry(dest).or_default() += 1
            } else {
                let name = mnemosyne_core::strip_md_extension(target).to_owned();
                *missing.entry(name).or_default() += 1
            }
        }
    }
    json!({"resolved":resolved,"unresolved":unresolved})
}
fn assert_phase(engine: &mut Engine, root: &Path, phase: &Value, label: &str) {
    assert_eq!(
        tree(root),
        phase["tree"],
        "{label} exact saved bytes/directories"
    );
    let mut markdown: Vec<_> = engine.vault.notes.keys().cloned().collect();
    markdown.sort();
    assert_eq!(
        json!(markdown),
        phase["metadata"]["markdown"],
        "{label} note classification"
    );
    let actual = maps(engine);
    assert_eq!(
        actual["resolved"], phase["metadata"]["resolved"],
        "{label} filename graph"
    );
    assert_eq!(
        actual["unresolved"], phase["metadata"]["unresolved"],
        "{label} unresolved aliases"
    );
    for (path, expected) in phase["metadata"]["cache"].as_object().unwrap() {
        let actual = engine
            .file_cache(path)
            .map(|cache| serde_json::to_value(cache).unwrap())
            .unwrap_or(Value::Null);
        let expected = expected.clone();
        assert_eq!(actual, expected, "{label} getFileCache({path})");
    }
    for c in phase["outputs"].as_array().unwrap() {
        assert_eq!(c["output"]["exit_code"], 0);
        assert_eq!(c["output"]["stderr"], "");
        let expected = STANDARD
            .decode(c["output"]["stdout"].as_str().unwrap())
            .unwrap();
        assert_eq!(
            invoke(engine, &c["args"]).as_bytes(),
            expected,
            "{label} {:?}",
            c["args"]
        );
    }
    let widget = engine
        .assigned_property_widget_units(&"priority".encode_utf16().collect::<Vec<_>>())
        .unwrap()
        .map(|v| v.to_value().unwrap())
        .unwrap_or(Value::Null);
    let info = serde_json::to_value(engine.property_info("priority").unwrap()).unwrap();
    assert_eq!(
        json!({"info":info,"assigned":widget}),
        phase["metadata"]["priority"],
        "{label} assigned and inferred type"
    );
    let types = root.join(CONFIG_DIR).join("types.json");
    let bytes = types
        .exists()
        .then(|| STANDARD.encode(std::fs::read(types).unwrap()));
    assert_eq!(
        json!(bytes),
        phase["types_bytes_base64"],
        "{label} persisted type bytes"
    );
    // Cache/source chronology is a later engine contract; only map contents are
    // compared here. Core graph/backlinks independently use the same filenames.
    let graph = engine.vault.graph();
    for (source, targets) in phase["metadata"]["resolved"].as_object().unwrap() {
        for (dest, count) in targets.as_object().unwrap() {
            assert!(
                graph.edges.iter().any(|edge| edge.from == *source
                    && edge.to == *dest
                    && edge.weight == count.as_u64().unwrap() as usize),
                "{label} core graph {source}->{dest}"
            );
            assert!(
                engine.vault.backlinks_to(dest).iter().any(|s| s == source),
                "{label} core backlink {source}->{dest}"
            );
        }
    }
}
#[test]
fn file_manager_alias_graph_extensions_and_property_types_match_actual_restart() {
    let d = data();
    let fixture = fixture(&d["before"], &d["initial_core_plugin_config"]);
    let mut engine = Engine::open(&fixture.0).unwrap();
    let order: Vec<String> =
        serde_json::from_value(d["before"]["metadata"]["file_cache_order"].clone()).unwrap();
    engine.import_cache_order(&order).unwrap();
    assert_phase(&mut engine, &fixture.0, &d["before"], "before");
    for (index, c) in d["graph_steps"]
        .as_array()
        .unwrap()
        .iter()
        .chain(d["extension_steps"].as_array().unwrap())
        .chain(std::iter::once(&d["property_step"]))
        .enumerate()
    {
        if index == d["graph_steps"].as_array().unwrap().len() {
            // Native enables Canvas at this exact point and persists its flag.
            // All other plugins remain disabled, including outgoing-link.
            std::fs::write(
                fixture.0.join(CONFIG_DIR).join("core-plugins.json"),
                serde_json::to_vec(&d["after_canvas_enable_core_plugin_config"]).unwrap(),
            )
            .unwrap();
            engine.reload_core_plugin_settings().unwrap();
        }
        let expected = STANDARD
            .decode(c["output"]["stdout"].as_str().unwrap())
            .unwrap();
        assert_eq!(
            invoke(&mut engine, &c["args"]).as_bytes(),
            expected,
            "step{index} {:?}",
            c["args"]
        );
        assert_phase(
            &mut engine,
            &fixture.0,
            &c["after"],
            &format!("step{index}"),
        );
    }
    assert_phase(&mut engine, &fixture.0, &d["before_restart"], "warm");
    drop(engine);
    let mut reopened = Engine::open(&fixture.0).unwrap();
    assert_phase(
        &mut reopened,
        &fixture.0,
        &d["after_restart"],
        "same saved vault reopened",
    );
    let again = Vault::open(&fixture.0).unwrap();
    assert!(again.get("Board.canvas").is_none());
    assert!(again.has_file("Board.canvas"));
    assert!(again.get("NewImage.png").is_none());
    assert!(again.get("Upper.md").is_some());
}
