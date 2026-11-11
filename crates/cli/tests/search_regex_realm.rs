use mnemosyne_core::CONFIG_DIR;
use mnemosyne_engine::Engine;
use serde_json::Value;
use std::{
    fs,
    path::PathBuf,
    time::{Duration, Instant},
};
struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
#[test]
fn persistent_regex_dictionaries_match_13_native_sensitive_poison_and_idle_steps() {
    let data: Value =
        serde_json::from_str(include_str!("fixtures/search_regex_realm.json")).unwrap();
    let root = Scratch(std::env::temp_dir().join(format!("mn-regex-realm-{}", std::process::id())));
    fs::create_dir_all(root.0.join(CONFIG_DIR)).unwrap();
    fs::write(
        root.0.join(CONFIG_DIR).join("core-plugins.json"),
        r#"{"global-search":true}"#,
    )
    .unwrap();
    let mut engine = Engine::open(&root.0).unwrap();
    let order: Vec<String> = serde_json::from_value(data["setup"]["files"].clone()).unwrap();
    for path in order.iter().rev() {
        engine
            .vault
            .create(path, data["initial_files"][path].as_str().unwrap())
            .unwrap();
        engine.file_created(path);
    }
    let mut expired = false;
    for row in data["cases"].as_array().unwrap() {
        if row["phase"] == "after_idle" && !expired {
            assert!(data["idle_elapsed_seconds"].as_f64().unwrap() >= 62.0);
            engine
                .search_regex_realm
                .poll_idle_at(Instant::now() + Duration::from_secs(62));
            expired = true;
        }
        let args: Vec<String> = serde_json::from_value(row["args"].clone()).unwrap();
        assert_eq!(
            mnemosyne::app::execute(&mut engine, &args),
            row["expected"]["stdout"],
            "{} {args:?}",
            row["phase"]
        );
        assert_eq!(row["expected"]["exit_code"], 0);
        assert_eq!(row["expected"]["stderr"], "");
    }
    assert!(expired);
    let poison = vec!["search".into(), "query=[hasOwnProperty]".into()];
    assert_eq!(
        mnemosyne::app::execute(&mut engine, &poison),
        "TypeError: i.hasOwnProperty is not a function\n"
    );
    // Realm state belongs to a retained renderer/engine, not disk/session JSON.
    let mut independent = Engine::open(&root.0).unwrap();
    assert_eq!(
        mnemosyne::app::execute(
            &mut independent,
            &["search".into(), "query=bodyMarker".into(), "total".into()]
        ),
        "2\n"
    );
}
