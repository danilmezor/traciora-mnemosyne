use mnemosyne_engine::Engine;
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(std::path::PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
#[test]
fn actual_metadata_file_source_chronology_preserves_modify_and_appends_rename() {
    let report: Value =
        serde_json::from_str(include_str!("fixtures/reference_cache_chronology.json")).unwrap();
    let first = &report["phases"]["initial"];
    let root = std::env::temp_dir().join(format!(
        "mnemosyne-cacheorder-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&root).unwrap();
    let fixture = Fixture(root);
    for (path, text) in report["initial_files"].as_object().unwrap() {
        let full = fixture.0.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        let bytes = if let Some(text) = text.as_str() {
            text.as_bytes().to_vec()
        } else {
            serde_json::from_value::<Vec<u8>>(text.clone()).unwrap()
        };
        std::fs::write(full, bytes).unwrap();
    }
    let mut engine = Engine::open(&fixture.0).unwrap();
    let order = first["file_cache_order"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    engine.import_cache_order(&order).unwrap();
    for phase in [
        "initial",
        "created",
        "modified",
        "renamed-source",
        "renamed-target",
        "external-modified",
        "deleted-source",
        "all-self",
        "final",
    ] {
        match phase {
            "created" => {
                engine
                    .vault
                    .create("Created.md", report["created_source"].as_str().unwrap())
                    .unwrap();
                engine.file_created("Created.md");
            }
            "modified" | "external-modified" => {
                for (path, text) in report["modified_sources"][phase].as_object().unwrap() {
                    if engine.vault.notes.contains_key(path) {
                        engine.vault.modify(path, text.as_str().unwrap()).unwrap();
                        engine.file_modified(path);
                    }
                }
            }
            "renamed-source" => {
                engine
                    .vault
                    .rename("Folder/Source.md", "Folder/Renamed.md")
                    .unwrap();
                engine.file_renamed("Folder/Source.md", "Folder/Renamed.md");
            }
            "renamed-target" => {
                engine.vault.rename("Target.md", "Renamed.md").unwrap();
                engine.file_renamed("Target.md", "Renamed.md");
            }
            "deleted-source" => {
                engine.vault.delete("Created.md").unwrap();
                engine.file_deleted("Created.md");
            }
            "all-self" => {
                for path in ["image.png", "Unused.bin"] {
                    engine.vault.delete(path).unwrap();
                    engine.file_deleted(path);
                }
            }
            _ => {}
        }
        let expected = report["phases"][phase]["file_cache_order"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(engine.cached_file_paths(), expected, "{phase}");
        let expected = report["phases"][phase]["resolved_order"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(engine.resolved_link_paths(), expected, "{phase}");
    }
    // Authored source control, not a captured reload phase: _preload at UTF-8
    // byte 1761014 builds a fresh fileCache from IDB getAllKeys/getAll order.
    // Its persisted scalar filename keys are sorted, rather than retaining the
    // insertion chronology of the preceding native mutation endpoint.
    let saved: Value = serde_json::from_slice(
        &std::fs::read(fixture.0.join(".mnemosyne/metadata-cache.json")).unwrap(),
    )
    .unwrap();
    let mut preload_order = saved["files"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    preload_order.sort_by_cached_key(|key| key.encode_utf16().collect::<Vec<_>>());
    engine.reload().unwrap();
    assert_eq!(
        engine.cached_file_paths(),
        preload_order,
        "source-grounded fresh preload order"
    );
}
