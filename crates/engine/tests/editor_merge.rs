use mnemosyne_engine::{EditorPosition, EditorSelection, Engine, FileTarget, OpenOptions};
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(std::path::PathBuf);
impl Fixture {
    fn new(source: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "mnemosyne-editor-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("Note.md"), source).unwrap();
        Self(root)
    }
    fn engine(&self) -> Engine {
        let mut e = Engine::open(&self.0).unwrap();
        e.open_file(FileTarget::path("Note.md"), OpenOptions::default())
            .unwrap();
        e
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn position(value: &Value) -> EditorPosition {
    serde_json::from_value(value.clone()).unwrap()
}
#[test]
fn actual_saved_append_prepend_replay_clean_dirty_text_selection_and_disk_separation() {
    let report: Value =
        serde_json::from_str(include_str!("fixtures/reference_editor_external.json")).unwrap();
    for row in report["cases"].as_array().unwrap() {
        let fixture = Fixture::new(row["old_saved"].as_str().unwrap());
        let mut engine = fixture.engine();
        let before = &row["before"];
        engine
            .editor_mut()
            .unwrap()
            .set_value(before["text"].as_str().unwrap().into());
        engine
            .editor_mut()
            .unwrap()
            .set_selection(EditorSelection {
                anchor: position(&before["anchor"]),
                head: position(&before["head"]),
            })
            .unwrap();
        let saved = row["new_saved"].as_str().unwrap();
        engine.vault.modify("Note.md", saved).unwrap();
        engine.file_modified("Note.md");
        let editor = engine.editor().unwrap();
        let after = &row["after"];
        assert_eq!(
            editor.text,
            after["text"].as_str().unwrap(),
            "{}",
            row["id"]
        );
        assert_eq!(
            editor.selection.anchor,
            position(&after["anchor"]),
            "{}",
            row["id"]
        );
        assert_eq!(
            editor.selection.head,
            position(&after["head"]),
            "{}",
            row["id"]
        );
        assert_eq!(engine.read_saved(FileTarget::default()).unwrap(), saved);
    }
}
#[test]
fn saved_reference_edits_preserve_retained_view_and_map_cursor_inside_replacement() {
    let fixture = Fixture::new("[[Known]]\nretained\n");
    let mut engine = fixture.engine();
    engine
        .editor_mut()
        .unwrap()
        .set_selection(EditorSelection {
            anchor: EditorPosition { line: 0, ch: 3 },
            head: EditorPosition { line: 0, ch: 3 },
        })
        .unwrap();
    engine
        .apply_saved_source_edits(
            "Note.md",
            "[[Known]]\nretained\n",
            &[mnemosyne_core::rewrite::TextEdit {
                start: 2,
                end: 7,
                text: "New".into(),
            }],
        )
        .unwrap();
    assert_eq!(engine.editor().unwrap().text, "[[New]]\nretained\n");
    assert_eq!(
        engine.editor().unwrap().selection.head,
        EditorPosition { line: 0, ch: 2 }
    );
    engine.vault.rename("Note.md", "Note.txt").unwrap();
    engine.file_renamed("Note.md", "Note.txt");
    assert_eq!(engine.session().leaves[0].view_type, "markdown");
    assert!(engine.editor().is_some());
    engine.vault.rename("Note.txt", ".hidden").unwrap();
    engine.file_renamed("Note.txt", ".hidden");
    assert_eq!(engine.active_file(), None);
    assert!(engine.editor().is_none());
}
#[test]
fn removing_trailing_lines_maps_cursor_to_new_document_end_without_panicking() {
    let fixture = Fixture::new("a\nb\nc");
    let mut engine = fixture.engine();
    engine
        .editor_mut()
        .unwrap()
        .set_selection(EditorSelection {
            anchor: EditorPosition { line: 2, ch: 1 },
            head: EditorPosition { line: 2, ch: 1 },
        })
        .unwrap();
    engine.saved_content_modified("Note.md", "a").unwrap();
    assert_eq!(engine.editor().unwrap().text, "a");
    assert_eq!(
        engine.editor().unwrap().selection.head,
        EditorPosition { line: 0, ch: 1 }
    );
}
