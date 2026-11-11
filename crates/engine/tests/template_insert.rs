use mnemosyne_core::CONFIG_DIR;
use mnemosyne_engine::{
    templates::TemplateInsertOutcome, EditorPosition, EditorSelection, Engine, FileTarget,
    OpenOptions,
};
use serde_json::Value;
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "mnemo-insert-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(path.join(CONFIG_DIR)).unwrap();
        std::fs::create_dir_all(path.join("Templates")).unwrap();
        std::fs::write(
            path.join(CONFIG_DIR).join("templates.json"),
            r#"{"folder":"Templates","dateFormat":"[DATE]","timeFormat":"[TIME]"}"#,
        )
        .unwrap();
        Self(path)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn position(value: &Value) -> EditorPosition {
    EditorPosition {
        line: value["line"].as_i64().unwrap(),
        ch: value["ch"].as_i64().unwrap(),
    }
}
fn expected_bytes(case: &Value) -> Vec<u8> {
    case["post_eval_file_bytes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|byte| byte.as_u64().unwrap() as u8)
        .collect()
}
fn assert_events(engine: &Engine, cursor: u64, expected: &Value, id: &str) {
    let actual = engine
        .event_journal_since(cursor)
        .unwrap()
        .iter()
        .map(|event| serde_json::json!({"name":event.name,"path":event.path,"kind":event.kind}))
        .collect::<Vec<_>>();
    let expected = expected["events"].as_array().unwrap().iter().map(|event| serde_json::json!({"name":event["name"],"path":event["path"],"kind":event["kind"]})).collect::<Vec<_>>();
    assert_eq!(actual, expected, "{id}: vault event journal");
}
fn assert_state(engine: &Engine, expected: &Value, id: &str) {
    let editor = engine.editor().unwrap();
    let state = engine.markdown_state().unwrap();
    assert_eq!(
        editor.text,
        expected["editor"].as_str().unwrap(),
        "{id}: editor"
    );
    assert_eq!(
        editor.selection.anchor,
        position(&expected["anchor"]),
        "{id}: anchor"
    );
    assert_eq!(
        editor.selection.head,
        position(&expected["head"]),
        "{id}: head"
    );
    assert_eq!(
        engine.markdown_view_data().unwrap(),
        expected["viewData"].as_str().unwrap(),
        "{id}: viewData"
    );
    assert_eq!(
        state.reading_data(),
        expected["data"].as_str().unwrap(),
        "{id}: data"
    );
    assert_eq!(
        state.last_saved_data(),
        expected["lastSavedData"].as_str().unwrap(),
        "{id}: lastSavedData"
    );
    assert_eq!(
        editor.edit_revision != state.clean_revision(),
        expected["dirty"].as_bool().unwrap(),
        "{id}: dirty"
    );
}
#[test]
fn replay_native_template_insertion() {
    let evidence: Value =
        serde_json::from_str(include_str!("fixtures/template_insert.json")).unwrap();
    for case in evidence["cases"].as_array().unwrap() {
        let input = &case["input"];
        let expected = &case["result"];
        let id = input["id"].as_str().unwrap();
        let fixture = Fixture::new();
        let path = input["path"].as_str().unwrap();
        let template_path = input["template_path"].as_str().unwrap();
        std::fs::write(fixture.0.join(path), input["note"].as_str().unwrap()).unwrap();
        std::fs::write(
            fixture.0.join(template_path),
            input["template"].as_str().unwrap(),
        )
        .unwrap();
        let mut engine = Engine::open(&fixture.0).unwrap();
        if input["empty"].as_bool() != Some(true) {
            engine
                .open_file(
                    FileTarget {
                        path: Some(path),
                        file: None,
                    },
                    OpenOptions::default(),
                )
                .unwrap();
        }
        if let Some(dirty) = input["dirty"].as_str() {
            engine.set_editor_value_at(dirty.to_owned(), 0).unwrap();
            if input["presave"].as_bool() == Some(true) {
                engine.save_active().unwrap();
            }
        }
        if input["mode"].as_str() == Some("preview") {
            engine
                .set_markdown_state(&serde_json::json!({"mode":"preview"}))
                .unwrap();
        }
        if input["empty"].as_bool() != Some(true) {
            let anchor = EditorPosition {
                line: input["line"].as_i64().unwrap(),
                ch: input["ch"].as_i64().unwrap(),
            };
            let head = EditorPosition {
                line: anchor.line,
                ch: input["head_ch"].as_i64().unwrap_or(anchor.ch),
            };
            engine
                .editor_mut()
                .unwrap()
                .set_selection(EditorSelection { anchor, head })
                .unwrap();
        }
        let cursor = engine.start_event_journal();
        // Inputs, never post-operation oracle data, establish the retained state.
        if input["empty"].as_bool() == Some(true) {
            assert_eq!(
                engine
                    .insert_template_file_at(template_path, time::OffsetDateTime::UNIX_EPOCH, 0)
                    .unwrap(),
                TemplateInsertOutcome::NoActiveEditor
            );
            assert!(engine.editor().is_none());
            assert_events(&engine, cursor, expected, id);
            assert_eq!(
                std::fs::read(fixture.0.join(path)).unwrap(),
                expected_bytes(case)
            );
            continue;
        }
        assert_state(&engine, &expected["before"], id);
        let outcome = engine
            .insert_template_file_at(template_path, time::OffsetDateTime::UNIX_EPOCH, 0)
            .unwrap();
        if expected["before"]["active_editor"].as_bool() == Some(false) {
            assert_eq!(outcome, TemplateInsertOutcome::NoActiveEditor, "{id}");
        }
        assert_state(&engine, &expected["immediate"], id);
        let immediate_bytes = if input["presave"].as_bool() == Some(true) {
            input["dirty"].as_str().unwrap()
        } else {
            input["note"].as_str().unwrap()
        };
        assert_eq!(
            std::fs::read(fixture.0.join(path)).unwrap(),
            immediate_bytes.as_bytes(),
            "{id}: immediate disk"
        );
        assert!(
            engine.event_journal_since(cursor).unwrap().is_empty(),
            "{id}: immediate events"
        );
        // Native readiness waits for its adapter callback / autosave. Candidate
        // time0 is the next loop's queued IO; 10/2000 are editor deadlines.
        engine.process_markdown_timers(0).unwrap();
        engine.process_markdown_timers(10).unwrap();
        engine.process_markdown_timers(2000).unwrap();
        assert_state(&engine, &expected["settled"], id);
        assert_events(&engine, cursor, expected, id);
        assert_eq!(
            std::fs::read(fixture.0.join(path)).unwrap(),
            expected_bytes(case),
            "{id}: file bytes"
        );
    }
}
#[test]
fn configured_insert_has_explicit_no_editor_and_invalid_properties_outcomes() {
    let fixture = Fixture::new();
    std::fs::write(fixture.0.join("Target.md"), "body\n").unwrap();
    std::fs::write(
        fixture.0.join("Templates/Invalid.md"),
        "---\nbad: [\n---\nbody\n",
    )
    .unwrap();
    let mut engine = Engine::open(&fixture.0).unwrap();
    assert_eq!(
        engine.insert_named_template("Invalid").unwrap(),
        TemplateInsertOutcome::NoActiveEditor
    );
    engine
        .open_file(
            FileTarget {
                path: Some("Target.md"),
                file: None,
            },
            OpenOptions::default(),
        )
        .unwrap();
    engine
        .editor_mut()
        .unwrap()
        .set_selection(EditorSelection {
            anchor: EditorPosition { line: 0, ch: 2 },
            head: EditorPosition { line: 0, ch: 2 },
        })
        .unwrap();
    assert_eq!(
        engine.insert_named_template("Invalid").unwrap(),
        TemplateInsertOutcome::InvalidProperties
    );
    assert_eq!(engine.editor().unwrap().text, "body\n");
    assert_eq!(
        std::fs::read_to_string(fixture.0.join("Target.md")).unwrap(),
        "body\n"
    );
}

fn opened_plain_fixture() -> (Fixture, Engine) {
    let fixture = Fixture::new();
    std::fs::write(fixture.0.join("Target.md"), "body\n").unwrap();
    std::fs::write(fixture.0.join("Templates/Plain.md"), "X").unwrap();
    let mut engine = Engine::open(&fixture.0).unwrap();
    engine
        .open_file(
            FileTarget {
                path: Some("Target.md"),
                file: None,
            },
            OpenOptions::default(),
        )
        .unwrap();
    let cursor = EditorPosition { line: 0, ch: 2 };
    engine
        .editor_mut()
        .unwrap()
        .set_selection(EditorSelection {
            anchor: cursor,
            head: cursor,
        })
        .unwrap();
    (fixture, engine)
}
#[test]
fn deferred_save_prefix_and_same_leaf_save_again_capture_latest() {
    let (fixture, mut engine) = opened_plain_fixture();
    let cursor = engine.start_event_journal();
    for _ in 0..3 {
        engine
            .insert_named_template_at("Plain", time::OffsetDateTime::UNIX_EPOCH, 0)
            .unwrap();
    }
    assert_eq!(
        engine.markdown_state().unwrap().last_saved_data(),
        "boXdy\n"
    );
    assert_eq!(engine.markdown_view_data().unwrap(), "boXXXdy\n");
    assert_eq!(
        std::fs::read(fixture.0.join("Target.md")).unwrap(),
        b"body\n"
    );
    assert!(engine.event_journal_since(cursor).unwrap().is_empty());
    engine.process_markdown_timers(0).unwrap();
    assert_eq!(
        std::fs::read(fixture.0.join("Target.md")).unwrap(),
        b"boXXXdy\n"
    );
    assert_eq!(engine.editor().unwrap().text, "boXXXdy\n");
    assert_eq!(
        engine.editor().unwrap().selection.head,
        EditorPosition { line: 0, ch: 5 }
    );
    assert_eq!(engine.event_journal_since(cursor).unwrap().len(), 2);
    engine.process_markdown_timers(2000).unwrap();
    assert_eq!(engine.event_journal_since(cursor).unwrap().len(), 2);
}
#[test]
fn synchronous_save_shutdown_and_reload_drain_template_continuation() {
    for operation in ["save", "shutdown", "reload"] {
        let (fixture, mut engine) = opened_plain_fixture();
        let cursor = engine.start_event_journal();
        engine
            .insert_named_template_at("Plain", time::OffsetDateTime::UNIX_EPOCH, 0)
            .unwrap();
        match operation {
            "save" => engine.save_active().unwrap(),
            "shutdown" => engine.prepare_shutdown().unwrap(),
            "reload" => engine.reload().unwrap(),
            _ => unreachable!(),
        }
        assert_eq!(
            std::fs::read(fixture.0.join("Target.md")).unwrap(),
            b"boXdy\n",
            "{operation}"
        );
        if operation == "reload" {
            assert!(engine.event_journal_since(cursor).is_err());
            let cursor = engine.start_event_journal();
            engine.process_markdown_timers(2000).unwrap();
            assert!(engine.event_journal_since(cursor).unwrap().is_empty());
        } else {
            assert_eq!(
                engine.event_journal_since(cursor).unwrap().len(),
                1,
                "{operation}"
            );
            engine.process_markdown_timers(2000).unwrap();
            assert_eq!(
                engine.event_journal_since(cursor).unwrap().len(),
                1,
                "{operation}"
            );
        }
    }
}
#[test]
fn deferred_failure_rolls_back_saved_prefix_and_reports_diagnostic() {
    let (fixture, mut engine) = opened_plain_fixture();
    let cursor = engine.start_event_journal();
    engine
        .insert_named_template_at("Plain", time::OffsetDateTime::UNIX_EPOCH, 0)
        .unwrap();
    std::fs::remove_file(fixture.0.join("Target.md")).unwrap();
    std::fs::create_dir(fixture.0.join("Target.md")).unwrap();
    engine.poll_editor_autosave();
    assert_eq!(engine.markdown_state().unwrap().last_saved_data(), "body\n");
    assert_eq!(engine.markdown_state().unwrap().reading_data(), "boXdy\n");
    assert!(engine.event_journal_since(cursor).unwrap().is_empty());
    assert!(engine
        .config_diagnostics()
        .iter()
        .any(|message| message.starts_with("Markdown template save:")));
}
#[test]
fn older_session_missing_property_owner_uses_retained_data() {
    let (fixture, mut engine) = opened_plain_fixture();
    std::fs::write(fixture.0.join("Target.md"), "---\nkeep: old\n---\nbody\n").unwrap();
    engine.reload().unwrap();
    let mut snapshot = serde_json::to_value(engine.session()).unwrap();
    for leaf in snapshot["leaves"].as_array_mut().unwrap() {
        if let Some(markdown) = leaf["markdown"].as_object_mut() {
            markdown.remove("template_properties_data");
        }
    }
    engine
        .restore(serde_json::from_value(snapshot).unwrap())
        .unwrap();
    let cursor = EditorPosition { line: 3, ch: 2 };
    engine
        .editor_mut()
        .unwrap()
        .set_selection(EditorSelection {
            anchor: cursor,
            head: cursor,
        })
        .unwrap();
    engine
        .insert_named_template_at("Plain", time::OffsetDateTime::UNIX_EPOCH, 0)
        .unwrap();
    assert_eq!(
        engine.markdown_view_data().unwrap(),
        "---\nkeep: old\n---\nboXdy\n"
    );
}
#[test]
fn pending_same_file_peers_keep_their_editor_prefix_during_older_completion() {
    let (fixture, mut engine) = opened_plain_fixture();
    let cursor = engine.start_event_journal();
    engine
        .insert_named_template_at("Plain", time::OffsetDateTime::UNIX_EPOCH, 0)
        .unwrap();
    let first_leaf = engine.session().active_leaf.clone().unwrap();
    engine
        .open_file(
            FileTarget {
                path: Some("Target.md"),
                file: None,
            },
            OpenOptions { new_tab: true },
        )
        .unwrap();
    let position = EditorPosition { line: 0, ch: 1 };
    engine
        .editor_mut()
        .unwrap()
        .set_selection(EditorSelection {
            anchor: position,
            head: position,
        })
        .unwrap();
    engine
        .insert_named_template_at("Plain", time::OffsetDateTime::UNIX_EPOCH, 0)
        .unwrap();
    engine.process_markdown_timers(0).unwrap();
    assert_eq!(
        std::fs::read(fixture.0.join("Target.md")).unwrap(),
        b"bXody\n"
    );
    assert_eq!(engine.editor().unwrap().text, "bXody\n");
    assert_eq!(
        engine.editor().unwrap().selection.head,
        EditorPosition { line: 0, ch: 2 }
    );
    assert_eq!(engine.editor_for_leaf(&first_leaf).unwrap().text, "bXody\n");
    assert_eq!(engine.event_journal_since(cursor).unwrap().len(), 2);
}
#[test]
fn saved_content_load_updates_property_owner_in_source_and_preview() {
    for mode in ["source", "preview"] {
        let (fixture, mut engine) = opened_plain_fixture();
        if mode == "preview" {
            engine
                .set_markdown_state(&serde_json::json!({"mode":"preview"}))
                .unwrap();
        }
        engine
            .vault
            .modify("Target.md", "---\nkeep: loaded\n---\nbody\n")
            .unwrap();
        engine.file_modified("Target.md");
        if mode == "source" {
            let position = EditorPosition { line: 3, ch: 2 };
            engine
                .editor_mut()
                .unwrap()
                .set_selection(EditorSelection {
                    anchor: position,
                    head: position,
                })
                .unwrap();
        }
        engine
            .insert_named_template_at("Plain", time::OffsetDateTime::UNIX_EPOCH, 0)
            .unwrap();
        engine.process_markdown_timers(0).unwrap();
        let expected = if mode == "source" {
            "---\nkeep: loaded\n---\nboXdy\n"
        } else {
            "---\nkeep: loaded\n---\nbody\n"
        };
        assert_eq!(
            std::fs::read(fixture.0.join("Target.md")).unwrap(),
            expected.as_bytes(),
            "{mode}"
        );
    }
}
#[test]
fn explicit_template_view_load_retains_sequential_properties() {
    let (fixture, mut engine) = opened_plain_fixture();
    engine
        .vault
        .create("Templates/First.md", "---\nfirst: one\n---\nX")
        .unwrap();
    engine
        .vault
        .create("Templates/Second.md", "---\nsecond: two\n---\nY")
        .unwrap();
    engine
        .insert_named_template_at("First", time::OffsetDateTime::UNIX_EPOCH, 0)
        .unwrap();
    // Keep the second operation on the non-start metadata-insertion branch,
    // independently of the editor's whole-line mapping after adding the block.
    let position = EditorPosition { line: 3, ch: 3 };
    engine
        .editor_mut()
        .unwrap()
        .set_selection(EditorSelection {
            anchor: position,
            head: position,
        })
        .unwrap();
    engine
        .insert_named_template_at("Second", time::OffsetDateTime::UNIX_EPOCH, 0)
        .unwrap();
    engine.process_markdown_timers(0).unwrap();
    assert_eq!(
        std::fs::read(fixture.0.join("Target.md")).unwrap(),
        b"---\nfirst: one\nsecond: two\n---\nboXYdy\n"
    );
}
#[test]
fn existing_null_owner_accepts_valid_template_properties() {
    let (fixture, mut engine) = opened_plain_fixture();
    engine
        .vault
        .modify("Target.md", "---\nnull\n---\nbody\n")
        .unwrap();
    engine.file_modified("Target.md");
    let position = EditorPosition { line: 3, ch: 2 };
    engine
        .editor_mut()
        .unwrap()
        .set_selection(EditorSelection {
            anchor: position,
            head: position,
        })
        .unwrap();
    engine
        .vault
        .create("Templates/Valid.md", "---\nnew: value\n---\nX")
        .unwrap();
    engine
        .insert_named_template_at("Valid", time::OffsetDateTime::UNIX_EPOCH, 0)
        .unwrap();
    engine.process_markdown_timers(0).unwrap();
    assert_eq!(
        std::fs::read(fixture.0.join("Target.md")).unwrap(),
        b"---\nnew: value\n---\nboXdy\n"
    );
}
#[test]
fn equal_internal_data_after_presave_keeps_stale_property_owner() {
    let (fixture, mut engine) = opened_plain_fixture();
    engine
        .vault
        .modify("Target.md", "---\nkeep: old\n---\nbody\n")
        .unwrap();
    engine.file_modified("Target.md");
    engine
        .set_editor_value_at("---\nkeep: dirty\n---\nbody\n".to_owned(), 0)
        .unwrap();
    engine.save_active().unwrap();
    engine.process_markdown_timers(10).unwrap();
    let position = EditorPosition { line: 3, ch: 2 };
    engine
        .editor_mut()
        .unwrap()
        .set_selection(EditorSelection {
            anchor: position,
            head: position,
        })
        .unwrap();
    engine
        .vault
        .create("Templates/Valid.md", "---\nnew: value\n---\nX")
        .unwrap();
    engine
        .insert_named_template_at("Valid", time::OffsetDateTime::UNIX_EPOCH, 20)
        .unwrap();
    engine.process_markdown_timers(20).unwrap();
    assert_eq!(
        std::fs::read(fixture.0.join("Target.md")).unwrap(),
        b"---\nkeep: old\nnew: value\n---\nboXdy\n"
    );
}
