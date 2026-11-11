use mnemosyne_engine::{Engine, FileTarget, OpenOptions};
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
fn invalid_utf8_saved_adapter_and_bom_stripped_editor_match_captured_outputs_without_rewriting_disk()
{
    let report: Value = serde_json::from_str(include_str!("fixtures/reference_utf8.json")).unwrap();
    let root = std::env::temp_dir().join(format!(
        "mnemosyne-utf8-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&root).unwrap();
    let fixture = Fixture(root);
    for case in report["cases"].as_array().unwrap() {
        let bytes = serde_json::from_value::<Vec<u8>>(case["bytes"].clone()).unwrap();
        std::fs::write(fixture.0.join(case["path"].as_str().unwrap()), bytes).unwrap();
    }
    let mut engine = Engine::open(&fixture.0).unwrap();
    for case in report["cases"].as_array().unwrap() {
        let path = case["path"].as_str().unwrap();
        let target = FileTarget::path(path);
        assert_eq!(
            engine.read_saved(target).unwrap(),
            case["observed"]["adapter"].as_str().unwrap(),
            "{}",
            case["id"]
        );
        engine.open_file(target, OpenOptions::default()).unwrap();
        assert_eq!(
            engine.editor().unwrap().text,
            case["observed"]["read"].as_str().unwrap(),
            "{}",
            case["id"]
        );
        assert_eq!(
            std::fs::read(fixture.0.join(path)).unwrap(),
            serde_json::from_value::<Vec<u8>>(case["bytes"].clone()).unwrap()
        );
    }
}
