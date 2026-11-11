use mnemosyne_core::CONFIG_DIR;
use mnemosyne::app;
use mnemosyne_engine::{EditorPosition, EditorSelection, Engine, FileTarget, OpenOptions};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
};
static NEXT: AtomicUsize = AtomicUsize::new(0);
struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn fixture(case: &Value) -> Fixture {
    let root = std::env::temp_dir().join(format!(
        "mnemosyne-file-manager-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(root.join(CONFIG_DIR)).unwrap();
    for (name, text) in case["before"]["tree"]["files"].as_object().unwrap() {
        let path = root.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text.as_str().unwrap()).unwrap();
    }
    for dir in case["before"]["tree"]["directories"].as_array().unwrap() {
        std::fs::create_dir_all(root.join(dir.as_str().unwrap())).unwrap();
    }
    std::fs::write(
        root.join(CONFIG_DIR).join("app.json"),
        r#"{"alwaysUpdateLinks":true,"newLinkFormat":"shortest"}"#,
    )
    .unwrap();
    std::fs::write(
        root.join(CONFIG_DIR).join("core-plugins.json"),
        r#"{"daily-notes":false,"templates":false,"canvas":false,"bases":false}"#,
    )
    .unwrap();
    Fixture(root)
}
fn snapshot(root: &Path) -> Value {
    fn walk(
        root: &Path,
        path: &Path,
        files: &mut BTreeMap<String, String>,
        dirs: &mut Vec<String>,
    ) {
        for entry in std::fs::read_dir(path).unwrap().flatten() {
            let path = entry.path();
            let name = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if name == CONFIG_DIR || name == ".mnemosyne" {
                continue;
            }
            if path.is_dir() {
                dirs.push(name);
                walk(root, &path, files, dirs)
            } else {
                files.insert(name, std::fs::read_to_string(path).unwrap());
            }
        }
    }
    let mut files = BTreeMap::new();
    let mut dirs = Vec::new();
    walk(root, root, &mut files, &mut dirs);
    dirs.sort();
    json!({"files":files,"directories":dirs,"vault_exists":root.is_dir()})
}
fn state(engine: &Engine) -> Value {
    let active = engine.active_file();
    let editor=engine.editor().map(|b|json!({"text":b.text,"anchor":b.selection.anchor,"head":b.selection.head,"selection":b.selected_text().unwrap()}));
    let leaves: Vec<_> = engine
        .session()
        .leaves
        .iter()
        .filter_map(|l| {
            l.file
                .as_ref()
                .map(|path| json!({"path":path,"type":l.view_type}))
        })
        .collect();
    json!({"active":active,"editor":editor,"file_leaves":leaves,"files":engine.vault.files()})
}
fn invoke(engine: &mut Engine, argv: &Value) -> String {
    let argv: Vec<_> = argv
        .as_array()
        .unwrap()
        .iter()
        .map(|arg| arg.as_str().unwrap().to_owned())
        .collect();
    app::execute(engine, &argv)
}
#[test]
fn rename_move_match_55_runtime_outputs_saved_bytes_and_retained_session() {
    let manifest: Value = serde_json::from_str(include_str!("fixtures/file_manager.json")).unwrap();
    assert_eq!(manifest["cases"].as_array().unwrap().len(), 55);
    let mut failures = Vec::new();
    for case in manifest["cases"].as_array().unwrap() {
        let dir = fixture(case);
        let mut engine = Engine::open(&dir.0).unwrap();
        if let Some(path) = case["opened"].as_str() {
            engine
                .open_file(FileTarget::path(path), OpenOptions::default())
                .unwrap();
            let expected = &case["before"]["state"]["editor"];
            engine
                .editor_mut()
                .unwrap()
                .set_selection(EditorSelection {
                    anchor: serde_json::from_value::<EditorPosition>(expected["anchor"].clone())
                        .unwrap(),
                    head: serde_json::from_value::<EditorPosition>(expected["head"].clone())
                        .unwrap(),
                })
                .unwrap();
        }
        assert_eq!(
            state(&engine),
            case["before"]["state"],
            "{} beforestate",
            case["id"]
        );
        let output = invoke(&mut engine, &case["args"]);
        let expected = case["output"]["stdout"]
            .as_str()
            .unwrap()
            .replace("{{vault}}", &dir.0.to_string_lossy());
        if output != expected {
            failures.push(format!(
                "{} output actual {output:?}, expected {expected:?}",
                case["id"]
            ));
        }
        for phase in ["immediate", "after"] {
            if snapshot(&dir.0) != case[phase]["tree"] {
                failures.push(format!(
                    "{} {phase} tree actual {}, expected {}",
                    case["id"],
                    snapshot(&dir.0),
                    case[phase]["tree"]
                ));
            }
            // File-index settling is asynchronous in several native captures;
            // compare its final state while preserving both raw observations.
            let mut actual = state(&engine);
            let mut expected = case[phase]["state"].clone();
            if phase == "immediate" {
                actual.as_object_mut().unwrap().remove("files");
                expected.as_object_mut().unwrap().remove("files");
            }
            if actual != expected {
                failures.push(format!(
                    "{} {phase} state actual {actual}, expected {expected}",
                    case["id"]
                ));
            }
        }
        assert_eq!(case["output"]["stderr"], "");
        assert_eq!(case["output"]["exit_code"], 0);
        for path in engine.vault.notes.keys() {
            assert_eq!(mnemosyne_core::extension(path), "md");
            assert!(!mnemosyne_core::has_hidden_ancestor(path));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn rename_move_traversal_matches_16_owned_parent_runtime_cases() {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use std::collections::BTreeSet;
    fn physical_tree(root: &Path) -> Value {
        fn walk(root: &Path, current: &Path, result: &mut serde_json::Map<String, Value>) {
            for entry in std::fs::read_dir(current).unwrap().flatten() {
                let path = entry.path();
                if entry.file_name() == ".mnemosyne" || entry.file_name() == CONFIG_DIR {
                    continue;
                }
                let key = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                if path.is_dir() {
                    if key != "vault" && key != "sibling" {
                        result.insert(key, json!({"kind":"directory"}));
                    }
                    walk(root, &path, result);
                } else {
                    result.insert(key, json!({"kind":"file","bytes_base64":STANDARD.encode(std::fs::read(path).unwrap())}));
                }
            }
        }
        let mut result = serde_json::Map::new();
        walk(root, root, &mut result);
        Value::Object(result)
    }
    let manifest: Value =
        serde_json::from_str(include_str!("fixtures/file_manager_traversal.json")).unwrap();
    assert_eq!(manifest["records"].as_array().unwrap().len(), 16);
    for case in manifest["records"].as_array().unwrap() {
        // Own the whole parent boundary, including every outside destination.
        let boundary = Fixture(std::env::temp_dir().join(format!(
            "mnemosyne-rename-traversal-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )));
        let vault = boundary.0.join("vault");
        std::fs::create_dir_all(&vault).unwrap();
        std::fs::create_dir(boundary.0.join("sibling")).unwrap();
        for (name, entry) in case["before"].as_object().unwrap() {
            let path = boundary.0.join(name);
            if entry["kind"] == "directory" {
                std::fs::create_dir_all(path).unwrap();
            } else {
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(
                    path,
                    STANDARD
                        .decode(entry["bytes_base64"].as_str().unwrap())
                        .unwrap(),
                )
                .unwrap();
            }
        }
        let mut engine = Engine::open(&vault).unwrap();
        assert_eq!(physical_tree(&boundary.0), case["before"]);
        let expected = case["stdout"]
            .as_str()
            .unwrap()
            .replace("{{boundary}}", &boundary.0.to_string_lossy());
        assert_eq!(
            invoke(&mut engine, &case["args"]),
            expected,
            "{} pipe",
            case["id"]
        );
        assert_eq!(
            physical_tree(&boundary.0),
            case["after"],
            "{} filesystem",
            case["id"]
        );
        let expected_files: BTreeSet<String> =
            serde_json::from_value(case["after_state"]["files"].clone()).unwrap();
        assert_eq!(
            engine.vault.files(),
            &expected_files,
            "{} settled file membership",
            case["id"]
        );
        // The raw fileCache/resolved ghost keys remain in the fixture so the
        // separate retained-cache implementation gap stays reviewable.
    }
}

#[test]
fn raw_dot_same_file_rename_matches_two_runtime_errors_and_unchanged_bytes() {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    let traversal: Value =
        serde_json::from_str(include_str!("fixtures/file_manager_traversal.json")).unwrap();
    let manifest: Value = serde_json::from_str(include_str!("fixtures/file_manager.json")).unwrap();
    let dir = fixture(&manifest["cases"][0]);
    let records = traversal["same_file_cases"].as_array().unwrap();
    assert_eq!(records.len(), 2);
    std::fs::write(
        dir.0.join("Known.md"),
        STANDARD
            .decode(records[0]["before_bytes"].as_str().unwrap())
            .unwrap(),
    )
    .unwrap();
    let mut engine = Engine::open(&dir.0).unwrap();
    let before = snapshot(&dir.0);
    for case in records {
        assert_eq!(invoke(&mut engine, &case["args"]), case["stdout"]);
        assert_eq!(snapshot(&dir.0), before);
        assert_eq!(
            STANDARD.encode(std::fs::read(dir.0.join("Known.md")).unwrap()),
            case["after_bytes"]
        );
    }
}

#[test]
fn traversal_dispatch_preserves_16_native_warm_cache_source_orders() {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    let manifest: Value =
        serde_json::from_str(include_str!("fixtures/file_manager_traversal.json")).unwrap();
    let records = manifest["records"].as_array().unwrap();
    let boundary = Fixture(std::env::temp_dir().join(format!(
        "mnemosyne-rename-warm-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )));
    let vault = boundary.0.join("vault");
    std::fs::create_dir_all(vault.join("Sub")).unwrap();
    std::fs::create_dir(boundary.0.join("sibling")).unwrap();
    for (name, entry) in records[0]["before"].as_object().unwrap() {
        if entry["kind"] == "file" {
            std::fs::write(
                boundary.0.join(name),
                STANDARD
                    .decode(entry["bytes_base64"].as_str().unwrap())
                    .unwrap(),
            )
            .unwrap();
        }
    }
    let mut engine = Engine::open(&vault).unwrap();
    let initial_order: Vec<String> =
        serde_json::from_value(records[0]["before_state"]["fileCache"].clone()).unwrap();
    engine.import_cache_order(&initial_order).unwrap();
    for case in records {
        let source = case["source"].as_str().unwrap();
        if !engine.vault.has_file(source) {
            let bytes = STANDARD
                .decode(
                    case["before"][format!("vault/{source}")]["bytes_base64"]
                        .as_str()
                        .unwrap(),
                )
                .unwrap();
            engine
                .vault
                .create(source, std::str::from_utf8(&bytes).unwrap())
                .unwrap();
            engine.file_created(source);
        }
        let expected = case["stdout"]
            .as_str()
            .unwrap()
            .replace("{{boundary}}", &boundary.0.to_string_lossy());
        assert_eq!(
            invoke(&mut engine, &case["args"]),
            expected,
            "{} pipe",
            case["id"]
        );
        let file_cache: Vec<String> =
            serde_json::from_value(case["after_state"]["fileCache"].clone()).unwrap();
        assert_eq!(
            engine.cached_file_paths(),
            file_cache,
            "{} fileCache chronology",
            case["id"]
        );
        let resolved: Vec<&str> = case["after_state"]["resolved"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            engine.resolved_link_paths(),
            resolved,
            "{} resolved chronology",
            case["id"]
        );
    }
}
