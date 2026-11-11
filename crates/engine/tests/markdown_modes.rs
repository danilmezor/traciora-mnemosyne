use mnemosyne_engine::{
    markdown::MarkdownMode, EditorPosition, EditorSelection, Engine, FileTarget, OpenOptions,
};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "mnemosyne-mode-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn receipt() -> Value {
    serde_json::from_str(include_str!("fixtures/reference_markdown_modes.json")).unwrap()
}
fn snapshot(engine: &Engine, files: &Value) -> Value {
    let leaves=engine.session().leaves.iter().filter(|leaf|leaf.file.is_some()).map(|leaf| {
        let path=leaf.file.as_deref().unwrap();let state=leaf.markdown.as_ref().unwrap();let editor=engine.editor_for_leaf(&leaf.id).unwrap();
        json!({"id":leaf.id,"type":leaf.view_type,"state":{"file":path,"mode":state.mode,"source":state.source,"backlinks":state.backlinks},"mode":state.mode,"source":state.source,
            "editor":{"text":editor.text,"cursor":editor.selection.head,"anchor":editor.selection.anchor,"head":editor.selection.head},"dirty":editor.edit_revision!=state.clean_revision(),
            "viewData":if state.mode==MarkdownMode::Source{editor.text.as_str()}else{state.reading_data()},"lastSavedData":state.last_saved_data()})
    }).collect::<Vec<_>>();
    let current = leaves
        .iter()
        .find(|leaf| leaf["id"].as_str() == engine.session().active_leaf.as_deref())
        .cloned();
    let saved = files
        .as_object()
        .unwrap()
        .keys()
        .map(|path| {
            (
                path.clone(),
                json!(std::fs::read_to_string(engine.vault.root.join(path)).unwrap()),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    json!({"activeLeaf":engine.session().active_leaf,"active":engine.active_file(),"current":current,"leaves":leaves,"saved":saved,"config":{"defaultViewMode":engine.get_config("defaultViewMode"),"livePreview":engine.get_config("livePreview")}})
}
fn replace_ids(value: &mut Value, ids: &BTreeMap<String, String>) {
    match value {
        Value::String(s) => {
            if let Some(replacement) = ids.get(s) {
                *s = replacement.clone()
            }
        }
        Value::Array(values) => {
            for value in values {
                replace_ids(value, ids)
            }
        }
        Value::Object(map) => {
            for value in map.values_mut() {
                replace_ids(value, ids)
            }
        }
        _ => {}
    }
}
fn without_timed_dirty(mut value: Value) -> Value {
    fn remove(value: &mut Value) {
        match value {
            Value::Object(map) => {
                map.shift_remove("dirty");
                for value in map.values_mut() {
                    remove(value);
                }
            }
            Value::Array(values) => {
                for value in values {
                    remove(value)
                }
            }
            _ => {}
        }
    }
    remove(&mut value);
    value
}
#[test]
fn actual_twenty_seven_mode_actions_match_all_leaf_states_editor_data_and_saved_files() {
    replay_modes(&receipt());
}
fn replay_modes(data: &Value) {
    let fixture = Fixture::new();
    for (path, source) in data["initial_files"].as_object().unwrap() {
        std::fs::write(fixture.0.join(path), source.as_str().unwrap()).unwrap();
    }
    std::fs::write(
        fixture.0.join("ParityReady.md"),
        "Isolated parity oracle.\n",
    )
    .unwrap();
    let mut engine = Engine::open(&fixture.0).unwrap();
    engine
        .open_file(FileTarget::path("ParityReady.md"), OpenOptions::default())
        .unwrap();
    let mut ids = BTreeMap::from([(
        data["initial"]["activeLeaf"].as_str().unwrap().into(),
        engine.session().active_leaf.clone().unwrap(),
    )]);
    for case in data["cases"].as_array().unwrap() {
        let id = case["id"].as_str().unwrap();
        let mut expected = case["before"].clone();
        replace_ids(&mut expected, &ids);
        assert_eq!(
            without_timed_dirty(snapshot(&engine, &data["initial_files"])),
            without_timed_dirty(expected),
            "{id} before"
        );
        let action = &case["action"];
        match action["type"].as_str().unwrap() {
            "open" => {
                engine
                    .open_file(
                        FileTarget::path(action["path"].as_str().unwrap()),
                        OpenOptions {
                            new_tab: action["new_tab"].as_bool().unwrap(),
                        },
                    )
                    .unwrap();
            }
            "cursor" => {
                let position = EditorPosition {
                    line: action["line"].as_i64().unwrap(),
                    ch: action["ch"].as_i64().unwrap(),
                };
                engine
                    .editor_mut()
                    .unwrap()
                    .set_selection(EditorSelection {
                        anchor: position,
                        head: position,
                    })
                    .unwrap();
            }
            "state" => {
                engine.set_markdown_state(&action["state"]).unwrap();
            }
            "subpath" => {
                engine
                    .apply_subpath(action["subpath"].as_str().unwrap())
                    .unwrap();
            }
            "config" => {
                engine
                    .set_config(action["key"].as_str().unwrap(), action["value"].clone())
                    .unwrap();
            }
            "editor" => engine
                .editor_mut()
                .unwrap()
                .set_value(action["text"].as_str().unwrap().into()),
            "saved_modify" => {
                let path = engine.active_file().unwrap().to_owned();
                engine
                    .vault
                    .modify(&path, action["text"].as_str().unwrap())
                    .unwrap();
                engine.file_modified(&path);
            }
            other => panic!("{other}"),
        }
        ids.entry(case["after"]["activeLeaf"].as_str().unwrap().into())
            .or_insert_with(|| engine.session().active_leaf.clone().unwrap());
        let mut expected = case["after"].clone();
        replace_ids(&mut expected, &ids);
        assert_eq!(
            without_timed_dirty(snapshot(&engine, &data["initial_files"])),
            without_timed_dirty(expected),
            "{id} after"
        );
    }
}

#[test]
fn actual_ten_reading_saved_modifications_crlf_and_mode_property_coercion() {
    // These paced snapshots did cross pending autosave callbacks. The exact
    // dirty field remains in receipts; the separate single-eval fixture below
    // certifies immediate dirty transitions and actual 2-second callbacks.
    let data: Value = serde_json::from_str(include_str!(
        "fixtures/reference_markdown_mode_followup.json"
    ))
    .unwrap();
    replay_modes(&data);
}

fn timer_frame(engine: &Engine) -> Value {
    let state = engine.markdown_state().unwrap();
    json!({"mode":state.mode,"source":state.source,"dirty":engine.markdown_is_dirty(),"editor":engine.editor().unwrap().text,"viewData":engine.markdown_view_data(),"lastSavedData":state.last_saved_data()})
}
fn assert_timer_frame(engine: &Engine, expected: &Value, id: &str) {
    let mut expected = expected.clone();
    let object = expected.as_object_mut().unwrap();
    object.shift_remove("elapsed_ms");
    object.shift_remove("label");
    assert_eq!(timer_frame(engine), expected, "{id}");
}
#[test]
fn actual_six_single_eval_dirty_transitions_and_explicit_autosave_deadlines() {
    let data: Value =
        serde_json::from_str(include_str!("fixtures/reference_markdown_timers.json")).unwrap();
    let fixture = Fixture::new();
    let mut engine = Engine::open(&fixture.0).unwrap();
    for (index, case) in data["cases"].as_array().unwrap().iter().enumerate() {
        let path = case["path"].as_str().unwrap();
        engine.vault.create(path, "original text\n").unwrap();
        engine.file_created(path);
        engine
            .open_file(FileTarget::path(path), OpenOptions { new_tab: true })
            .unwrap();
        engine
            .set_markdown_state(&json!({"mode":case["initial_mode"],"source":case["source"]}))
            .unwrap();
        let base = (index as u64) * 10000;
        for frame in case["result"]["frames"].as_array().unwrap() {
            let label = frame["label"].as_str().unwrap();
            match label {
                "loaded" => {}
                "saved-modify" => {
                    engine.vault.modify(path, "modified saved text\n").unwrap();
                    engine.file_modified(path);
                }
                "editor-immediate" => engine
                    .set_editor_value_at("unsaved editor text\n".into(), base)
                    .unwrap(),
                "mode-immediate" => {
                    let mode = if case["initial_mode"] == "source" {
                        "preview"
                    } else {
                        "source"
                    };
                    engine.set_markdown_state(&json!({"mode":mode})).unwrap();
                }
                "after-microtask" => {}
                "after-dirty-cleared-or-deadline" => {
                    assert!(
                        engine
                            .process_markdown_timers(base + 1999)
                            .unwrap()
                            .is_empty(),
                        "source Gl deadline must not fire early"
                    );
                    if frame["elapsed_ms"].as_f64().unwrap() >= 2000.0 {
                        assert!(engine
                            .process_markdown_timers(base + 2000)
                            .unwrap()
                            .contains(&engine.session().active_leaf.clone().unwrap()));
                    }
                }
                _ => panic!("{label}"),
            }
            assert_timer_frame(&engine, frame, case["id"].as_str().unwrap());
        }
        assert_eq!(
            engine.read_saved(FileTarget::default()).unwrap(),
            case["result"]["saved"]
        );
        // Source→reading saves immediately but its existing debounced callback
        // remains queued, exactly the cause of the original paced ambiguity.
        engine.process_markdown_timers(base + 4000).unwrap();
    }
}

#[test]
fn source_save_failure_resets_view_dirty_and_keeps_old_saved_baseline_without_switching_mode() {
    // Authored failure injection for the shipped Q4.save try/catch contract;
    // this is not a native write-failure conformance receipt.
    let fixture = Fixture::new();
    std::fs::write(fixture.0.join("Note.md"), "old saved\n").unwrap();
    let mut engine = Engine::open(&fixture.0).unwrap();
    engine
        .open_file(FileTarget::path("Note.md"), OpenOptions::default())
        .unwrap();
    engine.set_editor_value_at("new dirty\n".into(), 0).unwrap();
    std::fs::remove_file(fixture.0.join("Note.md")).unwrap();
    std::fs::create_dir(fixture.0.join("Note.md")).unwrap();
    assert!(engine
        .set_markdown_state(&json!({"mode":"preview"}))
        .is_err());
    assert_eq!(engine.markdown_state().unwrap().mode, MarkdownMode::Source);
    assert_eq!(engine.markdown_is_dirty(), Some(false));
    assert_eq!(
        engine.markdown_state().unwrap().last_saved_data(),
        "old saved\n"
    );
    assert_eq!(engine.editor().unwrap().text, "new dirty\n");
    assert_eq!(engine.editor().unwrap().saved_text, "old saved\n");
}

#[test]
fn imports_native_workspace_mode_flags_and_explicit_private_restore_retains_leaf_states() {
    let fixture = Fixture::new();
    let data = receipt();
    let final_state = &data["cases"].as_array().unwrap().last().unwrap()["after"];
    for (path, source) in final_state["saved"].as_object().unwrap() {
        std::fs::write(fixture.0.join(path), source.as_str().unwrap()).unwrap();
    }
    let mut engine = Engine::open(&fixture.0).unwrap();
    for (key, value) in final_state["config"].as_object().unwrap() {
        engine.set_config(key, value.clone()).unwrap();
    }
    engine.import_workspace(data["workspace"].clone()).unwrap();
    assert_eq!(
        engine.session().imported_workspace.as_ref(),
        Some(&data["workspace"])
    );
    assert_eq!(snapshot(&engine, &data["initial_files"]), *final_state);
    engine.persist_session().unwrap();
    let mut reopened = Engine::open(&fixture.0).unwrap();
    // A deliberate controller checkpoint uses restore. Normal startup loads
    // saved file contents and resets selection, as the separate quit fixture
    // verifies; it does not restore discarded hidden editor text.
    let checkpoint =
        serde_json::from_slice(&std::fs::read(fixture.0.join(".mnemosyne/session.json")).unwrap())
            .unwrap();
    reopened.restore(checkpoint).unwrap();
    assert_eq!(snapshot(&reopened, &data["initial_files"]), *final_state);
    assert_eq!(
        reopened.session().imported_workspace.as_ref(),
        Some(&data["workspace"])
    );
}
