use mnemosyne_core::CONFIG_DIR;
use mnemosyne_engine::{
    EditorPosition, EditorSelection, Engine, EngineError, FileTarget, OpenOptions,
};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "mnemosyne-engine-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join("folder")).unwrap();
        for (name, text) in [
            ("A.md", "saved A\n"),
            ("B.md", "saved B\n"),
            ("folder/B.md", "nested B\n"),
            ("Alias.md", "---\naliases: [Nick]\n---\nalias\n"),
            ("image.png", "image"),
        ] {
            std::fs::write(root.join(name), text).unwrap();
        }
        Self(root)
    }
    fn engine(&self) -> Engine {
        Engine::open(&self.0).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn target_resolution_distinguishes_exact_paths_link_targets_and_active_defaults() {
    let fixture = Fixture::new();
    let mut engine = fixture.engine();
    assert!(matches!(
        engine.resolve_target(FileTarget::default(), true),
        Err(EngineError::NoActiveFile)
    ));
    assert!(matches!(
        engine.open_file(FileTarget::default(), OpenOptions::default()),
        Err(EngineError::MissingTarget)
    ));
    assert!(matches!(
        engine.resolve_target(FileTarget::path("A"), true),
        Err(EngineError::FileNotFound(_))
    ));
    assert!(matches!(
        engine.resolve_target(FileTarget::path("folder"), true),
        Err(EngineError::Folder(_))
    ));
    assert!(matches!(
        engine.resolve_target(FileTarget::file("Nick"), true),
        Err(EngineError::FileNotFound(_))
    ));
    assert_eq!(
        engine.resolve_target(FileTarget::file("B"), true).unwrap(),
        "B.md"
    );
    assert_eq!(
        engine
            .resolve_target(
                FileTarget {
                    path: Some("A.md"),
                    file: Some("B")
                },
                true
            )
            .unwrap(),
        "A.md"
    );
    assert_eq!(
        engine
            .resolve_target(
                FileTarget {
                    path: Some(""),
                    file: Some("B")
                },
                true
            )
            .unwrap(),
        "B.md"
    );
    engine
        .open_file(FileTarget::path("A.md"), OpenOptions::default())
        .unwrap();
    assert_eq!(
        engine.read_saved(FileTarget::default()).unwrap(),
        "saved A\n"
    );
}

#[test]
fn opening_reuses_active_leaf_unless_new_tab_and_recents_are_unique_mru() {
    let fixture = Fixture::new();
    let mut engine = fixture.engine();
    engine
        .open_file(FileTarget::path("A.md"), OpenOptions::default())
        .unwrap();
    let first = engine.session().active_leaf.clone();
    engine
        .open_file(FileTarget::path("B.md"), OpenOptions { new_tab: true })
        .unwrap();
    assert_ne!(engine.session().active_leaf, first);
    assert_eq!(engine.session().leaves.len(), 2);
    engine
        .open_file(FileTarget::path("A.md"), OpenOptions::default())
        .unwrap();
    assert_eq!(engine.session().leaves.len(), 2);
    assert_eq!(engine.recents(), ["B.md", "A.md"]);
    engine
        .open_file(FileTarget::path("A.md"), OpenOptions::default())
        .unwrap();
    assert_eq!(engine.recents(), ["B.md", "A.md"]);
    for n in 0..30 {
        engine.collect_recent(&format!("Created{n}.md"));
    }
    assert_eq!(engine.recents().len(), 10);
    assert_eq!(engine.recents()[0], "Created29.md");
    // Tracker retains 25 previous Markdown entries plus the newest.
    assert_eq!(engine.session().last_open_files.len(), 26);
}

#[test]
fn dirty_buffers_survive_external_refresh_and_explicit_private_restore_then_save() {
    let fixture = Fixture::new();
    let mut engine = fixture.engine();
    engine
        .open_file(FileTarget::path("A.md"), OpenOptions::default())
        .unwrap();
    engine.editor_mut().unwrap().set_value("draft 😀\n".into());
    std::fs::write(fixture.0.join("A.md"), "external\n").unwrap();
    engine.refresh_vault().unwrap();
    assert_eq!(engine.editor().unwrap().text, "draft 😀\n");
    assert_eq!(
        engine.read_saved(FileTarget::default()).unwrap(),
        "external\n"
    );
    engine.persist_session().unwrap();
    let mut restored = fixture.engine();
    restored.restore(engine.session().clone()).unwrap();
    assert_eq!(restored.active_file(), Some("A.md"));
    assert_eq!(restored.editor().unwrap().text, "draft 😀\n");
    assert!(restored.editor().unwrap().is_dirty());
    restored.save_active().unwrap();
    assert_eq!(
        restored.read_saved(FileTarget::default()).unwrap(),
        "draft 😀\n"
    );
    assert!(!restored.editor().unwrap().is_dirty());
    assert_eq!(restored.vault.get("A.md").unwrap().source, "draft 😀\n");
}

#[test]
fn clean_buffers_follow_external_changes_without_changing_active_leaf() {
    let fixture = Fixture::new();
    let mut engine = fixture.engine();
    engine
        .open_file(FileTarget::path("A.md"), OpenOptions::default())
        .unwrap();
    let leaf = engine.session().active_leaf.clone();
    std::fs::write(fixture.0.join("A.md"), "changed\n").unwrap();
    engine.refresh_vault().unwrap();
    assert_eq!(engine.editor().unwrap().text, "changed\n");
    assert!(!engine.editor().unwrap().is_dirty());
    assert_eq!(engine.session().active_leaf, leaf);
}

#[test]
fn editor_utf16_selection_replaces_reversed_multiline_range() {
    let mut editor = mnemosyne_engine::EditorBuffer::new("a😀b\r\ncafé\n".into());
    editor
        .set_selection(EditorSelection {
            anchor: EditorPosition { line: 1, ch: 3 },
            head: EditorPosition { line: 0, ch: 1 },
        })
        .unwrap();
    assert_eq!(editor.selected_text().unwrap(), "😀b\ncaf");
    editor.replace_selection("λ\n😀").unwrap();
    assert_eq!(editor.text, "aλ\n😀é\n");
    assert_eq!(editor.selection.head, EditorPosition { line: 1, ch: 2 });
    assert!(editor
        .set_selection(EditorSelection {
            anchor: EditorPosition { line: 1, ch: 1 },
            head: EditorPosition { line: 1, ch: 2 },
        })
        .is_err());
}

#[test]
fn imports_observed_workspace_fields_and_preserves_unknown_payload() {
    let fixture = Fixture::new();
    std::fs::create_dir(fixture.0.join(CONFIG_DIR)).unwrap();
    let workspace = serde_json::json!({
        "main":{"id":"root","type":"split","children":[{"id":"tabs","type":"tabs","children":[
            {"id":"a","type":"leaf","state":{"type":"markdown","state":{"file":"A.md","mode":"source"}}},
            {"id":"b","type":"leaf","state":{"type":"markdown","state":{"file":"B.md"}}}
        ]}]},"active":"b","lastOpenFiles":["B.md","A.md"],"unknownPlugin":{"x":1}
    });
    std::fs::write(
        fixture.0.join(CONFIG_DIR).join("workspace.json"),
        workspace.to_string(),
    )
    .unwrap();
    let engine = fixture.engine();
    assert_eq!(engine.active_file(), Some("B.md"));
    assert_eq!(engine.editor().unwrap().text, "saved B\n");
    assert_eq!(engine.recents(), ["B.md", "A.md"]);
    assert_eq!(
        engine.session().imported_workspace.as_ref(),
        Some(&workspace)
    );
    engine.persist_session().unwrap();
    assert_eq!(fixture.engine().active_file(), Some("B.md"));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(
            &std::fs::read_to_string(fixture.0.join(CONFIG_DIR).join("workspace.json")).unwrap()
        )
        .unwrap(),
        workspace
    );
}

#[test]
fn malformed_persistent_state_does_not_replace_live_session() {
    let fixture = Fixture::new();
    let mut engine = fixture.engine();
    let mut state = engine.session().clone();
    state.leaves.push(state.leaves[0].clone());
    assert!(matches!(
        engine.restore(state),
        Err(EngineError::InvalidState(_))
    ));
    assert_eq!(engine.session().leaves.len(), 1);
    let mut state = engine.session().clone();
    state.leaves[0].file = Some("../outside.md".into());
    assert!(matches!(
        engine.restore(state),
        Err(EngineError::InvalidState(_))
    ));
    assert_eq!(engine.active_file(), None);
}

#[test]
fn deleted_active_file_returns_to_previous_note_and_does_not_resurrect_stale_buffer() {
    let fixture = Fixture::new();
    let mut engine = fixture.engine();
    engine
        .open_file(FileTarget::path("A.md"), OpenOptions::default())
        .unwrap();
    engine
        .open_file(FileTarget::path("B.md"), OpenOptions::default())
        .unwrap();
    engine.vault.delete("B.md").unwrap();
    engine.file_deleted("B.md");
    assert_eq!(engine.active_file(), Some("A.md"));
    assert_eq!(engine.editor().unwrap().text, "saved A\n");
    assert!(!engine.session().buffers.contains_key("B.md"));
    engine.vault.delete("A.md").unwrap();
    engine.file_deleted("A.md");
    assert_eq!(engine.active_file(), None);
    assert_eq!(engine.session().leaves[0].view_type, "empty");
    std::fs::write(fixture.0.join("B.md"), "new incarnation\n").unwrap();
    engine.refresh_vault().unwrap();
    assert_eq!(engine.active_file(), None);
    engine
        .open_file(FileTarget::path("B.md"), OpenOptions::default())
        .unwrap();
    assert_eq!(engine.editor().unwrap().text, "new incarnation\n");
}

#[test]
fn editor_clips_positions_and_normalizes_line_endings() {
    let mut editor = mnemosyne_engine::EditorBuffer::new("a\r\nb\rc".into());
    assert_eq!(editor.text, "a\nb\nc");
    assert!(!editor.is_dirty());
    editor
        .set_selection(EditorSelection {
            anchor: EditorPosition { line: -1, ch: 99 },
            head: EditorPosition { line: 99, ch: -1 },
        })
        .unwrap();
    assert_eq!(editor.selected_text().unwrap(), "a\nb\nc");
    assert_eq!(editor.selection.anchor, EditorPosition { line: 0, ch: 0 });
    assert_eq!(editor.selection.head, EditorPosition { line: 2, ch: 1 });
    editor.replace_selection("x\r\ny").unwrap();
    assert_eq!(editor.text, "x\ny");
}

#[test]
fn search_order_tracks_folder_child_creation_rename_and_move_without_resorting() {
    let fixture = Fixture::new();
    let mut engine = fixture.engine();
    assert_eq!(
        engine.ordered_note_paths(),
        ["folder/B.md", "B.md", "Alias.md", "A.md"]
    );
    let created = engine.vault.create("AA", "new\n").unwrap();
    engine.file_created(&created);
    assert_eq!(
        engine.ordered_note_paths(),
        ["AA.md", "folder/B.md", "B.md", "Alias.md", "A.md"]
    );
    engine.refresh_vault().unwrap();
    assert_eq!(engine.ordered_note_paths()[0], "AA.md");
    std::fs::rename(fixture.0.join("A.md"), fixture.0.join("Z.md")).unwrap();
    engine.file_renamed("A.md", "Z.md");
    engine.refresh_vault().unwrap();
    assert_eq!(
        engine.ordered_note_paths(),
        ["Z.md", "AA.md", "folder/B.md", "B.md", "Alias.md"]
    );
    std::fs::rename(fixture.0.join("Z.md"), fixture.0.join("folder/Z.md")).unwrap();
    engine.file_renamed("Z.md", "folder/Z.md");
    engine.refresh_vault().unwrap();
    assert_eq!(
        engine.ordered_note_paths(),
        ["AA.md", "folder/Z.md", "folder/B.md", "B.md", "Alias.md"]
    );
    engine.vault.delete("AA.md").unwrap();
    engine.file_deleted("AA.md");
    assert_eq!(
        engine.ordered_note_paths(),
        ["folder/Z.md", "folder/B.md", "B.md", "Alias.md"]
    );
    assert_eq!(
        engine
            .ordered_notes()
            .map(|note| note.path.to_string_lossy().into_owned())
            .collect::<Vec<_>>(),
        engine.ordered_note_paths()
    );
    // Restart reconstructs the disk tree; private session data does not claim
    // to preserve process-local vault object insertion order across restarts.
    let reopened = fixture.engine();
    assert_eq!(
        reopened.ordered_note_paths(),
        ["folder/Z.md", "folder/B.md", "B.md", "Alias.md"]
    );
}

#[test]
fn folder_rename_keeps_subtree_chronology_and_appends_to_new_parent() {
    let fixture = Fixture::new();
    let mut engine = fixture.engine();
    std::fs::write(fixture.0.join("folder/A.md"), "late\n").unwrap();
    engine.file_created("folder/A.md");
    engine.refresh_vault().unwrap();
    assert_eq!(engine.ordered_note_paths()[0], "folder/A.md");
    std::fs::rename(fixture.0.join("folder"), fixture.0.join("0folder")).unwrap();
    engine.file_renamed("folder", "0folder");
    engine.refresh_vault().unwrap();
    assert_eq!(
        engine.ordered_note_paths(),
        ["0folder/A.md", "0folder/B.md", "B.md", "Alias.md", "A.md"]
    );
    // Lexically sorting the newly renamed directory would put it before all
    // root notes and produce the opposite search order.
}

#[test]
fn replay_captured_session_outputs_and_nonvisual_state() {
    let evidence: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/reference_session.json")).unwrap();
    let fixture = Fixture::new();
    for (path, text) in evidence["initial_files"].as_object().unwrap() {
        let destination = fixture.0.join(path);
        std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
        std::fs::write(destination, text.as_str().unwrap()).unwrap();
    }
    let mut engine = fixture.engine();
    let mut state = engine.session().clone();
    state.last_open_files = evidence["initial_recents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap().into())
        .collect();
    engine.restore(state).unwrap();
    for step in evidence["steps"].as_array().unwrap() {
        if let Some(text) = step.get("editor_set_value") {
            engine
                .set_editor_value_at(text.as_str().unwrap().into(), 0)
                .unwrap();
            // This direct controller replay models the native fixture's
            // separate-command/eval observation boundary after the source's
            // 10ms quick-preview callback, before its 2-second saved write.
            // The CLI transport replay drives that callback on the real loop.
            engine.process_markdown_timers(10).unwrap();
            continue;
        }
        let args: Vec<&str> = step["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|arg| arg.as_str().unwrap())
            .collect();
        let target = FileTarget {
            path: args.iter().find_map(|arg| arg.strip_prefix("path=")),
            file: args.iter().find_map(|arg| arg.strip_prefix("file=")),
        };
        let output: Result<String, EngineError> = match args[0] {
            "read" => engine.read_saved(target),
            "open" => engine
                .open_file(
                    target,
                    OpenOptions {
                        new_tab: args.contains(&"newtab"),
                    },
                )
                .map(|path| format!("Opened: {path}")),
            "recents" if args.contains(&"total") => Ok(engine.recents().len().to_string()),
            "recents" => Ok(engine.recents().join("\n")),
            "tabs" => Ok(engine
                .session()
                .leaves
                .iter()
                .map(|leaf| {
                    let name = leaf
                        .file
                        .as_deref()
                        .unwrap_or("")
                        .rsplit('/')
                        .next()
                        .unwrap()
                        .strip_suffix(".md")
                        .unwrap();
                    format!("[{}] {name}", leaf.view_type)
                })
                .collect::<Vec<_>>()
                .join("\n")),
            other => panic!("unhandled reference fixture command {other}"),
        };
        let mut stdout = output.unwrap_or_else(|error| format!("Error: {error}"));
        if !stdout.ends_with('\n') {
            stdout.push('\n');
        }
        assert_eq!(
            stdout,
            step["expected_process"]["stdout"].as_str().unwrap(),
            "{args:?}"
        );
        assert_eq!(step["expected_process"]["exit_code"], 0);
        assert_eq!(step["expected_process"]["stderr"], "");
        let leaves: Vec<_> = engine.session().leaves.iter().map(|leaf|serde_json::json!({
            "type":leaf.view_type,"file":leaf.file,
            "text":engine.editor_for_leaf(&leaf.id).map(|buffer|&buffer.text)
        })).collect();
        let state = serde_json::json!({"active":engine.active_file(),"recents":engine.recents(),"leaves":leaves});
        assert_eq!(state, step["expected_state"], "{args:?}");
    }
}

#[test]
fn unchanged_crlf_file_refresh_preserves_editor_selection() {
    let fixture = Fixture::new();
    std::fs::write(fixture.0.join("A.md"), "first\r\nsecond\r\n").unwrap();
    let mut engine = fixture.engine();
    engine
        .open_file(FileTarget::path("A.md"), OpenOptions::default())
        .unwrap();
    engine
        .editor_mut()
        .unwrap()
        .set_selection(EditorSelection {
            anchor: EditorPosition { line: 1, ch: 0 },
            head: EditorPosition { line: 1, ch: 6 },
        })
        .unwrap();
    engine.refresh_vault().unwrap();
    assert_eq!(engine.editor().unwrap().selected_text().unwrap(), "second");
    assert_eq!(
        engine.read_saved(FileTarget::default()).unwrap(),
        "first\r\nsecond\r\n"
    );
    assert!(!engine.editor().unwrap().is_dirty());
}

#[test]
fn folder_rename_updates_active_buffers_histories_and_recent_descendants() {
    let fixture = Fixture::new();
    let mut engine = fixture.engine();
    engine
        .open_file(FileTarget::path("folder/B.md"), OpenOptions::default())
        .unwrap();
    engine
        .editor_mut()
        .unwrap()
        .set_value("draft nested\n".into());
    engine
        .open_file(FileTarget::path("A.md"), OpenOptions::default())
        .unwrap();
    engine
        .open_file(FileTarget::path("folder/B.md"), OpenOptions::default())
        .unwrap();
    std::fs::rename(fixture.0.join("folder"), fixture.0.join("renamed")).unwrap();
    engine.file_renamed("folder", "renamed");
    engine.refresh_vault().unwrap();
    assert_eq!(engine.active_file(), Some("renamed/B.md"));
    assert_eq!(engine.editor().unwrap().text, "draft nested\n");
    assert_eq!(engine.recents(), ["A.md", "renamed/B.md"]);
    assert_eq!(
        engine.session().leaves[0].back_history,
        ["renamed/B.md", "A.md"]
    );
}

#[test]
fn saved_metadata_cache_ignores_unsaved_editor_and_reuses_unchanged_entries() {
    let fixture = Fixture::new();
    std::fs::write(fixture.0.join("A.md"), "# Saved\n#saved\n").unwrap();
    let mut engine = fixture.engine();
    let unchanged = std::ptr::from_ref(engine.file_cache("B.md").unwrap());
    let original = std::ptr::from_ref(engine.file_cache("A.md").unwrap());
    engine
        .open_file(FileTarget::path("A.md"), OpenOptions::default())
        .unwrap();
    engine
        .editor_mut()
        .unwrap()
        .set_value("# Unsaved\n#unsaved\n".into());
    assert_eq!(
        engine.file_cache("A.md").unwrap().headings[0].heading,
        "Saved"
    );
    assert_eq!(engine.file_cache("A.md").unwrap().tags[0].tag, "#saved");
    engine.refresh_vault().unwrap();
    assert_eq!(
        std::ptr::from_ref(engine.file_cache("A.md").unwrap()),
        original
    );
    engine.save_active().unwrap();
    assert_eq!(
        engine.file_cache("A.md").unwrap().headings[0].heading,
        "Unsaved"
    );
    assert_eq!(engine.file_cache("A.md").unwrap().tags[0].tag, "#unsaved");
    assert_eq!(
        std::ptr::from_ref(engine.file_cache("B.md").unwrap()),
        unchanged
    );
    assert_eq!(
        serde_json::to_value(engine.file_cache("image.png").unwrap()).unwrap(),
        serde_json::json!({}),
        "actual non-Markdown cache contract is an empty object, not absence"
    );
    assert_eq!(
        engine
            .ordered_cached_notes()
            .map(|(note, _)| note.path.to_string_lossy().into_owned())
            .collect::<Vec<_>>(),
        engine.ordered_note_paths()
    );
}

#[test]
fn cache_invalidation_handles_repeated_mutations_create_rename_backlinks_and_delete() {
    let fixture = Fixture::new();
    std::fs::write(fixture.0.join("B.md"), "[[A]]\n").unwrap();
    let mut engine = fixture.engine();
    engine.vault.modify("A.md", "#one\n").unwrap();
    engine.vault.modify("A.md", "#two\n").unwrap();
    assert!(
        engine.file_cache("A.md").is_none(),
        "direct mutation must not expose stale metadata"
    );
    engine.refresh_vault().unwrap();
    assert_eq!(engine.file_cache("A.md").unwrap().tags[0].tag, "#two");
    engine.vault.modify("A.md", "#new\n").unwrap();
    engine.file_modified("A.md");
    assert_eq!(engine.file_cache("A.md").unwrap().tags[0].tag, "#new");
    let created = engine.vault.create("Created", "#created\n").unwrap();
    engine.file_created(&created);
    assert_eq!(engine.file_cache(&created).unwrap().tags[0].tag, "#created");
    engine.vault.rename("A.md", "Renamed.md").unwrap();
    engine.file_renamed("A.md", "Renamed.md");
    assert!(engine.file_cache("A.md").is_none());
    assert_eq!(engine.file_cache("Renamed.md").unwrap().tags[0].tag, "#new");
    assert_eq!(engine.file_cache("B.md").unwrap().links[0].link, "Renamed");
    engine.vault.delete("Renamed.md").unwrap();
    engine.file_deleted("Renamed.md");
    assert!(engine.file_cache("Renamed.md").is_none());
    assert!(engine
        .ordered_cached_notes()
        .all(|(note, _)| note.path.to_string_lossy() != "Renamed.md"));
}

#[test]
fn external_refresh_updates_cached_metadata_and_reloads_link_format_settings() {
    use mnemosyne_core::vault::LinkFormat;
    let fixture = Fixture::new();
    let config = fixture.0.join(CONFIG_DIR);
    std::fs::create_dir(&config).unwrap();
    let settings = config.join("app.json");
    std::fs::write(&settings, r#"{"newLinkFormat":"relative"}"#).unwrap();
    let mut engine = fixture.engine();
    assert_eq!(engine.vault.new_link_format, LinkFormat::Relative);
    let unchanged = std::ptr::from_ref(engine.file_cache("B.md").unwrap());
    std::fs::write(fixture.0.join("A.md"), "# External 😀\n#external\n").unwrap();
    std::fs::write(&settings, r#"{"newLinkFormat":"absolute"}"#).unwrap();
    engine.refresh_vault().unwrap();
    assert_eq!(engine.vault.new_link_format, LinkFormat::Absolute);
    assert_eq!(
        engine.file_cache("A.md").unwrap().headings[0].heading,
        "External 😀"
    );
    assert_eq!(engine.file_cache("A.md").unwrap().tags[0].tag, "#external");
    assert_eq!(
        std::ptr::from_ref(engine.file_cache("B.md").unwrap()),
        unchanged
    );
    std::fs::remove_file(&settings).unwrap();
    engine.refresh_vault().unwrap();
    assert_eq!(engine.vault.new_link_format, LinkFormat::Shortest);
    std::fs::remove_file(fixture.0.join("A.md")).unwrap();
    engine.refresh_vault().unwrap();
    assert!(engine.file_cache("A.md").is_none());
}

#[test]
fn app_config_exclusions_are_strict_json_prefix_or_regex_and_preserve_other_fields() {
    let fixture = Fixture::new();
    let config = fixture.0.join(CONFIG_DIR);
    std::fs::create_dir(&config).unwrap();
    let settings = config.join("app.json");
    std::fs::write(&settings,r#"{"userIgnoreFilters":["\ufeff Foo ","/bar\\.md$/","*.md","/[/"],"futureSetting":{"enabled":true}}"#).unwrap();
    let mut engine = fixture.engine();
    assert!(engine.is_user_ignored("FOObar.md"));
    assert!(engine.is_user_ignored("nested/BAR.md"));
    assert!(!engine.is_user_ignored("a.md"));
    assert!(engine.is_user_ignored("*.md-note"));
    assert!(engine
        .config_diagnostics()
        .iter()
        .any(|message| message.contains("Bad regex")));
    engine
        .set_config("userIgnoreFilters", serde_json::json!(["folder"]))
        .unwrap();
    assert!(engine.is_user_ignored("Folderish.md"));
    assert!(!engine.is_user_ignored("FOObar.md"));
    let persisted: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(settings).unwrap()).unwrap();
    assert_eq!(
        persisted["futureSetting"],
        serde_json::json!({"enabled":true})
    );
    assert_eq!(
        fixture.engine().get_config("userIgnoreFilters"),
        &serde_json::json!(["folder"])
    );
}

#[test]
fn config_refresh_updates_exclusions_and_malformed_entries_do_not_silently_compile_suffix() {
    let fixture = Fixture::new();
    let config = fixture.0.join(CONFIG_DIR);
    std::fs::create_dir(&config).unwrap();
    let settings = config.join("app.json");
    std::fs::write(&settings, "userIgnoreFilters:\n - Foo\n").unwrap();
    let mut engine = fixture.engine();
    assert_eq!(
        engine.get_config("userIgnoreFilters"),
        &serde_json::Value::Null
    );
    assert!(!engine.is_user_ignored("Foo.md"));
    assert!(engine
        .config_diagnostics()
        .iter()
        .any(|message| message.contains("failed to read JSON")));
    std::fs::write(&settings, r#"{"userIgnoreFilters":["Foo",false,"Bar"]}"#).unwrap();
    engine.refresh_vault().unwrap();
    assert!(engine.is_user_ignored("Foo.md"));
    assert!(!engine.is_user_ignored("Bar.md"));
    assert!(engine
        .config_diagnostics()
        .iter()
        .any(|message| message.contains("trim method")));
    std::fs::remove_file(settings).unwrap();
    engine.refresh_vault().unwrap();
    assert!(!engine.is_user_ignored("Foo.md"));
}

#[test]
fn observed_task_status_patch_preserves_clean_cursor_and_unrelated_dirty_editor_text() {
    let fixture = Fixture::new();
    std::fs::write(fixture.0.join("A.md"), "- [ ] todo\n- [x] retained\n").unwrap();
    let mut engine = fixture.engine();
    engine
        .open_file(FileTarget::path("A.md"), OpenOptions::default())
        .unwrap();
    let cursor = EditorPosition { line: 1, ch: 4 };
    engine
        .editor_mut()
        .unwrap()
        .set_selection(EditorSelection {
            anchor: cursor,
            head: cursor,
        })
        .unwrap();
    engine
        .vault
        .modify("A.md", "- [x] todo\n- [x] retained\n")
        .unwrap();
    engine.file_modified("A.md");
    engine
        .patch_editor_task_status("A.md", EditorPosition { line: 0, ch: 3 }, "x")
        .unwrap();
    assert_eq!(
        engine.editor().unwrap().text,
        "- [x] todo\n- [x] retained\n"
    );
    assert_eq!(engine.editor().unwrap().selection.head, cursor);
    assert!(!engine.editor().unwrap().is_dirty());
    engine
        .editor_mut()
        .unwrap()
        .set_value("- [x] unsaved edit\n- [x] retained\n".into());
    engine
        .vault
        .modify("A.md", "- [ ] todo\n- [x] retained\n")
        .unwrap();
    engine.file_modified("A.md");
    engine
        .patch_editor_task_status("A.md", EditorPosition { line: 0, ch: 3 }, " ")
        .unwrap();
    assert_eq!(
        engine.editor().unwrap().text,
        "- [ ] unsaved edit\n- [x] retained\n"
    );
    assert!(engine.editor().unwrap().is_dirty());
    assert_eq!(
        engine.editor().unwrap().selection.head,
        EditorPosition { line: 0, ch: 0 }
    );
    assert_eq!(
        engine.read_saved(FileTarget::default()).unwrap(),
        "- [ ] todo\n- [x] retained\n"
    );
    engine
        .editor_mut()
        .unwrap()
        .set_value("unrelated paragraph\n".into());
    assert!(matches!(
        engine.patch_editor_task_status("A.md", EditorPosition { line: 0, ch: 3 }, "x"),
        Err(EngineError::MissingTaskEditorMarker(_, _))
    ));
    assert_eq!(engine.editor().unwrap().text, "unrelated paragraph\n");
}

#[test]
fn opened_editor_removes_one_bom_and_saved_read_preserves_it() {
    let fixture = Fixture::new();
    std::fs::write(fixture.0.join("A.md"), "\u{feff}- [ ] first\n").unwrap();
    let mut engine = fixture.engine();
    engine
        .open_file(FileTarget::path("A.md"), OpenOptions::default())
        .unwrap();
    assert_eq!(engine.editor().unwrap().text, "- [ ] first\n");
    assert_eq!(
        engine.read_saved(FileTarget::default()).unwrap(),
        "\u{feff}- [ ] first\n"
    );
    let cursor = EditorPosition { line: 0, ch: 5 };
    engine
        .editor_mut()
        .unwrap()
        .set_selection(EditorSelection {
            anchor: cursor,
            head: cursor,
        })
        .unwrap();
    engine.refresh_vault().unwrap();
    assert_eq!(engine.editor().unwrap().selection.head, cursor);
    let buffer = mnemosyne_engine::EditorBuffer::new("\u{feff}\u{feff}x".into());
    assert_eq!(buffer.text, "\u{feff}x");
}

#[test]
fn internal_active_locale_selection_persists_and_unknown_id_retains_previous() {
    let fixture = Fixture::new();
    let mut engine = fixture.engine();
    assert_eq!(engine.locale(), "en");
    assert_eq!(engine.set_locale("FR"), "fr");
    assert_eq!(engine.set_locale("missing-locale"), "fr");
    assert_eq!(engine.set_locale("en_GB"), "en-gb");
    engine.reload().unwrap();
    assert_eq!(engine.locale(), "en-gb");
}

#[test]
fn source_file_and_empty_folder_inventory_reuses_live_reverse_traversal() {
    let fixture = Fixture::new();
    std::fs::create_dir_all(fixture.0.join("Empty/Sub")).unwrap();
    let mut engine = fixture.engine();
    assert_eq!(
        engine.ordered_file_paths(),
        ["image.png", "folder/B.md", "B.md", "Alias.md", "A.md"]
    );
    assert_eq!(
        engine.ordered_folder_paths(),
        ["/", "folder", "Empty", "Empty/Sub"]
    );
    std::fs::create_dir(fixture.0.join("NewEmpty")).unwrap();
    engine.file_created("NewEmpty");
    assert_eq!(
        engine.ordered_folder_paths(),
        ["/", "NewEmpty", "folder", "Empty", "Empty/Sub"]
    );
    std::fs::write(fixture.0.join("NewEmpty/image.dat"), b"bytes").unwrap();
    engine.file_created("NewEmpty/image.dat");
    assert_eq!(engine.ordered_file_paths()[0], "NewEmpty/image.dat");
    std::fs::remove_dir_all(fixture.0.join("NewEmpty")).unwrap();
    engine.file_deleted("NewEmpty");
    assert!(!engine.ordered_file_paths().contains(&"NewEmpty/image.dat"));
    assert!(!engine.ordered_folder_paths().contains(&"NewEmpty"));
}
