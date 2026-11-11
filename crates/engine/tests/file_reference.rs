//! Actual CLI FileManager fixture: pending decisions, captured edits and save boundary.
use mnemosyne_core::CONFIG_DIR;
use mnemosyne_engine::{events::CallbackFrame, file_operations::LinkUpdateDecision, Engine};
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn fixture() -> Fixture {
    let root = std::env::temp_dir().join(format!(
        "mn-file-reference-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&root).unwrap();
    fs::create_dir(root.join(CONFIG_DIR)).unwrap();
    Fixture(root)
}
fn native_state(mut state: Value) -> Value {
    for info in state["file_info"]
        .as_object_mut()
        .unwrap()
        .values_mut()
        .filter(|v| v.is_object())
    {
        let zero = info["mtime"] == 0;
        info.as_object_mut().unwrap().remove("mtime");
        info["mtime_zero"] = json!(zero);
    }
    for stat in state["file_stat"]
        .as_object_mut()
        .unwrap()
        .values_mut()
        .filter(|v| v.is_object())
    {
        stat.as_object_mut().unwrap().remove("ctime");
        stat.as_object_mut().unwrap().remove("mtime");
    }
    state
}
fn project(frame: &CallbackFrame, paths: &[String]) -> Value {
    let mut cache = serde_json::Map::new();
    let mut info = serde_json::Map::new();
    let mut stat = serde_json::Map::new();
    let mut relation = serde_json::Map::new();
    let mut resolved = serde_json::Map::new();
    let mut unresolved = serde_json::Map::new();
    for path in paths {
        cache.insert(
            path.clone(),
            frame
                .cache_json
                .get(path)
                .and_then(Option::as_ref)
                .map(|s| serde_json::from_str(s).unwrap())
                .unwrap_or(Value::Null),
        );
        info.insert(
            path.clone(),
            frame
                .file_info
                .get(path)
                .and_then(Option::as_ref)
                .map(|v| json!({"hash":v.hash,"size":v.size,"mtime_zero":v.mtime==0}))
                .unwrap_or(Value::Null),
        );
        stat.insert(
            path.clone(),
            frame
                .file_stat
                .get(path)
                .and_then(Option::as_ref)
                .map(|v| json!({"size":v.size}))
                .unwrap_or(Value::Null),
        );
        relation.insert(
            path.clone(),
            json!(frame.info_stat_matches.get(path).copied().flatten()),
        );
        for (target, source) in [
            (&mut resolved, &frame.resolved),
            (&mut unresolved, &frame.unresolved),
        ] {
            target.insert(
                path.clone(),
                source
                    .get(path)
                    .map(|value| json!({"present":true,"value":value}))
                    .unwrap_or(json!({"present":false,"value":null})),
            );
        }
    }
    json!({"cache":cache,"file_info":info,"file_stat":stat,"info_stat_matches":relation,"resolved":resolved,"unresolved":unresolved,"readiness":frame.readiness})
}
fn first_difference(actual: &Value, expected: &Value, path: &str) -> Option<String> {
    if actual == expected {
        return None;
    }
    match (actual, expected) {
        (Value::Object(a), Value::Object(b)) => {
            let keys = a
                .keys()
                .chain(b.keys())
                .collect::<std::collections::BTreeSet<_>>();
            for key in keys {
                match (a.get(key), b.get(key)) {
                    (Some(a), Some(b)) => {
                        if let Some(diff) = first_difference(a, b, &format!("{path}.{key}")) {
                            return Some(diff);
                        }
                    }
                    (a, b) => return Some(format!("{path}.{key}: actual {a:?}, expected {b:?}")),
                }
            }
        }
        (Value::Array(a), Value::Array(b)) if a.len() == b.len() => {
            for (i, (a, b)) in a.iter().zip(b).enumerate() {
                if let Some(diff) = first_difference(a, b, &format!("{path}[{i}]")) {
                    return Some(diff);
                }
            }
        }
        _ => return Some(format!("{path}: actual {actual}, expected {expected}")),
    }
    Some(format!("{path}: different values"))
}
fn compare(actual: Value, expected: Value, label: &str) {
    if let Some(difference) = first_difference(&actual, &expected, "$") {
        panic!("{label}: {difference}");
    }
}
fn assert_state(engine: &Engine, expected: &Value, paths: &[String], label: &str) {
    compare(
        project(&engine.metadata_lifecycle_snapshot(), paths),
        native_state(expected.clone()),
        label,
    );
}
fn disk_matches(engine: &Engine, root: &Path, paths: &[String]) -> bool {
    let frame = engine.metadata_lifecycle_snapshot();
    paths
        .iter()
        .all(|path| match fs::metadata(root.join(path)) {
            Ok(disk) => frame
                .file_stat
                .get(path)
                .and_then(Option::as_ref)
                .is_some_and(|held| {
                    let millis = disk
                        .modified()
                        .unwrap()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos();
                    held.size == disk.len() && held.mtime == ((millis + 500_000) / 1_000_000) as i64
                }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                frame.file_stat.get(path).and_then(Option::as_ref).is_none()
            }
            Err(error) => panic!("disk barrier: {error}"),
        })
}
fn settle(engine: &mut Engine, root: &Path, paths: &[String], label: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut previous = None;
    let mut stable = None;
    while Instant::now() < deadline {
        engine.poll_external_changes().unwrap();
        engine.poll_metadata_lifecycle().unwrap();
        let frame = engine.metadata_lifecycle_snapshot();
        if frame.readiness.is_cache_clean && disk_matches(engine, root, paths) {
            let token = serde_json::to_string(&project(&frame, paths)).unwrap();
            if previous.as_ref() == Some(&token) {
                if stable.is_some_and(|at: Instant| at.elapsed() >= Duration::from_millis(30)) {
                    return;
                }
            } else {
                previous = Some(token);
                stable = Some(Instant::now());
            }
        } else {
            previous = None;
            stable = None;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    panic!("{label}: real owner/disk readiness deadline");
}
fn bytes(root: &Path, expected: &Value, label: &str) {
    for (path, value) in expected.as_object().unwrap() {
        let actual = fs::read(root.join(path)).ok();
        let expected = if value.is_null() {
            None
        } else {
            Some(serde_json::from_value::<Vec<u8>>(value.clone()).unwrap())
        };
        assert_eq!(actual, expected, "{label}: saved {path}");
    }
}
fn assert_events(engine: &Engine, cursor: u64, expected: &Value, paths: &[String], label: &str) {
    let actual = engine.lifecycle_journal_since(cursor).unwrap();
    let expected = expected.as_array().unwrap();
    assert_eq!(
        actual.len(),
        expected.len(),
        "{label}: event names {:?}",
        actual
            .iter()
            .map(|e| (&e.domain, &e.name, &e.path))
            .collect::<Vec<_>>()
    );
    for (i, (event, want)) in actual.iter().zip(expected).enumerate() {
        let actual = json!({"domain":event.domain,"name":event.name,"path":event.path,"old_path":event.old_path,"data":event.data,"cache_argument":event.cache_argument_json.as_ref().map(|s|serde_json::from_str::<Value>(s).unwrap()),"state":project(&event.state,paths)});
        let mut expected = want.clone();
        expected.as_object_mut().unwrap().remove("step");
        expected.as_object_mut().unwrap().remove("performance_ms");
        expected["state"] = native_state(expected["state"].clone());
        compare(actual, expected, &format!("{label}: callback {i}"));
    }
}
#[test]
fn native_file_manager_decisions_captured_current_text_and_natural_config_save() {
    let report: Value =
        serde_json::from_str(include_str!("fixtures/file_reference.json")).unwrap();
    let fixture = fixture();
    let paths: Vec<String> = serde_json::from_value(report["selected_paths"].clone()).unwrap();
    for (path, source) in report["preconditioning"]["initial_bootstrap_files"]
        .as_object()
        .unwrap()
    {
        fs::write(fixture.0.join(path), source.as_str().unwrap()).unwrap();
    }
    let config_input: Vec<u8> =
        serde_json::from_value(report["preconditioning"]["app_config_bytes"].clone()).unwrap();
    fs::write(fixture.0.join(CONFIG_DIR).join("app.json"), &config_input).unwrap();
    let mut warm = Engine::open(&fixture.0).unwrap();
    for operation in report["preconditioning"]["operations"].as_array().unwrap() {
        let path = operation["path"].as_str().unwrap();
        match operation["type"].as_str().unwrap() {
            "create" => {
                warm.vault
                    .create(path, operation["content"].as_str().unwrap())
                    .unwrap();
                warm.file_created(path);
            }
            "delete" => {
                warm.vault.delete(path).unwrap();
                warm.file_deleted(path);
            }
            other => panic!("precondition {other}"),
        }
    }
    warm.prepare_shutdown().unwrap();
    drop(warm);
    // The native controller authors this startup input after preconditioning
    // shutdown; it is an input, not a copied post-operation state.
    fs::write(fixture.0.join(CONFIG_DIR).join("app.json"), &config_input).unwrap();
    let mut engine = Engine::prepare_startup(&fixture.0).unwrap();
    // Actual native probe subscriptions are installed in plugin.onload before
    // MetadataCache attaches its Vault handlers during initialization.
    engine.start_lifecycle_journal();
    engine.start_startup_loading().unwrap();
    while !engine.poll_startup().unwrap() {}
    engine.enable_retained_watch().unwrap();
    settle(&mut engine, &fixture.0, &paths, "initial");
    for row in report["steps"].as_array().unwrap() {
        let label = row["id"].as_str().unwrap();
        assert_state(
            &engine,
            &row["before"]["state"],
            &paths,
            &format!("{label}: before"),
        );
        assert_eq!(
            engine.get_config("alwaysUpdateLinks"),
            &row["before"]["config"]["alwaysUpdateLinks"]
        );
        let argv = row["operation"]["argv"].as_array().unwrap();
        let from = argv[1].as_str().unwrap().strip_prefix("path=").unwrap();
        let to = argv[2].as_str().unwrap().strip_prefix("to=").unwrap();
        let cursor = engine.lifecycle_cursor();
        let id = engine.begin_file_rename(from, to, to);
        engine.poll_file_operations();
        if !row["operation"]["decision"].is_null() {
            let pending = engine
                .pending_link_update()
                .expect("actual pending owner decision");
            assert_eq!(pending.id, id);
            assert_eq!(pending.affected_references, 1);
            assert!(engine.take_file_operation_result(id).is_none());
            // Native controller samples different genuine resolver boundaries:
            // decline before any resolver entry, once/always after resolution,
            // edited-source after one entry with the sentinel still queued.
            match label {
                "decline" => {}
                "pending-source-edit" => {
                    engine.poll_metadata_resolution();
                }
                _ => settle(&mut engine, &fixture.0, &paths, label),
            }
            assert_state(
                &engine,
                &row["pending"]["observation"]["state"],
                &paths,
                &format!("{label}: pending"),
            );
            bytes(
                &fixture.0,
                &row["pending"]["observation"]["file_bytes"],
                label,
            );
            // The genuine decision/source edit arrives after outstanding
            // rename resolution completes, as the native callback order shows.
            settle(&mut engine, &fixture.0, &paths, "before pending input");
            if let Some(edit) = row["operation"].get("pending_modify") {
                let path = edit["path"].as_str().unwrap();
                engine
                    .vault
                    .modify(path, edit["content"].as_str().unwrap())
                    .unwrap();
                engine.file_modified(path);
                settle(&mut engine, &fixture.0, &paths, "pending source edit");
            }
            let decision = match row["operation"]["decision"].as_str().unwrap() {
                "decline" => LinkUpdateDecision::DoNotUpdate,
                "once" => LinkUpdateDecision::Once,
                "always" => LinkUpdateDecision::Always,
                other => panic!("decision {other}"),
            };
            engine.decide_link_update(id, decision).unwrap();
            engine.poll_file_operations();
        }
        engine
            .take_file_operation_result(id)
            .expect("original operation completes only after genuine owner decision")
            .unwrap();
        let promise = row["settled"]["markers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|marker| marker["kind"] == "file-manager-resolved")
            .unwrap();
        assert_state(
            &engine,
            &promise["state"],
            &paths,
            &format!("{label}: promise completion"),
        );
        bytes(&fixture.0, &promise["file_bytes"], label);
        assert_eq!(
            engine.get_config("alwaysUpdateLinks"),
            &promise["config"]["alwaysUpdateLinks"]
        );
        assert_eq!(
            fs::read(fixture.0.join(CONFIG_DIR).join("app.json")).unwrap(),
            serde_json::from_value::<Vec<u8>>(promise["app_config_bytes"].clone()).unwrap(),
            "{label}: memory/disk config boundary"
        );
        settle(&mut engine, &fixture.0, &paths, label);
        assert_state(
            &engine,
            &row["settled"]["state"],
            &paths,
            &format!("{label}: settled"),
        );
        assert_events(&engine, cursor, &row["settled"]["events"], &paths, label);
        bytes(&fixture.0, &row["file_bytes"], label);
        if label == "no-links" {
            // Native setupConfig naturally rewrites the authored compact input
            // before the next captured row. Pump the real one-second owner timer.
            let deadline = Instant::now() + Duration::from_secs(3);
            while !engine.poll_file_operation_config().unwrap() {
                assert!(Instant::now() < deadline, "startup config timer");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        if let Some(endpoint) = row.get("config_save_endpoint") {
            let deadline = Instant::now() + Duration::from_secs(3);
            while !engine.poll_file_operation_config().unwrap() {
                assert!(Instant::now() < deadline, "natural config timer");
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(
                fs::read(fixture.0.join(CONFIG_DIR).join("app.json")).unwrap(),
                serde_json::from_value::<Vec<u8>>(endpoint["app_config_bytes"].clone()).unwrap(),
                "natural delayed config bytes"
            );
        }
    }
    engine.prepare_shutdown().unwrap();
}
