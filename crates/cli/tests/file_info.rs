use base64::{engine::general_purpose::STANDARD, Engine as _};
use mnemosyne::{
    app,
    file_info::{self, FileStat},
    registry::frame_output,
};
use mnemosyne_engine::{Engine, FileTarget, OpenOptions};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, UNIX_EPOCH},
};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "mnemosyne-file-info-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn cases() -> Vec<Value> {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/file_info.json")).unwrap();
    fixture["cases"].as_array().unwrap().clone()
}
fn write(root: &Path, case: &Value) {
    let path = root.join(case["path"].as_str().unwrap());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        path,
        STANDARD
            .decode(case["content_base64"].as_str().unwrap())
            .unwrap(),
    )
    .unwrap();
}
fn invoke(engine: &mut Engine, case: &Value) -> String {
    app::execute(
        engine,
        &serde_json::from_value::<Vec<String>>(case["args"].clone()).unwrap(),
    )
}

#[test]
fn thirty_actual_stat_tuples_match_complete_native_tsv_and_pipe_framing() {
    let mut count = 0;
    for case in cases().iter().filter(|case| case.get("raw_stat").is_some()) {
        let raw = &case["raw_stat"];
        let stat = FileStat::from_raw_milliseconds(
            raw["size"].as_u64().unwrap(),
            raw["birthtimeMs"].as_f64().unwrap(),
            raw["mtimeMs"].as_f64().unwrap(),
        );
        assert_eq!(
            stat.size,
            case["stat"]["size"].as_u64().unwrap(),
            "{} size",
            case["id"]
        );
        assert_eq!(
            stat.created_ms,
            case["stat"]["ctime"].as_f64().unwrap(),
            "{} birthtime rounding",
            case["id"]
        );
        assert_eq!(
            stat.modified_ms,
            case["stat"]["mtime"].as_f64().unwrap(),
            "{} mtime rounding",
            case["id"]
        );
        assert_eq!(
            frame_output(&file_info::format(case["path"].as_str().unwrap(), stat)),
            case["expected"]["stdout"],
            "{} TSV",
            case["id"]
        );
        assert_eq!(case["expected"]["stderr"], "");
        assert_eq!(case["expected"]["exit_code"], 0);
        count += 1;
    }
    assert_eq!(count, 30);
}

#[test]
fn controlled_filesystem_mtimes_match_eighteen_native_rounding_samples() {
    let fixture = Fixture::new();
    let mut count = 0;
    for case in cases()
        .iter()
        .filter(|case| case.get("requested_mtime_ns").is_some())
    {
        write(&fixture.0, case);
        let ns = case["requested_mtime_ns"]
            .as_str()
            .unwrap()
            .parse::<i128>()
            .unwrap();
        let duration = Duration::from_nanos(ns.unsigned_abs().try_into().unwrap());
        let modified = if ns < 0 {
            UNIX_EPOCH.checked_sub(duration).unwrap()
        } else {
            UNIX_EPOCH.checked_add(duration).unwrap()
        };
        let file = std::fs::File::options()
            .write(true)
            .open(fixture.0.join(case["path"].as_str().unwrap()))
            .unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        let stat = FileStat::from_metadata(&file.metadata().unwrap()).unwrap();
        assert_eq!(
            stat.size,
            case["stat"]["size"].as_u64().unwrap(),
            "{} bytes",
            case["id"]
        );
        assert_eq!(
            stat.modified_ms,
            case["stat"]["mtime"].as_f64().unwrap(),
            "{} mtime",
            case["id"]
        );
        // On the captured macOS filesystem, nonnegative historical mtimes
        // also clamp birthtime. These are independent OS-created samples.
        #[cfg(target_os = "macos")]
        if case["ns"]["birthtimeNs"] == case["ns"]["mtimeNs"] {
            assert_eq!(
                stat.created_ms,
                case["stat"]["ctime"].as_f64().unwrap(),
                "{} OS birthtime",
                case["id"]
            );
        }
        count += 1;
    }
    assert_eq!(count, 18);
}

#[test]
fn target_resolution_and_seven_errors_match_native_cases() {
    let fixture = Fixture::new();
    let cases = cases();
    for case in cases.iter().filter(|case| case.get("path").is_some()) {
        write(&fixture.0, case);
    }
    let mut engine = Engine::open(&fixture.0).unwrap();
    let mut errors = 0;
    for case in cases.iter().filter(|case| case["id"] != "active-unsaved") {
        let output = invoke(&mut engine, case);
        let expected = case["expected"]["stdout"].as_str().unwrap();
        if expected.starts_with("Error:") {
            assert_eq!(output, expected, "{} target error", case["id"]);
            errors += 1;
        } else {
            // Birthtime differs for independently created files; the full
            // formatter contract is replayed from captured raw inputs above.
            assert_eq!(
                output.lines().next(),
                expected.lines().next(),
                "{} resolved path",
                case["id"]
            );
        }
    }
    assert_eq!(errors, 7);
}

#[test]
fn active_dirty_editor_uses_saved_file_stat_and_leaves_buffer_untouched() {
    let case = cases()
        .into_iter()
        .find(|case| case["id"] == "active-unsaved")
        .unwrap();
    let fixture = Fixture::new();
    write(&fixture.0, &case);
    let mut engine = Engine::open(&fixture.0).unwrap();
    engine
        .open_file(FileTarget::path("Empty.md"), OpenOptions::default())
        .unwrap();
    let dirty = case["editor"]["editor"].as_str().unwrap();
    engine.editor_mut().unwrap().text = dirty.into();
    let before = std::fs::metadata(fixture.0.join("Empty.md")).unwrap();
    let output = invoke(&mut engine, &case);
    assert_eq!(output.lines().nth(0), Some("path\tEmpty.md"));
    assert_eq!(output.lines().nth(3), Some("size\t0"));
    assert_eq!(engine.editor().unwrap().text, dirty);
    assert_eq!(std::fs::read(fixture.0.join("Empty.md")).unwrap(), b"");
    let after = std::fs::metadata(fixture.0.join("Empty.md")).unwrap();
    assert_eq!(before.created().unwrap(), after.created().unwrap());
    assert_eq!(before.modified().unwrap(), after.modified().unwrap());
}
