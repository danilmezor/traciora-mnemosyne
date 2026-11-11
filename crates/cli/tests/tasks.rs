use mnemosyne_core::CONFIG_DIR;
use mnemosyne::app;
use mnemosyne_engine::{EditorPosition, EditorSelection, Engine, FileTarget, OpenOptions};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};
static NEXT: AtomicUsize = AtomicUsize::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new(files: &Value) -> Self {
        let path = std::env::temp_dir().join(format!(
            "mnemosyne-tasks-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(path.join(CONFIG_DIR)).unwrap();
        std::fs::write(
            path.join(CONFIG_DIR).join("core-plugins.json"),
            r#"{"daily-notes":false}"#,
        )
        .unwrap();
        for (name, text) in files.as_object().unwrap() {
            let file = path.join(name);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, text.as_str().unwrap()).unwrap();
        }
        Self(path)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn invoke(engine: &mut Engine, args: &Value) -> String {
    let args: Vec<_> = args
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect();
    app::execute(engine, &args)
}
fn editor_state(engine: &Engine) -> Value {
    let active = engine.active_file();
    let editor=engine.editor().map(|buffer|json!({
        "text":buffer.text,"anchor":buffer.selection.anchor,"head":buffer.selection.head,"selection":buffer.selected_text().unwrap()
    }));
    json!({"active":active,"editor":editor})
}
fn open(engine: &mut Engine, path: &str) {
    engine
        .open_file(FileTarget::path(path), OpenOptions::default())
        .unwrap();
}
#[test]
fn task_outputs_match_108_captured_reference_observations() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/tasks.json")).unwrap();
    assert_eq!(fixture["cases"].as_array().unwrap().len(), 108);
    let mut failures = Vec::new();
    for case in fixture["cases"].as_array().unwrap() {
        let dir = Fixture::new(&fixture["files"]);
        let mut engine = Engine::open(&dir.0).unwrap();
        if let Some(active) = case["active"].as_str() {
            open(&mut engine, active)
        }
        let actual = invoke(&mut engine, &case["args"]);
        if actual != case["output"]["stdout"].as_str().unwrap() {
            failures.push(format!(
                "{} {:?}:\nactual {actual:?}\nexpected {:?}",
                case["id"], case["args"], case["output"]["stdout"]
            ));
        }
        assert_eq!(case["output"]["stderr"], "");
        assert_eq!(case["output"]["exit_code"], 0);
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
#[test]
fn task_mutations_match_saved_files_and_editor_state() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/task_mutations.json")).unwrap();
    assert_eq!(fixture["cases"].as_array().unwrap().len(), 14);
    for case in fixture["cases"].as_array().unwrap() {
        let dir = Fixture::new(&case["initial"]);
        let mut engine = Engine::open(&dir.0).unwrap();
        if let Some(path) = case["opened"].as_str() {
            open(&mut engine, path);
            let expected = &case["before"]["state"]["editor"];
            let buffer = engine.editor_mut().unwrap();
            if case["dirty_setup"] == true {
                buffer.set_value(expected["text"].as_str().unwrap().into());
            }
            let position = |key: &str| {
                serde_json::from_value::<EditorPosition>(expected[key].clone()).unwrap()
            };
            buffer
                .set_selection(EditorSelection {
                    anchor: position("anchor"),
                    head: position("head"),
                })
                .unwrap();
        }
        assert_eq!(
            editor_state(&engine),
            case["before"]["state"],
            "{} before editor",
            case["id"]
        );
        assert_eq!(
            invoke(&mut engine, &case["args"]),
            case["output"]["stdout"].as_str().unwrap(),
            "{} output",
            case["id"]
        );
        let actual_files: BTreeMap<_, _> = case["initial"]
            .as_object()
            .unwrap()
            .keys()
            .map(|name| {
                (
                    name.clone(),
                    std::fs::read_to_string(dir.0.join(name)).unwrap(),
                )
            })
            .collect();
        for phase in ["immediate", "after"] {
            assert_eq!(
                serde_json::to_value(&actual_files).unwrap(),
                case[phase]["files"],
                "{} {phase} saved files",
                case["id"]
            );
            assert_eq!(
                editor_state(&engine),
                case[phase]["state"],
                "{} {phase} editor",
                case["id"]
            );
        }
    }
}

#[test]
fn enabled_daily_tasks_match_existing_note_reference_without_opening_editor() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/tasks_daily.json")).unwrap();
    assert_eq!(fixture["cases"].as_array().unwrap().len(), 5);
    for case in fixture["cases"].as_array().unwrap() {
        let dir = Fixture::new(&case["before"]["files"]);
        std::fs::write(
            dir.0.join(CONFIG_DIR).join("core-plugins.json"),
            r#"{"daily-notes":true}"#,
        )
        .unwrap();
        std::fs::write(
            dir.0.join(CONFIG_DIR).join("daily-notes.json"),
            serde_json::to_vec(&case["config"]["options"]).unwrap(),
        )
        .unwrap();
        let mut engine = Engine::open(&dir.0).unwrap();
        assert_eq!(
            invoke(&mut engine, &case["args"]),
            case["output"]["stdout"].as_str().unwrap(),
            "{} output",
            case["id"]
        );
        assert_eq!(
            editor_state(&engine),
            case["after"]["state"],
            "{} editor",
            case["id"]
        );
        let actual_files: BTreeMap<_, _> = case["before"]["files"]
            .as_object()
            .unwrap()
            .keys()
            .map(|name| {
                (
                    name.clone(),
                    std::fs::read_to_string(dir.0.join(name)).unwrap(),
                )
            })
            .collect();
        assert_eq!(
            serde_json::to_value(&actual_files).unwrap(),
            case["after"]["files"],
            "{} saved files",
            case["id"]
        );
    }
}

#[test]
fn task_line_numbers_match_javascript_precision_and_formatting() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/task_numbers.json")).unwrap();
    assert_eq!(fixture["cases"].as_array().unwrap().len(), 6);
    for case in fixture["cases"].as_array().unwrap() {
        let dir = Fixture::new(&fixture["files"]);
        let mut engine = Engine::open(&dir.0).unwrap();
        assert_eq!(
            invoke(&mut engine, &case["args"]),
            case["output"]["stdout"].as_str().unwrap(),
            "{} output",
            case["id"]
        );
    }
}

#[test]
fn source_contract_exact_folder_ref_prevents_filename_fallback() {
    // Shipped task handler uses (exactAbstract || metadataDestination), then
    // checks TFile. An exact folder is truthy and must suppress name fallback.
    let dir = Fixture::new(
        &json!({"Folder/Other.md":"- [ ] other\n","Else/Folder.md":"- [ ] named Folder\n"}),
    );
    let mut engine = Engine::open(&dir.0).unwrap();
    assert_eq!(
        invoke(&mut engine, &json!(["task", "ref=Folder:1"])),
        "Error: File \"Folder\" not found.\n"
    );
    assert!(
        invoke(&mut engine, &json!(["task", "file=Folder", "line=1"]))
            .starts_with("file\tElse/Folder.md\n")
    );
}
