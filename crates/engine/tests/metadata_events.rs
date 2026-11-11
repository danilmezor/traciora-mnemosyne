//! Actual nineteen-step native lifecycle trace. Callback cache/graph/counter
//! state is frozen by the owner; timestamps remain raw in the fixture and are
//! compared by stat relationships instead of importing native wall-clock values.
use mnemosyne_engine::{
    events::{CallbackFrame, LifecycleEvent, MetadataLinkMap},
    json_units::JsonUnits,
    observation::{snapshot, Selection},
    Engine, FileTarget, OpenOptions,
};
use serde::Serialize;
use serde_json::{json, value::RawValue, Value};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        loop {
            let root = std::env::temp_dir().join(format!(
                "mn-meta-events-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&root) {
                Ok(()) => {
                    fs::create_dir(root.join("vault")).unwrap();
                    return Self(root);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("cannot create event fixture {}: {error}", root.display()),
            }
        }
    }
    fn vault(&self) -> PathBuf {
        self.0.join("vault")
    }
    fn outside(&self) -> PathBuf {
        self.0.join("Outside.md")
    }
    fn identity(&self) -> String {
        format!("file:{}", self.outside().display())
    }
    fn physical(&self, path: &str) -> PathBuf {
        if path == "$EXTERNAL" {
            self.outside()
        } else {
            self.vault().join(path)
        }
    }
    fn path(&self, path: &str) -> String {
        if path == "$EXTERNAL" {
            self.identity()
        } else {
            path.into()
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn normalized_path(path: &str, fixture: &Fixture) -> String {
    if path == fixture.identity() {
        "$EXTERNAL".into()
    } else {
        path.into()
    }
}
fn canonical(value: &mut JsonUnits) {
    match value {
        JsonUnits::Object(fields) => {
            fields.sort_by(|(left, _), (right, _)| left.cmp(right));
            for (_, value) in fields {
                canonical(value);
            }
        }
        JsonUnits::Array(values) => {
            for value in values {
                canonical(value);
            }
        }
        _ => {}
    }
}
fn lossless(text: &str) -> JsonUnits {
    let mut value = JsonUnits::parse(text).unwrap();
    canonical(&mut value);
    value
}
fn first_difference(actual: &JsonUnits, expected: &JsonUnits, path: &str) -> Option<String> {
    match (actual, expected) {
        (JsonUnits::Object(a), JsonUnits::Object(b)) if a.len() == b.len() => {
            for ((ak, av), (bk, bv)) in a.iter().zip(b) {
                if ak != bk {
                    return Some(format!("{path}: keys {ak:?} != {bk:?}"));
                }
                let key = String::from_utf16(ak).unwrap_or_else(|_| format!("UTF16{ak:?}"));
                if let Some(diff) = first_difference(av, bv, &format!("{path}.{key}")) {
                    return Some(diff);
                }
            }
        }
        (JsonUnits::Array(a), JsonUnits::Array(b)) if a.len() == b.len() => {
            for (index, (a, b)) in a.iter().zip(b).enumerate() {
                if let Some(diff) = first_difference(a, b, &format!("{path}[{index}]")) {
                    return Some(diff);
                }
            }
        }
        _ if actual != expected => {
            return Some(format!("{path}: actual {actual:?}, native {expected:?}"))
        }
        _ => {}
    }
    None
}
fn assert_lossless(actual: &str, expected: &str, label: &str) {
    let actual = lossless(actual);
    let expected = lossless(expected);
    assert!(
        actual == expected,
        "{label}: {}",
        first_difference(&actual, &expected, "$").unwrap()
    );
}
fn raw(text: Option<&String>) -> Option<Box<RawValue>> {
    text.map(|text| RawValue::from_string(text.clone()).unwrap())
}
#[derive(Serialize)]
struct StateProjection {
    cache: BTreeMap<String, Option<Box<RawValue>>>,
    file_info: BTreeMap<String, Value>,
    file_stat: BTreeMap<String, Value>,
    info_stat_matches: BTreeMap<String, Option<bool>>,
    resolved: BTreeMap<String, Value>,
    unresolved: BTreeMap<String, Value>,
    readiness: mnemosyne_engine::events::MetadataReadiness,
}
fn links(map: &MetadataLinkMap, path: &str, fixture: &Fixture) -> Value {
    match map.get(path) {
        Some(links) => {
            json!({"present":true,"value":links.iter().map(|(path,count)|(normalized_path(path,fixture),*count)).collect::<BTreeMap<_,_>>()})
        }
        None => json!({"present":false,"value":null}),
    }
}
fn project(frame: &CallbackFrame, selected: &[String], fixture: &Fixture) -> StateProjection {
    let mut state = StateProjection {
        cache: BTreeMap::new(),
        file_info: BTreeMap::new(),
        file_stat: BTreeMap::new(),
        info_stat_matches: BTreeMap::new(),
        resolved: BTreeMap::new(),
        unresolved: BTreeMap::new(),
        readiness: frame.readiness.clone(),
    };
    for selected_path in selected {
        let path = fixture.path(selected_path);
        state.cache.insert(
            selected_path.clone(),
            raw(frame.cache_json.get(&path).and_then(Option::as_ref)),
        );
        state.file_info.insert(
            selected_path.clone(),
            frame
                .file_info
                .get(&path)
                .and_then(Option::as_ref)
                .map(
                    |info| json!({"size":info.size,"hash":info.hash,"mtime_is_zero":info.mtime==0}),
                )
                .unwrap_or(Value::Null),
        );
        state.file_stat.insert(
            selected_path.clone(),
            frame
                .file_stat
                .get(&path)
                .and_then(Option::as_ref)
                .map(|stat| json!({"size":stat.size}))
                .unwrap_or(Value::Null),
        );
        state.info_stat_matches.insert(
            selected_path.clone(),
            frame.info_stat_matches.get(&path).copied().flatten(),
        );
        state.resolved.insert(
            selected_path.clone(),
            links(&frame.resolved, &path, fixture),
        );
        state.unresolved.insert(
            selected_path.clone(),
            links(&frame.unresolved, &path, fixture),
        );
    }
    state
}
fn native_state(mut state: Value) -> Value {
    for info in state["file_info"]
        .as_object_mut()
        .unwrap()
        .values_mut()
        .filter(|value| value.is_object())
    {
        let zero = info["mtime"] == 0;
        info.as_object_mut().unwrap().remove("mtime");
        info["mtime_is_zero"] = json!(zero);
    }
    for stat in state["file_stat"]
        .as_object_mut()
        .unwrap()
        .values_mut()
        .filter(|value| value.is_object())
    {
        stat.as_object_mut().unwrap().remove("ctime");
        stat.as_object_mut().unwrap().remove("mtime");
    }
    state
}
fn assert_state(
    frame: &CallbackFrame,
    expected: &Value,
    selected: &[String],
    fixture: &Fixture,
    label: &str,
) {
    let actual = serde_json::to_string(&project(frame, selected, fixture)).unwrap();
    let expected = serde_json::to_string(&native_state(expected.clone())).unwrap();
    assert_lossless(
        &actual,
        &expected,
        &format!("{label}: full selected owner state"),
    );
}
#[derive(Serialize)]
struct EventProjection {
    domain: mnemosyne_engine::events::LifecycleDomain,
    name: String,
    path: Option<String>,
    old_path: Option<String>,
    data: Option<String>,
    cache_argument: Option<Box<RawValue>>,
    state: StateProjection,
}
fn assert_events(
    events: &[LifecycleEvent],
    expected: &Value,
    selected: &[String],
    fixture: &Fixture,
    label: &str,
) {
    assert_eq!(
        events.len(),
        expected.as_array().unwrap().len(),
        "{label}: callback count; actual names {:?}",
        events
            .iter()
            .map(|event| (event.qualified_name(), event.path.as_deref().map(|path| normalized_path(path, fixture))))
            .collect::<Vec<_>>()
    );
    for (index, (event, expected)) in events.iter().zip(expected.as_array().unwrap()).enumerate() {
        let actual = EventProjection {
            domain: event.domain,
            name: event.name.clone(),
            path: event
                .path
                .as_deref()
                .map(|path| normalized_path(path, fixture)),
            old_path: event
                .old_path
                .as_deref()
                .map(|path| normalized_path(path, fixture)),
            data: event.data.clone(),
            cache_argument: raw(event.cache_argument_json.as_ref()),
            state: project(&event.state, selected, fixture),
        };
        let mut expected = expected.clone();
        expected.as_object_mut().unwrap().remove("step");
        expected.as_object_mut().unwrap().remove("performance_ms");
        expected["state"] = native_state(expected["state"].clone());
        assert_lossless(
            &serde_json::to_string(&actual).unwrap(),
            &serde_json::to_string(&expected).unwrap(),
            &format!("{label}: callback {index} {}", event.qualified_name()),
        );
    }
}
fn activate(engine: &mut Engine) {
    engine.enable_metadata_lifecycle().unwrap();
    engine.enable_retained_watch().unwrap();
}
fn advance_mtime(path: &Path, previous: std::time::SystemTime) {
    fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(previous + Duration::from_millis(1)))
        .unwrap();
}
fn disk_matches_owner(
    engine: &Engine,
    fixture: &Fixture,
    selected: &[String],
    frame: &CallbackFrame,
) -> Option<String> {
    for selected_path in selected {
        let path = fixture.path(selected_path);
        if selected_path == "$EXTERNAL" && engine.external_file(&path).is_none() {
            continue;
        }
        let disk = match fs::metadata(fixture.physical(selected_path)) {
            Ok(metadata) => {
                Some(mnemosyne_engine::external::ExternalStat::from_metadata(&metadata).unwrap())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => panic!("stat {selected_path}: {error}"),
        };
        let retained = frame.file_stat.get(&path).and_then(Option::as_ref);
        match (disk.as_ref(), retained) {
            (Some(disk), Some(retained))
                if disk.mtime == retained.mtime && disk.size == retained.size => {}
            (None, None) => {}
            _ => return Some(format!("{selected_path}: disk={disk:?} retained={retained:?}")),
        }
        if selected_path == "$EXTERNAL"
            && engine
                .external_file(&path)
                .is_some_and(|file| file.users > 0)
        {
            let bytes = fs::read(fixture.outside()).unwrap();
            let note = mnemosyne_core::Note::parse(
                path.clone().into(),
                String::from_utf8_lossy(&bytes).into_owned(),
            );
            let current = mnemosyne_core::cache::CachedMetadata::try_from_note(&note)
                .ok()
                .map(|cache| serde_json::to_string(&cache).unwrap());
            let published = frame.cache_json.get(&path).cloned().flatten();
            match (current, published) {
                (Some(current), Some(published)) if lossless(&current) == lossless(&published) => {}
                (None, None) => {}
                _ => return Some(format!("{selected_path}: active outside cache differs from disk")),
            }
        }
    }
    None
}
fn settle(engine: &mut Engine, fixture: &Fixture, selected: &[String], label: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut stable = 0;
    let mut previous = None;
    while Instant::now() < deadline {
        engine.poll_external_changes().unwrap();
        engine.poll_metadata_lifecycle_at(Instant::now()).unwrap();
        let state = engine.metadata_lifecycle_snapshot();
        let ready = state.readiness.initialized
            && state.readiness.is_cache_clean
            && disk_matches_owner(engine, fixture, selected, &state).is_none();
        let serialized = serde_json::to_string(&project(&state, selected, fixture)).unwrap();
        if ready && previous.as_ref() == Some(&serialized) {
            stable += 1;
        } else {
            stable = 0;
        }
        previous = Some(serialized);
        if stable >= 15 {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!(
        "{label}: metadata/watch owner did not settle: {:?}; {:?}",
        engine.metadata_readiness(),
        disk_matches_owner(engine, fixture, selected, &engine.metadata_lifecycle_snapshot())
    );
}
fn assert_bytes(fixture: &Fixture, expected: &Value, label: &str) {
    for (path, expected) in expected.as_object().unwrap() {
        let actual = match fs::read(fixture.physical(path)) {
            Ok(bytes) => serde_json::to_value(bytes).unwrap(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Value::Null,
            Err(error) => panic!("{label}: read {path}: {error}"),
        };
        assert_eq!(actual, *expected, "{label}: saved bytes {path}");
    }
}

#[test]
fn native_nineteen_stage_metadata_callbacks_cache_graph_readiness_and_external_changes() {
    let evidence: Value =
        serde_json::from_str(include_str!("fixtures/metadata_events.json")).unwrap();
    assert_eq!(evidence["identity"]["reference_version"], "1.14.4");
    assert_eq!(evidence["steps"].as_array().unwrap().len(), 19);
    let selected: Vec<String> = serde_json::from_value(evidence["selected_paths"].clone()).unwrap();
    let fixture = Fixture::new();
    for (path, text) in evidence["initial_files"].as_object().unwrap() {
        fs::write(fixture.physical(path), text.as_str().unwrap()).unwrap();
    }
    for (path, text) in evidence["initial_external_files"].as_object().unwrap() {
        fs::write(fixture.0.join(path), text.as_str().unwrap()).unwrap();
    }
    let mut engine = Engine::open(&fixture.vault()).unwrap();
    activate(&mut engine);
    for stage in evidence["steps"].as_array().unwrap() {
        let id = stage["id"].as_str().unwrap();
        let restart = stage["operation"]["type"] == "offline-and-restart";
        // Native restart listeners and the before snapshot are installed only
        // after offline mutations and startup, unlike the other operation rows.
        if !restart {
            assert_state(
                &engine.metadata_lifecycle_snapshot(),
                &stage["before"],
                &selected,
                &fixture,
                &format!("{id}: before"),
            );
        }
        let mut cursor = engine.start_lifecycle_journal();
        let operation = &stage["operation"];
        match operation["type"].as_str().unwrap() {
            "create" => {
                let path = operation["path"].as_str().unwrap();
                engine
                    .vault
                    .create(path, operation["content"].as_str().unwrap())
                    .unwrap();
                engine.file_created(path);
            }
            "modify" => {
                let path = operation["path"].as_str().unwrap();
                let previous = fs::metadata(fixture.physical(path))
                    .unwrap()
                    .modified()
                    .unwrap();
                engine
                    .vault
                    .modify(path, operation["content"].as_str().unwrap())
                    .unwrap();
                advance_mtime(&fixture.physical(path), previous);
                engine.file_modified(path);
            }
            "metadata-refresh" => {
                engine
                    .compute_file_metadata(operation["path"].as_str().unwrap())
                    .unwrap();
            }
            "rename" => {
                let path = operation["path"].as_str().unwrap();
                let to = operation["to"].as_str().unwrap();
                fs::rename(fixture.physical(path), fixture.physical(to)).unwrap();
                engine.vault.reconcile_path_from_disk(path).unwrap();
                engine.vault.reconcile_path_from_disk(to).unwrap();
                engine.file_renamed(path, to);
            }
            "delete" => {
                let path = operation["path"].as_str().unwrap();
                engine.vault.delete(path).unwrap();
                engine.file_deleted(path);
            }
            "external-create" => {
                fs::write(
                    fixture.physical(operation["path"].as_str().unwrap()),
                    operation["content"].as_str().unwrap(),
                )
                .unwrap();
            }
            "external-modify" => {
                let path = fixture.physical(operation["path"].as_str().unwrap());
                let previous = fs::metadata(&path).unwrap().modified().unwrap();
                fs::write(&path, operation["content"].as_str().unwrap()).unwrap();
                advance_mtime(&path, previous);
            }
            "external-rename" => {
                fs::rename(
                    fixture.physical(operation["path"].as_str().unwrap()),
                    fixture.physical(operation["to"].as_str().unwrap()),
                )
                .unwrap();
            }
            "external-delete" => {
                fs::remove_file(fixture.physical(operation["path"].as_str().unwrap())).unwrap();
            }
            "external-open" => {
                engine
                    .open_external_file(&fixture.outside(), OpenOptions::default())
                    .unwrap();
            }
            "external-close" => {
                engine
                    .open_file(FileTarget::path("Source.md"), OpenOptions::default())
                    .unwrap();
            }
            "offline-and-restart" => {
                engine.prepare_shutdown().unwrap();
                drop(engine);
                for offline in operation["operations"].as_array().unwrap() {
                    let path = fixture.physical(offline["path"].as_str().unwrap());
                    match offline["type"].as_str().unwrap() {
                        "modify" | "create" => {
                            fs::write(path, offline["content"].as_str().unwrap()).unwrap()
                        }
                        "delete" => fs::remove_file(path).unwrap(),
                        kind => panic!("unknown offline operation {kind}"),
                    }
                }
                engine = Engine::open(&fixture.vault()).unwrap();
                activate(&mut engine);
                assert_state(
                    &engine.metadata_lifecycle_snapshot(),
                    &stage["before"],
                    &selected,
                    &fixture,
                    &format!("{id}: post-startup before"),
                );
                cursor = engine.start_lifecycle_journal();
            }
            kind => panic!("unknown native operation {kind}"),
        }
        settle(&mut engine, &fixture, &selected, id);
        assert_events(
            engine.lifecycle_journal_since(cursor).unwrap(),
            &stage["result"]["events"],
            &selected,
            &fixture,
            id,
        );
        assert_state(
            &engine.metadata_lifecycle_snapshot(),
            &stage["result"]["state"],
            &selected,
            &fixture,
            &format!("{id}: settled"),
        );
        assert_bytes(&fixture, &stage["file_bytes"], id);
    }
}

#[test]
fn collection_is_passive_and_unchanged_read_refresh_emits_no_callbacks() {
    let fixture = Fixture::new();
    fs::write(fixture.vault().join("A.md"), "# Saved\n").unwrap();
    let mut engine = Engine::open(&fixture.vault()).unwrap();
    activate(&mut engine);
    let before = engine.metadata_lifecycle_snapshot();
    engine.start_lifecycle_journal();
    assert_eq!(engine.metadata_lifecycle_snapshot(), before);
    engine.stop_lifecycle_journal();
    assert_eq!(engine.metadata_lifecycle_snapshot(), before);
    let cursor = engine.start_lifecycle_journal();
    assert_eq!(
        engine.read_saved(FileTarget::path("A.md")).unwrap(),
        "# Saved\n"
    );
    engine.refresh_vault().unwrap();
    settle(&mut engine, &fixture, &["A.md".into()], "passive control");
    assert!(engine.lifecycle_journal_since(cursor).unwrap().is_empty());
    assert_eq!(engine.metadata_lifecycle_snapshot(), before);
    let selection = Selection {
        metadata_paths: vec!["A.md".into()],
        metadata_fields: vec!["readiness".into()],
        readiness_fields: vec!["metadata".into()],
        ..Selection::default()
    };
    let observed = serde_json::to_value(snapshot(&engine, &selection).unwrap()).unwrap();
    assert_eq!(observed["readiness"]["ready"], true);
    assert_eq!(
        observed["readiness"]["metadata"],
        serde_json::to_value(engine.metadata_readiness()).unwrap()
    );
}
