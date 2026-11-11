use mnemosyne_engine::{Engine, FileTarget};
use serde_json::Value;
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "mnemosyne-worker-failure-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        Self(root)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn actual_eight_worker_results_preserve_discovery_and_readable_source_without_fabricating_metadata()
{
    let fixture = Fixture::new();
    let data: Value = serde_json::from_str(include_str!(
        "../../core/tests/fixtures/yaml_worker_failure.json"
    ))
    .unwrap();
    for case in data["cases"].as_array().unwrap() {
        std::fs::write(
            fixture.0.join(case["path"].as_str().unwrap()),
            case["source"].as_str().unwrap(),
        )
        .unwrap();
    }
    let mut engine = Engine::open(&fixture.0).unwrap();
    let order: Vec<String> = serde_json::from_value(data["setup"]["order"].clone()).unwrap();
    engine.import_cache_order(&order).unwrap();
    assert_eq!(
        engine.cached_file_paths(),
        order.iter().map(String::as_str).collect::<Vec<_>>()
    );
    let resolved = data["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|case| !case["expected"]["resolved"].is_null())
        .map(|case| case["path"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(engine.resolved_link_paths(), resolved);
    assert_eq!(engine.ordered_notes_with_cache().count(), 8);
    assert_eq!(
        engine
            .ordered_notes_with_cache()
            .filter(|(_, cache)| cache.is_some())
            .count(),
        2
    );
    for case in data["cases"].as_array().unwrap() {
        let path = case["path"].as_str().unwrap();
        assert_eq!(
            serde_json::to_value(engine.file_cache(path)).unwrap(),
            case["expected"]["cache"],
            "{path}"
        );
        assert_eq!(
            engine.read_saved(FileTarget::path(path)).unwrap(),
            case["expected"]["cached"],
            "{path}"
        );
        assert_eq!(
            engine.metadata_worker_failure(path).is_some(),
            case["expected"]["cache"].is_null(),
            "{path}"
        );
    }
    engine.refresh_cache();
    assert_eq!(
        engine.resolved_link_paths(),
        resolved,
        "refresh must not repopulate failed caches through permissive from_note"
    );
    // A successful saved-source replacement must replace the failure, and a
    // subsequent failing source must remove graph/type metadata again.
    engine
        .vault
        .modify("object-cycle.md", "---\nrestored: 2\n---\n[[Missing]]\n")
        .unwrap();
    engine.file_modified("object-cycle.md");
    assert!(engine.metadata_worker_failure("object-cycle.md").is_none());
    assert!(engine.file_cache("object-cycle.md").is_some());
    assert_eq!(engine.property_occurrences("restored").unwrap(), 1);
    assert!(engine.resolved_link_paths().contains(&"object-cycle.md"));
    engine
        .vault
        .modify(
            "object-cycle.md",
            data["cases"][0]["source"].as_str().unwrap(),
        )
        .unwrap();
    engine.file_modified("object-cycle.md");
    assert!(engine.file_cache("object-cycle.md").is_none());
    assert!(engine.metadata_worker_failure("object-cycle.md").is_some());
    assert_eq!(engine.property_occurrences("restored").unwrap(), 0);
    assert!(!engine.resolved_link_paths().contains(&"object-cycle.md"));
    assert!(engine.cached_file_paths().contains(&"object-cycle.md"));
}
