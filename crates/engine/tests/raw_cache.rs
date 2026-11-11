use mnemosyne_core::Vault;
use mnemosyne_engine::Engine;
use serde_json::Value;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "mnemosyne-rawcache-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(path.join("vault/Sub")).unwrap();
        std::fs::create_dir(path.join("sibling")).unwrap();
        Self(path)
    }
    fn vault(&self) -> PathBuf {
        self.0.join("vault")
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn canonical_relative(raw: &str) -> String {
    let mut absolute = PathBuf::from("/owned/vault");
    for part in Path::new(raw).components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                absolute.pop();
            }
            part => absolute.push(part.as_os_str()),
        }
    }
    if let Ok(path) = absolute.strip_prefix("/owned/vault") {
        path.to_string_lossy().into_owned()
    } else {
        format!(
            "../{}",
            absolute.strip_prefix("/owned").unwrap().to_string_lossy()
        )
    }
}
fn string_array(value: &Value) -> Vec<&str> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect()
}
fn graph_maps(engine: &Engine) -> (Value, Value) {
    let mut resolved = serde_json::Map::new();
    let mut unresolved = serde_json::Map::new();
    for source in engine.resolved_link_paths() {
        let mut destinations = serde_json::Map::new();
        let mut missing = serde_json::Map::new();
        for link in mnemosyne_core::graph_refs::references(engine.file_cache(source).unwrap()) {
            let path = mnemosyne_core::graph_refs::link_path(link);
            let (map, key) = if let Some(destination) = engine.resolve_cached_link(source, path) {
                (&mut destinations, destination)
            } else {
                (
                    &mut missing,
                    if mnemosyne_core::extension(path) == "md" {
                        path[..path.len() - 3].to_owned()
                    } else {
                        path.to_owned()
                    },
                )
            };
            let count = map.get(&key).and_then(Value::as_u64).unwrap_or(0) + 1;
            map.insert(key, Value::from(count));
        }
        resolved.insert(source.into(), Value::Object(destinations));
        unresolved.insert(source.into(), Value::Object(missing));
    }
    (Value::Object(resolved), Value::Object(unresolved))
}

#[test]
fn actual_raw_alias_graph_re_resolution_and_duplicate_reference_write_cleanup() {
    let receipt: Value =
        serde_json::from_str(include_str!("fixtures/reference_external_graph.json")).unwrap();
    let fixture = Fixture::new();
    let root = fixture.vault();
    std::fs::write(root.join("ParityReady.md"), "Isolated parity oracle.\n").unwrap();
    let mut engine = Engine::open(&root).unwrap();
    for case in receipt["ghost_cases"].as_array().unwrap() {
        let id = case["id"].as_str().unwrap();
        let target = case["target"].as_str().unwrap();
        let source = case["source"].as_str().unwrap();
        let raw = case["raw"].as_str().unwrap();
        let canonical = case["canonical"].as_str().unwrap();
        let missing = case["missing"].as_str().unwrap();
        engine.vault.create(target, "target\n").unwrap();
        engine.file_created(target);
        let content = format!(
            "[[{}]] [[{missing}]]\n",
            target.strip_suffix(".md").unwrap()
        );
        engine.vault.create(source, &content).unwrap();
        engine.file_created(source);
        std::fs::rename(root.join(source), root.join(canonical)).unwrap();
        engine.vault = Vault::open(&root).unwrap();
        engine.file_renamed_with_raw(source, raw, canonical);
        engine.refresh_vault().unwrap();
        for (phase, state) in [("before", &case["before"]), ("after", &case["after"])] {
            if phase == "after" {
                match id {
                    "target-renamed" => {
                        let plan = engine.prepare_raw_link_update();
                        let new = format!("New-{target}");
                        let edits = engine.vault.rename_with_edits(target, &new).unwrap();
                        engine.file_renamed(target, &new);
                        for edit in edits {
                            engine.file_modified(&edit.path);
                        }
                        engine.file_modified(&new);
                        assert_eq!(
                            engine.complete_raw_link_update(plan, target, &new).unwrap(),
                            [canonical]
                        );
                    }
                    "target-deleted" => {
                        engine.vault.delete(target).unwrap();
                        engine.file_deleted(target);
                    }
                    "missing-created" => {
                        let path = format!("{missing}.md");
                        engine.vault.create(&path, "created\n").unwrap();
                        engine.file_created(&path);
                    }
                    _ => panic!("{id}"),
                }
            }
            assert_eq!(
                engine.cached_file_paths(),
                string_array(&state["fileCacheKeys"]),
                "{id} {phase} cache chronology"
            );
            let (resolved, unresolved) = graph_maps(&engine);
            assert_eq!(resolved, state["resolved"], "{id} {phase} resolved values");
            assert_eq!(
                unresolved, state["unresolved"],
                "{id} {phase} unresolved values"
            );
            assert_eq!(
                serde_json::to_value(engine.file_cache(raw)).unwrap(),
                state["rawcache"],
                "{id} {phase} rawcache"
            );
        }
        assert_eq!(
            std::fs::read_to_string(root.join(canonical)).unwrap(),
            case["after"]["post_stop_saved_source"],
            "{id} post-stop owned filesystem bytes"
        );
    }
}
fn assert_state(engine: &Engine, state: &Value, label: &str) {
    assert_eq!(
        engine.cached_file_paths(),
        string_array(&state["file_cache_keys"]),
        "{label} cacheorder"
    );
    assert_eq!(
        engine.resolved_link_paths(),
        string_array(&state["resolved_keys"]),
        "{label} resolvedorder"
    );
    assert_eq!(
        engine.ordered_file_paths(),
        string_array(&state["files"]),
        "{label} VFSorder"
    );
}

#[test]
fn actual_three_raw_cache_metadata_identities_staleness_and_outside_vfs() {
    let receipt: Value =
        serde_json::from_str(include_str!("fixtures/reference_focus_ghost.json")).unwrap();
    let fixture = Fixture::new();
    let root = fixture.vault();
    std::fs::write(root.join("ParityReady.md"), "Isolated parity oracle.\n").unwrap();
    let mut engine = Engine::open(&root).unwrap();
    for row in receipt["ghost_cases"].as_array().unwrap() {
        let id = row["id"].as_str().unwrap();
        let from = row["source"].as_str().unwrap();
        let raw = row["raw_destination"].as_str().unwrap();
        std::fs::write(root.join(from), row["saved_source"].as_str().unwrap()).unwrap();
        engine.vault = Vault::open(&root).unwrap();
        engine.file_created(from);
        let old_metadata = serde_json::to_value(engine.file_cache(from).unwrap()).unwrap();
        assert_eq!(old_metadata, row["before"]["raw"], "{id} initialmetadata");
        let physical = canonical_relative(raw);
        std::fs::rename(root.join(from), root.join(&physical)).unwrap();
        engine.vault = Vault::open(&root).unwrap();
        engine.file_renamed_with_raw(from, raw, &physical);
        engine.refresh_vault().unwrap();
        let after = &row["after"];
        assert_state(&engine, after, id);
        assert_eq!(
            serde_json::to_value(engine.file_cache(raw).unwrap()).unwrap(),
            after["raw"],
            "{id} rawmetadata"
        );
        if let Some(canonical) = row["physical_destination"].as_str() {
            assert_eq!(
                serde_json::to_value(engine.file_cache(canonical).unwrap()).unwrap(),
                after["canonical"],
                "{id} canonicalmetadata"
            );
            assert_eq!(
                std::ptr::eq(
                    engine.file_cache(raw).unwrap(),
                    engine.file_cache(canonical).unwrap()
                ),
                after["same_metadata_object"].as_bool().unwrap(),
                "{id} sameobject"
            );
            engine
                .vault
                .modify(canonical, row["modified_source"].as_str().unwrap())
                .unwrap();
            engine.file_modified(canonical);
            let changed = &row["after_canonical_modified"];
            assert_state(&engine, changed, id);
            assert_eq!(
                serde_json::to_value(engine.file_cache(raw).unwrap()).unwrap(),
                changed["raw"],
                "{id} staleghost"
            );
            assert_eq!(
                serde_json::to_value(engine.file_cache(canonical).unwrap()).unwrap(),
                changed["canonical"],
                "{id} modifiedcanonical"
            );
            assert!(!std::ptr::eq(
                engine.file_cache(raw).unwrap(),
                engine.file_cache(canonical).unwrap()
            ));
        } else {
            assert_eq!(
                raw, physical,
                "explicit raw==physical outside path regression"
            );
            assert!(!engine.vault.has_file(raw));
            assert!(engine.file_cache(raw).is_some());
        }
    }
    // Native initialize deletePath for fileCache paths absent from loaded files.
    // Authored source control, not a captured reload phase: _preload at UTF-8
    // byte 1761014 builds a fresh fileCache in stored-key order. Reload removes
    // absent ghost keys while the remaining scalar keys have fresh sorted order.
    engine.reload().unwrap();
    assert_eq!(
        engine.cached_file_paths(),
        ["Ghost.md", "Middle.md", "ParityReady.md"]
    );
}

fn write_snapshot(root: &Path, snapshot: &Value, except: Option<&str>) {
    for (path, record) in snapshot.as_object().unwrap() {
        if except == Some(path.as_str()) {
            continue;
        }
        let target = root.join(path);
        if record["kind"] == "directory" {
            std::fs::create_dir_all(target).unwrap();
        } else {
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            let bytes: Vec<u8> = serde_json::from_value(record["bytes"].clone()).unwrap();
            std::fs::write(target, bytes).unwrap();
        }
    }
}
fn assert_order(engine: &Engine, state: &Value, label: &str) {
    assert_eq!(
        engine.cached_file_paths(),
        string_array(&state["fileCache"]),
        "{label} fileCache"
    );
    assert_eq!(
        engine.resolved_link_paths(),
        state["resolved"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        "{label} resolved"
    );
    assert_eq!(
        engine.ordered_file_paths(),
        string_array(&state["files"]),
        "{label} files"
    );
}
#[test]
fn actual_sixteen_cumulative_raw_rename_chronologies() {
    let receipt: Value =
        serde_json::from_str(include_str!("fixtures/reference_raw_cache_order.json")).unwrap();
    let rows = receipt["records"].as_array().unwrap();
    let fixture = Fixture::new();
    let root = fixture.vault();
    let first_source = format!("vault/{}", rows[0]["source"].as_str().unwrap());
    write_snapshot(&fixture.0, &rows[0]["before"], Some(&first_source));
    let mut engine = Engine::open(&root).unwrap();
    for row in rows {
        let label = row["id"].as_str().unwrap();
        let source = row["source"].as_str().unwrap();
        let raw = row["destination"].as_str().unwrap();
        write_snapshot(&fixture.0, &row["before"], None);
        engine.vault = Vault::open(&root).unwrap();
        engine.file_created(source);
        assert_order(&engine, &row["before_state"], &format!("{label} before"));
        if !row["stdout"].as_str().unwrap().starts_with("Error:") {
            let physical = canonical_relative(raw);
            std::fs::rename(root.join(source), root.join(&physical)).unwrap();
            engine.vault = Vault::open(&root).unwrap();
            engine.file_renamed_with_raw(source, raw, &physical);
            engine.refresh_vault().unwrap();
        }
        assert_order(&engine, &row["after_state"], &format!("{label} after"));
    }
}
