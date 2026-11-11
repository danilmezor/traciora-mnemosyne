use mnemosyne_engine::{language, moment, Engine};
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
fn actual_renderer_preference_reload_keeps_ui_moment_and_intl_context_separate() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/reference_language.json")).unwrap();
    let root = std::env::temp_dir().join(format!(
        "mnemosyne-language-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&root).unwrap();
    let dir = Fixture(root);
    let mut engine = Engine::open(&dir.0).unwrap();
    let now =
        time::OffsetDateTime::from_unix_timestamp_nanos(1767240306987_i128 * 1_000_000).unwrap();
    for row in fixture["cases"].as_array().unwrap() {
        let observed = &row["observed"];
        engine.import_language(row["storage"].as_str(), row["navigator"].as_str().unwrap());
        engine
            .import_collation_locale(observed["intl"].as_str().unwrap())
            .unwrap();
        if let Some(ui) = observed["ui"].as_str() {
            assert_eq!(engine.ui_language(), ui, "{}", row["id"]);
        }
        assert_eq!(
            engine.locale(),
            observed["moment"].as_str().unwrap(),
            "{}",
            row["id"]
        );
        assert_eq!(engine.collation_locale(), "en-US", "{}", row["id"]);
        assert_eq!(
            moment::format(now, "LLLL", engine.locale()).unwrap(),
            observed["date"].as_str().unwrap(),
            "{}",
            row["id"]
        );
    }
    engine.reload().unwrap();
    assert_eq!(engine.ui_language(), "fa");
    assert_eq!(engine.locale(), "fa");
    assert_eq!(engine.collation_locale(), "en-US");
}
#[test]
fn explicit_import_is_case_sensitive_for_navigator_and_validates_collation_without_linking_date_locale(
) {
    assert_eq!(language::supported_languages().len(), 51);
    assert_eq!(language::select(None, "en-GB"), "en-GB");
    assert_eq!(language::select(None, "en-gb"), "en");
    assert_eq!(
        language::select(Some("unknown-preference"), "fr-FR"),
        "unknown-preference"
    );
    let root = std::env::temp_dir().join(format!(
        "mnemosyne-language-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&root).unwrap();
    let dir = Fixture(root);
    let mut engine = Engine::open(&dir.0).unwrap();
    engine.import_collation_locale("sv-SE").unwrap();
    engine.import_language(Some("ru"), "en-US");
    assert_eq!(engine.collation_locale(), "sv-SE");
    assert_eq!(engine.locale(), "ru");
    assert!(engine.import_collation_locale("not a locale").is_err());
    assert_eq!(engine.collation_locale(), "sv-SE");
}
