use mnemosyne_core::CONFIG_DIR;
use mnemosyne_engine::Engine;
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
use time::{Date, Month};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "mnemo-templates-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(path.join(CONFIG_DIR)).unwrap();
        Self(path)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
#[test]
fn replay_named_templates_from_actual_create_cases() {
    let evidence: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/reference_templates.json")).unwrap();
    let now = Date::from_calendar_date(2026, Month::October, 14)
        .unwrap()
        .with_hms(16, 8, 7)
        .unwrap()
        .assume_utc();
    for case in evidence["cases"].as_array().unwrap() {
        let fixture = Fixture::new();
        for path in case["initial_directories"].as_array().unwrap() {
            std::fs::create_dir_all(fixture.0.join(path.as_str().unwrap())).unwrap();
        }
        for (path, text) in case["initial_files"].as_object().unwrap() {
            let destination = fixture.0.join(path);
            std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
            std::fs::write(destination, text.as_str().unwrap()).unwrap();
        }
        std::fs::write(
            fixture.0.join(CONFIG_DIR).join("core-plugins.json"),
            serde_json::json!({"templates":case["enabled"]}).to_string(),
        )
        .unwrap();
        std::fs::write(
            fixture.0.join(CONFIG_DIR).join("templates.json"),
            case["options"].to_string(),
        )
        .unwrap();
        let mut engine = Engine::open(&fixture.0).unwrap();
        let result = engine.render_named_template_at(
            case["name"].as_str().unwrap(),
            case["raw_title"].as_str().unwrap(),
            now,
        );
        if let Some(text) = case["expected_text"].as_str() {
            assert_eq!(result.unwrap(), text, "{}", case["id"]);
        } else {
            assert_eq!(
                result.unwrap_err().to_string(),
                case["expected_error"].as_str().unwrap(),
                "{}",
                case["id"]
            );
        }
        assert!(engine.active_file().is_none());
        assert!(engine.editor().is_none());
    }
}
#[test]
fn title_substitution_follows_js_replacement_then_date_callbacks() {
    let fixture = Fixture::new();
    std::fs::create_dir(fixture.0.join("Templates")).unwrap();
    std::fs::write(
        fixture.0.join("Templates/Basic.md"),
        "a{{title}}b{{TITLE}}c {{date}}\r\n",
    )
    .unwrap();
    std::fs::write(
        fixture.0.join(CONFIG_DIR).join("templates.json"),
        r#"{"folder":"Templates","dateFormat":"[DATE]"}"#,
    )
    .unwrap();
    let mut engine = Engine::open(&fixture.0).unwrap();
    assert_eq!(
        engine.render_named_template("Basic", "$$-$&").unwrap(),
        "a$-{{title}}b$-{{TITLE}}c DATE\r\n"
    );
    assert_eq!(
        engine.render_named_template("Basic", "{{date}}").unwrap(),
        "aDATEbDATEc DATE\r\n"
    );
    assert_eq!(
        engine.render_named_template("Basic", "$`").unwrap(),
        "aaba{{title}}bc DATE\r\n"
    );
    assert_eq!(
        engine.render_named_template("Basic", "$'").unwrap(),
        "ab{{TITLE}}c DATE\r\nbc DATE\r\nc DATE\r\n"
    );
}
