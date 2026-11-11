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
            "mnemo-daily-{}-{}",
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
fn tree(root: &std::path::Path) -> (serde_json::Value, Vec<String>) {
    fn walk(
        root: &std::path::Path,
        path: &std::path::Path,
        files: &mut serde_json::Map<String, serde_json::Value>,
        dirs: &mut Vec<String>,
    ) {
        for entry in std::fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if path.is_dir() {
                dirs.push(relative);
                walk(root, &path, files, dirs);
            } else {
                files.insert(relative, std::fs::read_to_string(path).unwrap().into());
            }
        }
    }
    let mut files = serde_json::Map::new();
    let mut dirs = Vec::new();
    walk(root, root, &mut files, &mut dirs);
    dirs.sort();
    (files.into(), dirs)
}
#[test]
fn replay_actual_daily_getter_creation_templates_existing_errors_and_recents() {
    let evidence: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/reference_daily.json")).unwrap();
    let now = Date::from_calendar_date(2026, Month::October, 14)
        .unwrap()
        .with_hms_milli(16, 8, 7, 123)
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
        for (file, value) in [
            ("core-plugins.json", serde_json::json!({"daily-notes":true})),
            ("daily-notes.json", case["options"].clone()),
            ("app.json", case["app_config"].clone()),
        ] {
            std::fs::write(fixture.0.join(CONFIG_DIR).join(file), value.to_string()).unwrap();
        }
        let mut engine = Engine::open(&fixture.0).unwrap();
        let first = engine.daily_note_path_at(now);
        if let Some(path) = case["first_path"].as_str() {
            assert_eq!(first.unwrap(), path, "{}", case["id"]);
            if case["expected_first_stdout"] == "No tasks found.\n" {
                assert!(
                    engine.file_cache(path).is_none_or(|cache| cache
                        .list_items
                        .iter()
                        .all(|item| item.task.is_none())),
                    "{} creation frame cache",
                    case["id"]
                );
            }
            assert_eq!(
                engine.daily_note_path_at(now).unwrap(),
                case["second_path"].as_str().unwrap(),
                "{} repeated getter",
                case["id"]
            );
        } else {
            assert_eq!(
                first.unwrap_err().to_string(),
                "Failed to get daily note.",
                "{}",
                case["id"]
            );
            assert_eq!(
                engine.daily_note_path_at(now).unwrap_err().to_string(),
                "Failed to get daily note."
            );
        }
        let (files, dirs) = tree(&fixture.0);
        assert_eq!(files, case["after_files"], "{} saved files", case["id"]);
        assert_eq!(
            serde_json::json!(dirs),
            case["after_directories"],
            "{} directories",
            case["id"]
        );
        assert!(engine.active_file().is_none());
        assert!(engine.editor().is_none());
        assert_eq!(
            serde_json::json!(engine.recents()),
            case["after_recents"],
            "{} recents",
            case["id"]
        );
        engine.refresh_vault().unwrap();
        if let Some(path) = case["second_path"]
            .as_str()
            .filter(|path| path.ends_with(".md"))
        {
            assert!(
                engine.file_cache(path).is_some(),
                "{} metadata after explicit readiness",
                case["id"]
            );
        }
    }
}
#[test]
fn actual_retained_daily_plugin_flags_require_disable_or_cold_boot() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.0.join(CONFIG_DIR).join("daily-notes.json"),
        r#"{"format":"[Today]"}"#,
    )
    .unwrap();
    let mut engine = Engine::open(&fixture.0).unwrap();
    assert_eq!(engine.daily_note_path().unwrap(), "Today.md");
    std::fs::write(
        fixture.0.join(CONFIG_DIR).join("core-plugins.json"),
        r#"{"daily-notes":false}"#,
    )
    .unwrap();
    assert_eq!(engine.daily_note_path().unwrap(), "Today.md");
    engine.reload_core_plugin_settings().unwrap();
    assert_eq!(engine.daily_note_path().unwrap(), "Today.md");
    engine = Engine::open(&fixture.0).unwrap();
    assert_eq!(
        engine.daily_note_path().unwrap_err().to_string(),
        "Daily notes plugin is not enabled."
    );
    std::fs::write(
        fixture.0.join(CONFIG_DIR).join("core-plugins.json"),
        r#"["daily-notes"]"#,
    )
    .unwrap();
    engine.reload_core_plugin_settings().unwrap();
    assert_eq!(engine.daily_note_path().unwrap(), "Today.md");
    std::fs::write(fixture.0.join(CONFIG_DIR).join("core-plugins.json"), "[]").unwrap();
    engine.reload_core_plugin_settings().unwrap();
    assert_eq!(engine.daily_note_path().unwrap(), "Today.md");
    engine = Engine::open(&fixture.0).unwrap();
    assert_eq!(
        engine.daily_note_path().unwrap_err().to_string(),
        "Daily notes plugin is not enabled."
    );
}
