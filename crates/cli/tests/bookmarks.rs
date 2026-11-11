use mnemosyne_core::CONFIG_DIR;
use mnemosyne_engine::{Engine, FileTarget, OpenOptions};
use serde_json::{json, Value};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
struct Fixture(PathBuf);
impl Fixture {
    fn new(data: &Value) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "mn-bookmarks-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(path.join(CONFIG_DIR)).unwrap();
        fs::write(
            path.join(CONFIG_DIR).join("core-plugins.json"),
            r#"{"bookmarks":true}"#,
        )
        .unwrap();
        for (name, text) in data["initial_files"].as_object().unwrap() {
            let file = path.join(name);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, text.as_str().unwrap()).unwrap();
        }
        Self(path)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/bookmarks.json")).unwrap()
}
#[test]
fn bookmarks_match_48_actual_nested_duplicate_empty_and_format_outputs() {
    let data = fixture();
    let root = Fixture::new(&data);
    let mut engine = Engine::open(&root.0).unwrap();
    for case in data["listing"].as_array().unwrap() {
        let document = json!({"items":case["initial_items"]});
        fs::write(
            root.0.join(CONFIG_DIR).join("bookmarks.json"),
            serde_json::to_vec_pretty(&document).unwrap(),
        )
        .unwrap();
        engine.bookmarks.reload();
        let args: Vec<String> = serde_json::from_value(case["args"].clone()).unwrap();
        assert_eq!(
            mnemosyne::app::execute(&mut engine, &args),
            case["expected"]["stdout"],
            "{}",
            case["id"]
        );
        assert_eq!(
            engine.bookmarks.items(),
            case["initial_items"].as_array().unwrap()
        );
        assert_eq!(
            fs::read(root.0.join(CONFIG_DIR).join("bookmarks.json")).unwrap(),
            serde_json::to_vec_pretty(&document).unwrap(),
            "listing does not mutate data"
        );
    }
}
#[test]
fn bookmarks_match_31_actual_additions_errors_and_saved_bytes() {
    let data = fixture();
    for case in data["mutations"].as_array().unwrap() {
        let root = Fixture::new(&data);
        fs::write(
            root.0.join(CONFIG_DIR).join("bookmarks.json"),
            case["before"]["disk"].as_str().unwrap(),
        )
        .unwrap();
        let mut engine = Engine::open(&root.0).unwrap();
        if case["before"]["active"].is_string() {
            engine
                .open_file(
                    FileTarget::path(case["before"]["active"].as_str().unwrap()),
                    OpenOptions::default(),
                )
                .unwrap();
        }
        let args: Vec<String> = serde_json::from_value(case["args"].clone()).unwrap();
        let (_, params) = mnemosyne::args::parse_argv(&args);
        let result = mnemosyne::bookmarks::add_at(&mut engine, &params, 1780000000123);
        let output = match result {
            Ok(value) => mnemosyne::registry::frame_output(&value),
            Err(error) => mnemosyne::error::render(&error),
        };
        assert_eq!(output, case["expected"]["stdout"], "{}", case["id"]);
        engine.bookmarks.wait_for_pending_saves();
        assert!(engine.bookmarks.take_save_diagnostics().is_empty());
        assert_eq!(
            engine.bookmarks.items(),
            case["after"]["items"].as_array().unwrap(),
            "{} items",
            case["id"]
        );
        assert_eq!(
            fs::read_to_string(root.0.join(CONFIG_DIR).join("bookmarks.json")).unwrap(),
            case["after"]["disk"].as_str().unwrap(),
            "{} saved bytes",
            case["id"]
        );
    }
}
#[test]
fn bookmark_handlers_follow_actual_disabled_plugin_availability() {
    let data = fixture();
    let root = Fixture::new(&data);
    fs::write(
        root.0.join(CONFIG_DIR).join("core-plugins.json"),
        r#"{"bookmarks":false}"#,
    )
    .unwrap();
    let mut engine = Engine::open(&root.0).unwrap();
    for command in ["bookmark", "bookmarks"] {
        assert_eq!(
            mnemosyne::app::execute(&mut engine, &[command.into()]),
            data["disabled"][command]["stdout"]
        );
    }
}

#[test]
fn normal_binary_adds_and_reloads_duplicates_with_real_clock() {
    use std::{
        process::Command,
        time::{SystemTime, UNIX_EPOCH},
    };
    let data = fixture();
    let root = Fixture::new(&data);
    let invoke = |args: &[&str]| {
        let result = Command::new(env!("CARGO_BIN_EXE_mnemosyne"))
            .args(args)
            .env("MNEMOSYNE_VAULT", &root.0)
            .env_remove("MNEMOSYNE_SOCKET")
            .env_remove("MNEMOSYNE_PROFILE")
            .current_dir(&root.0)
            .output()
            .unwrap();
        assert_eq!(result.status.code(), Some(0));
        assert!(result.stderr.is_empty());
        String::from_utf8(result.stdout).unwrap()
    };
    let before = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    for title in ["First", "Second"] {
        assert_eq!(
            invoke(&["bookmark", "file=Note.md", &format!("title={title}")]),
            "Bookmarked: Note.md\n"
        );
    }
    let after = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let saved = fs::read(root.0.join(CONFIG_DIR).join("bookmarks.json")).unwrap();
    let document: Value = serde_json::from_slice(&saved).unwrap();
    let items = document["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    for (index, title) in ["First", "Second"].iter().enumerate() {
        let ctime = items[index]["ctime"].as_u64().unwrap();
        assert!((before..=after).contains(&ctime));
        assert_eq!(
            items[index],
            json!({"type":"file","ctime":ctime,"path":"Note.md","title":title})
        );
    }
    assert_eq!(saved, serde_json::to_vec_pretty(&document).unwrap());
    assert_eq!(invoke(&["bookmarks"]), "Note.md\nNote.md\n");
    assert_eq!(
        invoke(&["bookmarks", "verbose"]),
        "file\tNote.md\tFirst\nfile\tNote.md\tSecond\n"
    );
    assert_eq!(invoke(&["bookmarks", "total"]), "2\n");
}
