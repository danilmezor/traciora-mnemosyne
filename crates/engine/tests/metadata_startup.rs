//! Early startup listeners, mixed warm/cold cache work and command reachability.
//! Object member order is normalized; array/event order and UTF16 units remain
//! exact. Absolute native/candidate timestamps are compared by record relations.
use mnemosyne_engine::{
    events::{CallbackFrame, LifecycleEvent, MetadataLinkMap},
    json_units::JsonUnits,
    Engine, FileTarget, OpenOptions,
};
use serde::Serialize;
use serde_json::{json, value::RawValue, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

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

fn first_difference(actual: &JsonUnits, expected: &JsonUnits, path: &str) -> Option<String> {
    match (actual, expected) {
        (JsonUnits::Object(a), JsonUnits::Object(b)) => {
            for (key, value) in b {
                if !a.iter().any(|(candidate, _)| candidate == key) {
                    let key = String::from_utf16(key).unwrap_or_else(|_| format!("UTF16{key:?}"));
                    return Some(format!(
                        "{path}.{key}: absent in candidate; native {value:?}"
                    ));
                }
            }
            for (key, _) in a {
                if !b.iter().any(|(native, _)| native == key) {
                    let key = String::from_utf16(key).unwrap_or_else(|_| format!("UTF16{key:?}"));
                    return Some(format!("{path}.{key}: extra candidate key"));
                }
            }
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
            return Some(format!("{path}: actual {actual:?}, native {expected:?}"));
        }
        _ => {}
    }
    None
}

fn assert_lossless(actual: &str, expected: &str, label: &str) {
    let mut actual = JsonUnits::parse(actual).expect("candidate lossless JSON");
    let mut expected = JsonUnits::parse(expected).expect("native lossless JSON");
    canonical(&mut actual);
    canonical(&mut expected);
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
    files: Vec<String>,
    preload_promise_present: bool,
    layout_ready: bool,
}
fn links(map: &MetadataLinkMap, path: &str) -> Value {
    match map.get(path) {
        Some(links) => {
            json!({"present":true,"value":links.iter().map(|(path,count)|(path.clone(),*count)).collect::<BTreeMap<_,_>>()})
        }
        None => json!({"present":false,"value":null}),
    }
}
fn project(frame: &CallbackFrame, selected: &[String]) -> StateProjection {
    let mut state = StateProjection {
        cache: BTreeMap::new(),
        file_info: BTreeMap::new(),
        file_stat: BTreeMap::new(),
        info_stat_matches: BTreeMap::new(),
        resolved: BTreeMap::new(),
        unresolved: BTreeMap::new(),
        readiness: frame.readiness.clone(),
        files: frame.files.clone(),
        preload_promise_present: frame.preload_promise_present,
        layout_ready: frame.layout_ready,
    };
    for selected_path in selected {
        let path = selected_path.clone();
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
        state
            .resolved
            .insert(selected_path.clone(), links(&frame.resolved, &path));
        state
            .unresolved
            .insert(selected_path.clone(), links(&frame.unresolved, &path));
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
        .as_object_mut()
        .unwrap()
        .remove("file_record_identity");
    state
}
fn assert_state(frame: &CallbackFrame, expected: &Value, selected: &[String], label: &str) {
    let actual = serde_json::to_string(&project(frame, selected)).unwrap();
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
fn assert_events(events: &[LifecycleEvent], expected: &Value, selected: &[String], label: &str) {
    assert_eq!(
        events.len(),
        expected.as_array().unwrap().len(),
        "{label}: callback count {:?}",
        events
            .iter()
            .map(|event| (&event.name, &event.path))
            .collect::<Vec<_>>()
    );
    for (index, (event, expected)) in events.iter().zip(expected.as_array().unwrap()).enumerate() {
        let actual = EventProjection {
            domain: event.domain,
            name: event.name.clone(),
            path: event.path.clone(),
            old_path: event.old_path.clone(),
            data: event.data.clone(),
            cache_argument: raw(event.cache_argument_json.as_ref()),
            state: project(&event.state, selected),
        };
        let mut expected = expected.clone();
        expected.as_object_mut().unwrap().remove("performance_ms");
        expected.as_object_mut().unwrap().remove("step");
        expected["state"] = native_state(expected["state"].take());
        expected.as_object_mut().unwrap().remove("kind");
        expected.as_object_mut().unwrap().remove("sequence");
        assert_lossless(
            &serde_json::to_string(&actual).unwrap(),
            &serde_json::to_string(&expected).unwrap(),
            &format!("{label}: callback {index}"),
        );
    }
}
fn expand_positions(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            if let Some(pos) = fields.remove("pos") {
                let p = pos.as_array().expect("native packed position");
                assert_eq!(p.len(), 6);
                fields.insert("position".into(), json!({"start":{"line":p[0],"col":p[1],"offset":p[2]},"end":{"line":p[3],"col":p[4],"offset":p[5]}}));
            }
            for value in fields.values_mut() {
                expand_positions(value);
            }
        }
        Value::Array(values) => {
            for value in values {
                expand_positions(value);
            }
        }
        _ => {}
    }
}
fn normalized_record(mut value: Value, native: bool) -> Value {
    if value.is_null() {
        return value;
    }
    let key = if native { "mtime" } else { "mtime_ms" };
    let zero = value.as_object_mut().unwrap().remove(key).unwrap() == 0;
    value["mtime_is_zero"] = json!(zero);
    value
}
fn assert_store(
    root: &std::path::Path,
    expected: &Value,
    selected: &[String],
    known_hashes: &BTreeSet<String>,
    label: &str,
) {
    let actual: Value =
        serde_json::from_slice(&fs::read(root.join(".mnemosyne/metadata-cache.json")).unwrap())
            .unwrap();
    for path in selected {
        assert_eq!(
            normalized_record(actual["files"][path].clone(), false),
            normalized_record(expected["stored_files"][path].clone(), true),
            "{label}: durable file record {path}"
        );
    }
    let mut candidate = BTreeMap::new();
    for (hash, record) in actual["metadata"].as_object().unwrap() {
        assert!(
            known_hashes.contains(hash),
            "{label}: unexpected candidate hash {hash}"
        );
        let mut cache =
            mnemosyne_core::cache::CachedMetadata::decode_persistent(record.clone()).unwrap();
        // Keep an actual persisted marker. Only the decoder's cold-load marker
        // is removed when the disk record itself has no enumerable v field.
        cache.version = record
            .get("v")
            .map(|value| serde_json::from_value::<u32>(value.clone()).unwrap());
        candidate.insert(hash.clone(), serde_json::to_value(cache).unwrap());
    }
    let mut native = BTreeMap::new();
    for (hash, cache) in expected["stored_metadata"].as_object().unwrap() {
        assert!(
            known_hashes.contains(hash),
            "{label}: native stored hash lacks a declared independently authored input: {hash}"
        );
        let mut cache = cache.clone();
        expand_positions(&mut cache);
        native.insert(hash.clone(), cache);
        // Bootstrap bytes and their create/delete history are explicit fixture
        // inputs, so the entire captured durable metadata store is compared.
    }
    assert_lossless(
        &serde_json::to_string(&candidate).unwrap(),
        &serde_json::to_string(&native).unwrap(),
        &format!("{label}: durable selected metadata hashes and fields"),
    );
}
fn assert_bytes(root: &std::path::Path, expected: &Value, label: &str) {
    for (path, expected) in expected.as_object().unwrap() {
        let actual = match fs::read(root.join(path)) {
            Ok(bytes) => serde_json::to_value(bytes).unwrap(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Value::Null,
            Err(error) => panic!("{label}: read {path}: {error}"),
        };
        assert_eq!(actual, *expected, "{label}: exact bytes {path}");
    }
}

struct Fixture(PathBuf);
impl Fixture {
    fn new(files: &Value) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        loop {
            let root = std::env::temp_dir().join(format!(
                "mn-meta-startup-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&root) {
                Ok(()) => {
                    for (path, content) in files.as_object().unwrap() {
                        fs::write(root.join(path), content.as_str().unwrap()).unwrap();
                    }
                    return Self(root);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create startup fixture {}: {error}", root.display()),
            }
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn json_units(text: &str) -> JsonUnits {
    let mut value = JsonUnits::parse(text).unwrap();
    canonical(&mut value);
    value
}
fn assert_phase_observations(states: &[CallbackFrame], rows: &Value, selected: &[String]) {
    let candidate: Vec<_> = states
        .iter()
        .map(|state| json_units(&serde_json::to_string(&project(state, selected)).unwrap()))
        .collect();
    let mut index = 0;
    for row in rows
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["kind"] != "event")
    {
        let expected =
            json_units(&serde_json::to_string(&native_state(row["state"].clone())).unwrap());
        // Transparent native wrappers can observe the same unchanged state
        // more than once; they do not correspond to extra Rust owner jobs.
        while index < candidate.len() && candidate[index] != expected {
            index += 1;
        }
        assert!(
            index < candidate.len(),
            "native phase {} has no chronological actual owner observation",
            row["kind"]
        );
    }
}
fn drive_metadata(engine: &mut Engine) {
    let now = Instant::now();
    for turn in 0..20 {
        engine
            .poll_metadata_lifecycle_at(now + Duration::from_millis(turn * 10))
            .unwrap();
    }
}
#[test]
fn mixed_startup_early_listeners_preserve_empty_inventory_warm_cache_and_pending_cold_work() {
    let evidence: Value =
        serde_json::from_str(include_str!("fixtures/metadata_startup.json")).unwrap();
    let selected: Vec<String> = serde_json::from_value(evidence["selected_paths"].clone()).unwrap();
    let preconditioning = &evidence["preconditioning"];
    let fixture = Fixture::new(&preconditioning["initial_bootstrap_files"]);
    let mut known_hashes = BTreeSet::new();
    for files in [
        &preconditioning["initial_bootstrap_files"],
        &evidence["initial_files"],
        &evidence["offline_files"],
    ] {
        for content in files.as_object().unwrap().values() {
            known_hashes.insert(hash(content.as_str().unwrap().as_bytes()));
        }
    }
    let mut warmup = Engine::open(&fixture.0).unwrap();
    for operation in preconditioning["operations"].as_array().unwrap() {
        let path = operation["path"].as_str().unwrap();
        match operation["type"].as_str().unwrap() {
            "create" => {
                warmup
                    .vault
                    .create(path, operation["content"].as_str().unwrap())
                    .unwrap();
                warmup.file_created(path);
            }
            "delete" => {
                warmup.vault.delete(path).unwrap();
                warmup.file_deleted(path);
            }
            kind => panic!("unknown native preconditioning operation {kind}"),
        }
    }
    // The fixture declares the warm source leaf, independently of expected
    // metadata. This uses normal workspace navigation and persistence.
    warmup
        .open_file(FileTarget::path("Warm.md"), OpenOptions::default())
        .unwrap();
    warmup.prepare_shutdown().unwrap();
    assert_store(
        &fixture.0,
        &evidence["precondition_committed"],
        &selected,
        &known_hashes,
        "real preconditioning",
    );
    drop(warmup);
    for (path, content) in evidence["offline_files"].as_object().unwrap() {
        fs::write(fixture.0.join(path), content.as_str().unwrap()).unwrap();
    }
    let mut engine = Engine::prepare_startup(&fixture.0).unwrap();
    engine.enable_metadata_lifecycle().unwrap();
    let cursor = engine.start_lifecycle_journal();
    let mut states = vec![engine.metadata_lifecycle_snapshot()];
    assert_state(
        &states[0],
        &evidence["cli_pending"]["rows"][0]["state"],
        &selected,
        "early empty owner",
    );
    engine.start_startup_loading().unwrap();
    states.push(engine.metadata_lifecycle_snapshot());
    loop {
        let layout_ready = engine.poll_startup().unwrap();
        states.push(engine.metadata_lifecycle_snapshot());
        if layout_ready {
            break;
        }
    }
    // Real independent resolution/writes proceed while worker delivery is held.
    engine.poll_metadata_resolution();
    engine.poll_metadata_persistence().unwrap();
    states.push(engine.metadata_lifecycle_snapshot());
    assert_state(
        states.last().unwrap(),
        &evidence["cli_pending"]["state"],
        &selected,
        "initialized CLI-accessible cold-pending owner",
    );
    assert_phase_observations(&states, &evidence["cli_pending"]["rows"], &selected);
    // Source-grounded authored control: preload inserts the existing Warm key
    // before initialize creates the Cold placeholder. This array was not a
    // separately captured field in the startup native fixture.
    assert_eq!(engine.cached_file_paths(), vec!["Warm.md", "Cold.md"]);
    assert_store(
        &fixture.0,
        &evidence["pending_committed"],
        &selected,
        &known_hashes,
        "pending committed",
    );
    // Normal saved reads leave cold indexing pending; actual CLI dispatch is
    // additionally proved by the independent process harness.
    for (path, content) in evidence["offline_files"].as_object().unwrap() {
        assert_eq!(
            engine.read_saved(FileTarget::path(path)).unwrap(),
            content.as_str().unwrap()
        );
    }
    assert_state(
        &engine.metadata_lifecycle_snapshot(),
        &evidence["cli_pending"]["state"],
        &selected,
        "after ordinary saved read",
    );
    drive_metadata(&mut engine);
    assert_state(
        &engine.metadata_lifecycle_snapshot(),
        &evidence["settled"]["state"],
        &selected,
        "settled startup",
    );
    let event_rows = Value::Array(
        evidence["settled"]["rows"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["kind"] == "event")
            .cloned()
            .collect(),
    );
    assert_events(
        engine.lifecycle_journal_since(cursor).unwrap(),
        &event_rows,
        &selected,
        "early listener complete callbacks",
    );
    assert_store(
        &fixture.0,
        &evidence["settled_committed"],
        &selected,
        &known_hashes,
        "settled committed",
    );
    assert_bytes(&fixture.0, &evidence["file_bytes"], "settled startup");
}
#[test]
fn normal_retained_startup_exposes_pending_work_without_any_collector() {
    let fixture = Fixture::new(&json!({"Guard.md":"# Guard\n"}));
    let mut engine = Engine::open_retained(&fixture.0).unwrap();
    let pending = engine.metadata_lifecycle_snapshot();
    assert!(pending.readiness.initialized && pending.layout_ready);
    assert!(!pending.readiness.is_cache_clean);
    assert_eq!(pending.readiness.in_progress, 1);
    assert!(pending
        .cache_json
        .get("Guard.md")
        .and_then(Option::as_ref)
        .is_none());
    assert_eq!(engine.lifecycle_cursor(), 0);
    drive_metadata(&mut engine);
    let settled = engine.metadata_lifecycle_snapshot();
    assert!(settled.readiness.is_cache_clean);
    assert!(settled
        .cache_json
        .get("Guard.md")
        .and_then(Option::as_ref)
        .is_some());
    assert_eq!(engine.lifecycle_cursor(), 0);
}
