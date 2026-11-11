use mnemosyne_core::CONFIG_DIR;
use mnemosyne_engine::{EditorPosition, EditorSelection, Engine, FileTarget, OpenOptions};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn fixture(files: &Value) -> Fixture {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let root = std::env::temp_dir().join(format!(
        "mn-text-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&root).unwrap();
    for (name, text) in files.as_object().unwrap() {
        let path = root.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text.as_str().unwrap()).unwrap();
    }
    Fixture(root)
}
fn tree(root: &Path) -> Value {
    fn walk(
        root: &Path,
        path: &Path,
        files: &mut BTreeMap<String, String>,
        dirs: &mut Vec<String>,
    ) {
        for e in std::fs::read_dir(path).unwrap() {
            let path = e.unwrap().path();
            let name = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if name == ".mnemosyne" || name == CONFIG_DIR {
                continue;
            }
            if path.is_dir() {
                dirs.push(name);
                walk(root, &path, files, dirs);
            } else {
                files.insert(name, std::fs::read_to_string(path).unwrap());
            }
        }
    }
    let mut files = BTreeMap::new();
    let mut dirs = Vec::new();
    walk(root, root, &mut files, &mut dirs);
    dirs.sort();
    json!({"files":files,"directories":dirs,"vault_exists":true})
}
fn state(engine: &Engine) -> Value {
    json!({"active":engine.active_file(),"editor":engine.editor().map(|buffer|json!({"text":buffer.text,"anchor":buffer.selection.anchor,"head":buffer.selection.head}))})
}
#[test]
fn append_prepend_match_raw_saved_bytes_errors_and_editor_state() {
    let manifest: Value = serde_json::from_str(include_str!("fixtures/text_edit.json")).unwrap();
    let mut failures = Vec::new();
    for case in manifest["cases"].as_array().unwrap() {
        let dir = fixture(&case["initial"]);
        let mut engine = Engine::open(&dir.0).unwrap();
        if case["opened"].as_bool().unwrap() {
            engine
                .open_file(FileTarget::path("Note.md"), OpenOptions::default())
                .unwrap();
            if let Some(dirty) = case["dirty"].as_str() {
                engine.editor_mut().unwrap().set_value(dirty.into());
            }
            let editor = &case["before"]["editor"];
            let position = |key: &str| EditorPosition {
                line: editor[key]["line"].as_i64().unwrap(),
                ch: editor[key]["ch"].as_i64().unwrap(),
            };
            engine
                .editor_mut()
                .unwrap()
                .set_selection(EditorSelection {
                    anchor: position("anchor"),
                    head: position("head"),
                })
                .unwrap();
        }
        assert_eq!(state(&engine), case["before"], "{} setup", case["id"]);
        let args: Vec<String> = serde_json::from_value(case["args"].clone()).unwrap();
        let result = mnemosyne::app::execute(&mut engine, &args);
        for (label, actual, expected) in [
            ("stdout", json!(result), case["expected"]["stdout"].clone()),
            ("tree", tree(&dir.0), case["after"]["tree"].clone()),
            ("state", state(&engine), case["after"]["state"].clone()),
        ] {
            if actual != expected {
                failures.push(format!(
                    "{} {label}\nactual: {actual}\nexpected: {expected}",
                    case["id"]
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} differences:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
#[test]
fn empty_recents_match_captured_reference_message_and_count() {
    let manifest: Value =
        serde_json::from_str(include_str!("fixtures/recents_empty.json")).unwrap();
    let dir = fixture(&json!({}));
    let mut engine = Engine::open(&dir.0).unwrap();
    for case in manifest["cases"].as_array().unwrap() {
        assert_eq!(
            json!({"active":engine.active_file(),"recents":engine.recents()}),
            case["state_proof"]
        );
        let args: Vec<String> = serde_json::from_value(case["args"].clone()).unwrap();
        assert_eq!(
            mnemosyne::app::execute(&mut engine, &args),
            case["output"]["stdout"]
        );
    }
}
