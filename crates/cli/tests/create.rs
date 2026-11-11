use mnemosyne_core::CONFIG_DIR;
use mnemosyne::{app, create, search::Params};
use mnemosyne_engine::{Engine, FileTarget, OpenOptions};
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
    json!({"active":active,"editor":editor,"file_leaves":leaves})
}
fn fixture(case: &Value) -> Fixture {
    let root = std::env::temp_dir().join(format!(
        "mnemosyne-create-{}-{}",
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
    let plugins = json!({"templates":case["config"]["enabled"],"canvas":case["config"]["feature_plugins"]["canvas"],"bases":case["config"]["feature_plugins"]["bases"],"daily-notes":false});
    std::fs::write(
        root.join(CONFIG_DIR).join("core-plugins.json"),
        serde_json::to_vec(&plugins).unwrap(),
    )
    .unwrap();
    std::fs::write(
        root.join(CONFIG_DIR).join("templates.json"),
        serde_json::to_vec(&case["config"]["options"]).unwrap(),
    )
    .unwrap();
    std::fs::write(
        root.join(CONFIG_DIR).join("app.json"),
        serde_json::to_vec(&case["settings"]).unwrap(),
    )
    .unwrap();
    Fixture(root)
}
fn invoke(engine: &mut Engine, argv: &Value) -> String {
    let args: Vec<_> = argv
        .as_array()
        .unwrap()
        .iter()
        .map(|arg| arg.as_str().unwrap().to_owned())
        .collect();
    app::execute(engine, &args)
}

#[test]
fn create_matches_63_reference_outputs_file_bytes_and_session_effects() {
    let suites: [Value; 2] = [
        serde_json::from_str(include_str!("fixtures/create.json")).unwrap(),
        serde_json::from_str(include_str!("fixtures/create_edges.json")).unwrap(),
    ];
    let cases: Vec<_> = suites
        .iter()
        .flat_map(|suite| suite["cases"].as_array().unwrap())
        .collect();
    assert_eq!(cases.len(), 63);
    let mut failures = Vec::new();
    for case in cases {
        let dir = fixture(case);
        let mut engine = Engine::open(&dir.0).unwrap();
        if let Some(path) = case["before"]["state"]["active"].as_str() {
            engine
                .open_file(FileTarget::path(path), OpenOptions::default())
                .unwrap();
        }
        assert_eq!(
            snapshot(&dir.0),
            case["before"]["tree"],
            "{} beforetree",
            case["id"]
        );
        assert_eq!(
            state(&engine),
            case["before"]["state"],
            "{} beforestate",
            case["id"]
        );
        let actual = invoke(&mut engine, &case["args"]);
        if actual != case["output"]["stdout"].as_str().unwrap() {
            failures.push(format!(
                "{} outputactual {actual:?} expected {:?}",
                case["id"], case["output"]["stdout"]
            ));
        }
        if snapshot(&dir.0) != case["after"]["tree"] {
            failures.push(format!(
                "{} treeactual {} expected {}",
                case["id"],
                snapshot(&dir.0),
                case["after"]["tree"]
            ));
        }
        if state(&engine) != case["after"]["state"] {
            failures.push(format!(
                "{} stateactual {} expected {}",
                case["id"],
                state(&engine),
                case["after"]["state"]
            ));
        }
        assert_eq!(case["output"]["exit_code"], 0);
        assert_eq!(case["output"]["stderr"], "");
        for path in engine.vault.notes.keys() {
            assert_eq!(
                mnemosyne_core::extension(path),
                "md",
                "created attachment must not be indexed as note"
            );
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn source_contract_hidden_collision_preserves_existing_file() {
    // Available-path lookup sees only indexed entries; the final adapter exists
    // check rejects the unindexed hidden collision without overwriting bytes.
    let cases: Value = serde_json::from_str(include_str!("fixtures/create.json")).unwrap();
    let case = cases["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "hiddenname")
        .unwrap();
    let dir = fixture(case);
    let mut engine = Engine::open(&dir.0).unwrap();
    let mut params = Params::new();
    params.insert("name".into(), Some(".hidden".into()));
    params.insert("content".into(), Some("first".into()));
    assert_eq!(
        mnemosyne::error::render(&create::execute(&mut engine, &params).unwrap_err()),
        "TypeError: Cannot read properties of null (reading 'path')\n"
    );
    params.insert("content".into(), Some("replacement".into()));
    assert_eq!(
        create::execute(&mut engine, &params)
            .unwrap_err()
            .to_string(),
        "File already exists."
    );
    assert_eq!(
        std::fs::read_to_string(dir.0.join(".hidden.md")).unwrap(),
        "first"
    );
}

#[test]
fn source_contract_overwrite_folder_uses_collision_path_and_overwrote_label() {
    // Native label uses the truthiness of the original exact abstract lookup,
    // although only TFile may enter its direct-modify branch.
    let cases: Value = serde_json::from_str(include_str!("fixtures/create.json")).unwrap();
    let case = cases["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "foldercollision")
        .unwrap();
    let dir = fixture(case);
    let mut engine = Engine::open(&dir.0).unwrap();
    let mut params = Params::new();
    params.insert("name".into(), Some("Folder.md".into()));
    params.insert("overwrite".into(), None);
    assert_eq!(
        create::execute(&mut engine, &params).unwrap(),
        "Overwrote: Folder 1.md"
    );
    assert!(dir.0.join("Folder.md/Child.md").is_file());
}

#[test]
fn source_contract_regular_file_parent_rejects_folder_creation() {
    let cases: Value = serde_json::from_str(include_str!("fixtures/create.json")).unwrap();
    let dir = fixture(&cases["cases"][0]);
    std::fs::write(dir.0.join("Blocker"), "retain attachment").unwrap();
    let mut engine = Engine::open(&dir.0).unwrap();
    let mut params = Params::new();
    params.insert("path".into(), Some("Blocker/Child".into()));
    assert_eq!(
        create::execute(&mut engine, &params)
            .unwrap_err()
            .to_string(),
        "Folder already exists."
    );
    assert_eq!(
        std::fs::read_to_string(dir.0.join("Blocker")).unwrap(),
        "retain attachment"
    );
}

#[test]
fn traversal_create_matches_five_owned_parent_reference_cases() {
    let suite: Value =
        serde_json::from_str(include_str!("fixtures/create_traversal.json")).unwrap();
    for case in suite["cases"].as_array().unwrap() {
        // This fixture owns the parent and sibling as well as the vault, so
        // the observed mkdir side effect cannot affect any unrelated path.
        let boundary = Fixture(std::env::temp_dir().join(format!(
            "mnemosyne-create-traversal-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )));
        let vault = boundary.0.join("vault");
        for directory in suite["initial_directories"].as_array().unwrap() {
            std::fs::create_dir_all(boundary.0.join(directory.as_str().unwrap())).unwrap();
        }
        for (name, bytes) in suite["initial_vault_files"].as_object().unwrap() {
            std::fs::write(vault.join(name), bytes.as_str().unwrap()).unwrap();
        }
        let mut engine = Engine::open(&vault).unwrap();
        let before = snapshot(&boundary.0);
        assert_eq!(
            invoke(&mut engine, &case["args"]),
            case["stdout"],
            "{}",
            case["id"]
        );
        let mut after = before;
        let directories = after["directories"].as_array_mut().unwrap();
        directories.extend(
            case["created_directories"]
                .as_array()
                .unwrap()
                .iter()
                .cloned(),
        );
        directories.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
        assert_eq!(snapshot(&boundary.0), after, "{} filesystem", case["id"]);
        assert_eq!(
            engine
                .vault
                .files()
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["ParityReady.md"]
        );
    }
}

#[test]
fn falsy_extensions_infer_from_stem_before_using_retained_creators() {
    let suite: Value =
        serde_json::from_str(include_str!("fixtures/create_extensions.json")).unwrap();
    let dir = Fixture(std::env::temp_dir().join(format!(
        "mn-create-ext-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )));
    std::fs::create_dir_all(dir.0.join(CONFIG_DIR)).unwrap();
    std::fs::write(
        dir.0.join(CONFIG_DIR).join("core-plugins.json"),
        r#"{"canvas":true,"bases":true}"#,
    )
    .unwrap();
    let mut engine = Engine::open(&dir.0).unwrap();
    for case in suite["cases"].as_array().unwrap() {
        assert_eq!(
            invoke(&mut engine, &case["args"]),
            case["expected"]["stdout"]
        );
        assert_eq!(snapshot(&dir.0), case["after"], "{} bytes", case["args"]);
    }
}
