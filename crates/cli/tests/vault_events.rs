//! Four native vault callbacks through retained CLI dispatch. Collection is
//! opt-in and payload paths are frozen at notification time. This does not claim
//! watcher/autosave/folder event timing or complete mutable SDK TFile payloads.
use mnemosyne_core::CONFIG_DIR;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use mnemosyne_engine::{
    events::{EventJournalError, EventName},
    Engine,
};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
const CORE_PLUGINS: [&str; 30] = [
    "audio-recorder",
    "backlink",
    "bases",
    "bookmarks",
    "canvas",
    "command-palette",
    "daily-notes",
    "editor-status",
    "file-explorer",
    "file-recovery",
    "footnotes",
    "global-search",
    "graph",
    "note-composer",
    "outgoing-link",
    "outline",
    "page-preview",
    "properties",
    "publish",
    "random-note",
    "slash-command",
    "slides",
    "switcher",
    "sync",
    "tag-pane",
    "templates",
    "webviewer",
    "word-count",
    "workspaces",
    "zk-prefixer",
];
struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn fixture() -> (Fixture, Engine) {
    let root = Fixture(std::env::temp_dir().join(format!(
            "mn-vault-events-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )));
    fs::create_dir_all(root.0.join(CONFIG_DIR)).unwrap();
    fs::write(root.0.join("Initial.md"), "Initial fixture.\n").unwrap();
    // Match the capture's disabled plugin setting. This is an independently
    // authored setup, not import of post-operation metadata or event snapshots.
    let plugins: serde_json::Map<String, Value> = CORE_PLUGINS
        .iter()
        .map(|id| (id.to_string(), Value::Bool(false)))
        .collect();
    fs::write(
        root.0.join(CONFIG_DIR).join("core-plugins.json"),
        serde_json::to_vec(&plugins).unwrap(),
    )
    .unwrap();
    let mut engine = Engine::open(&root.0).unwrap();
    let mut state = engine.session().clone();
    // Declared controlled initial leaf identity and prior creation chronology;
    // retain it through every command rather than patching later observations.
    state.leaves[0].id = "parity-leaf".into();
    state.active_leaf = Some("parity-leaf".into());
    // Rebuild the authored one-leaf topology with the declared ID as well.
    // No post-operation native state is imported into this fixture.
    state.pane_layout = None;
    state.last_open_files = vec!["ParityReady.md".into(), "Initial.md".into()];
    engine.restore(state).unwrap();
    (root, engine)
}
fn session(engine: &Engine) -> Value {
    json!({"active_file":engine.active_file(),"leaves":engine.session().leaves.iter().map(|leaf|json!({"id":leaf.id,"file":leaf.file,"view_type":leaf.view_type})).collect::<Vec<_>>(),"last_open_files":engine.recents()})
}
fn tree(root: &Path) -> Value {
    fn walk(
        root: &Path,
        path: &Path,
        files: &mut BTreeMap<String, String>,
        dirs: &mut Vec<String>,
    ) {
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let path = entry.path();
            let name = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if path.is_dir() {
                dirs.push(name);
                walk(root, &path, files, dirs);
            } else {
                files.insert(name, STANDARD.encode(fs::read(&path).unwrap()));
            }
        }
    }
    let mut files = BTreeMap::new();
    let mut dirs = Vec::new();
    walk(root, root, &mut files, &mut dirs);
    dirs.sort();
    json!({"files":files,"directories":dirs,"vault_exists":root.is_dir()})
}
#[test]
fn retained_cli_emits_exact_native_four_events_with_frozen_paths_and_unopened_modify() {
    let native: Value = serde_json::from_str(include_str!(
        "../../engine/tests/fixtures/vault_events.json"
    ))
    .unwrap();
    assert_eq!(native["identity"]["reference_version"], "1.14.4");
    let (root, mut engine) = fixture();
    assert_eq!(
        engine.event_journal_since(0),
        Err(EventJournalError::Disabled)
    );
    assert_eq!(session(&engine), native["initial_snapshot"]["session"]);
    let operation_start_cursor = engine.event_cursor();
    let mut held = Vec::new();
    for step in native["steps"].as_array().unwrap() {
        let cursor = engine.start_event_journal();
        assert!(engine.event_journal_since(cursor).unwrap().is_empty());
        assert_eq!(
            session(&engine),
            step["before"]["session"],
            "{} before session",
            step["id"]
        );
        assert_eq!(
            tree(&root.0),
            step["before_files"],
            "{} before bytes",
            step["id"]
        );
        let args: Vec<String> = serde_json::from_value(step["args"].clone()).unwrap();
        let output = mnemosyne::app::execute(&mut engine, &args);
        assert_eq!(
            output.as_bytes(),
            STANDARD
                .decode(step["expected_process"]["stdout"].as_str().unwrap())
                .unwrap(),
            "{} output",
            step["id"]
        );
        assert_eq!(step["expected_process"]["stderr"], "");
        assert_eq!(step["expected_process"]["exit_code"], 0);
        let actual = engine.event_journal_since(cursor).unwrap();
        assert_eq!(actual.len(), 1, "{} event cardinality", step["id"]);
        held.extend_from_slice(actual);
        let mut projection = serde_json::to_value(actual).unwrap();
        // Native probe instrumentation restarts its ordinal at every step;
        // this comparison does not advertise a native public sequence counter.
        for (ordinal, event) in projection.as_array_mut().unwrap().iter_mut().enumerate() {
            event["sequence"] = json!(ordinal + 1);
        }
        assert_eq!(
            projection, step["after"]["events"],
            "{} exact event",
            step["id"]
        );
        assert_eq!(
            session(&engine),
            step["after"]["session"],
            "{} after session",
            step["id"]
        );
        assert_eq!(
            tree(&root.0),
            step["after_files"],
            "{} after bytes",
            step["id"]
        );
        assert!(
            engine.editor().is_none(),
            "unopened operation must not invent an editor"
        );
    }
    assert_eq!(
        held.iter().map(|event| event.sequence).collect::<Vec<_>>(),
        (1..=4)
            .map(|offset| operation_start_cursor + offset)
            .collect::<Vec<_>>(),
        "internal cursor control: actual operations advance from the captured starting cursor"
    );
    assert_eq!(
        held[0].path, "P1A.md",
        "create callback path must not become the later rename destination"
    );
    assert_eq!(held[2].name, EventName::Rename);
    assert_eq!(held[2].path, "P1B.md");
    assert_eq!(held[2].old_path.as_deref(), Some("P1A.md"));
    assert_eq!(held[3].name, EventName::Delete);
    assert_eq!(held[3].path, "P1B.md");
    assert!(
        !engine.vault.has_file("P1B.md"),
        "deleted callback payload survives missing current file"
    );
    engine.stop_event_journal();
    assert_eq!(
        engine.event_journal_since(engine.event_cursor()),
        Err(EventJournalError::Disabled)
    );
}
#[test]
fn collection_does_not_infer_refresh_events_or_double_count_raw_rename_delegation() {
    let (_root, mut engine) = fixture();
    let cursor = engine.start_event_journal();
    engine.refresh_cache();
    engine.refresh_vault().unwrap();
    assert!(engine.event_journal_since(cursor).unwrap().is_empty());
    // Explicit physical rename followed by the source's raw-path notification.
    // This supplements the native four with an authored delegation invariant;
    // it does not certify raw watcher callback ordering.
    engine.vault.rename("Initial.md", "Renamed.md").unwrap();
    engine.file_renamed_with_raw("Initial.md", "./Renamed.md", "Renamed.md");
    let events = engine.event_journal_since(cursor).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].name, EventName::Rename);
    assert_eq!(events[0].path, "./Renamed.md");
    assert_eq!(events[0].old_path.as_deref(), Some("Initial.md"));
    let next = engine.event_cursor();
    engine.synchronize_file_modified("Renamed.md");
    assert!(engine.event_journal_since(next).unwrap().is_empty());
    engine.vault.modify("Renamed.md", "changed").unwrap();
    engine.file_modified("Renamed.md");
    assert_eq!(engine.event_journal_since(next).unwrap().len(), 1);
    // Failed real dispatch does not reach a successful operation notification.
    let cursor = engine.start_event_journal();
    let output = mnemosyne::app::execute(
        &mut engine,
        &[
            "append".into(),
            "path=Missing.md".into(),
            "content=X".into(),
        ],
    );
    assert!(output.starts_with("Error:"));
    assert!(engine.event_journal_since(cursor).unwrap().is_empty());
}

#[test]
fn raw_alias_target_rename_matches_native_delete_then_canonical_modify() {
    // Reuse the actual warm alias/cache fixture, rather than infer a callback
    // sequence from the settled filesystem or metadata maps.
    let native: Value = serde_json::from_str(include_str!(
        "../../engine/tests/fixtures/reference_external_graph.json"
    ))
    .unwrap();
    let case = &native["ghost_cases"][0];
    assert_eq!(case["id"], "target-renamed");
    let (root, mut engine) = fixture();
    // The external-graph capture was taken with alwaysUpdateLinks=true. Preserve that input now that
    // default-false linked renames genuinely wait for a decision.
    engine.set_config("alwaysUpdateLinks", json!(true)).unwrap();
    let target = case["target"].as_str().unwrap();
    let source = case["source"].as_str().unwrap();
    let raw = case["raw"].as_str().unwrap();
    let canonical = case["canonical"].as_str().unwrap();
    engine.vault.create(target, "target\n").unwrap();
    engine.file_created(target);
    engine
        .vault
        .create(
            source,
            &format!(
                "[[{}]] [[{}]]\n",
                target.strip_suffix(".md").unwrap(),
                case["missing"].as_str().unwrap()
            ),
        )
        .unwrap();
    engine.file_created(source);
    assert_eq!(
        mnemosyne::app::execute(
            &mut engine,
            &["move".into(), format!("path={source}"), format!("to={raw}"),]
        ),
        format!("Moved: {source} -> {raw}\n")
    );
    assert!(engine.file_cache(raw).is_some());
    let cursor = engine.start_event_journal();
    let output = mnemosyne::app::execute(
        &mut engine,
        &[
            "rename".into(),
            format!("path={target}"),
            format!("name=New-{target}"),
        ],
    );
    assert_eq!(
        output.as_bytes(),
        STANDARD
            .decode(case["action"]["stdout"].as_str().unwrap())
            .unwrap()
    );
    let actual = engine
        .event_journal_since(cursor)
        .unwrap()
        .iter()
        .map(|event| {
            let mut values = vec![json!(event.name), json!(event.path)];
            if let Some(old) = &event.old_path {
                values.push(json!(old));
            }
            Value::Array(values)
        })
        .collect::<Vec<_>>();
    let expected = case["after"]["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| matches!(event[0].as_str(), Some("rename" | "delete" | "modify")))
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        actual, expected,
        "exact Vault callbacks, excluding metadata events"
    );
    assert!(engine.file_cache(raw).is_none());
    assert_eq!(
        fs::read_to_string(root.0.join(canonical)).unwrap(),
        case["after"]["post_stop_saved_source"].as_str().unwrap()
    );
}
