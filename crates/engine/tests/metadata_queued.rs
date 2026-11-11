//! Five actual queued-work cases and their restart endpoint.
//! Object member order is normalized; array/event order and UTF16 units remain
//! exact. Absolute native/candidate timestamps are compared by record relations.
use mnemosyne_engine::{
    events::{CallbackFrame, LifecycleEvent, MetadataLinkMap},
    json_units::JsonUnits,
    Engine,
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

// These native instrumented JS pointer descriptors remain raw evidence, not a
// Rust SDK object-identity certificate: file_is_current/file_is_original,
// gate_start_stat_is_current, enqueued_stat_is_current, held_is_original/current.
// Stat values and queue counts are covered by owner frames; retained-file
// behavior by callback current paths, lookup absence and captured-key effects.
fn assert_record_alias(engine: &Engine, expected_state: &Value, label: &str) {
    let facts = &expected_state["file_record_identity"];
    if facts.is_null() {
        return;
    } // Before instrumentation there is no held record.
    let original = facts["original_path"].as_str().unwrap();
    let current = facts["current_path"].as_str().unwrap();
    let expected = if facts["original_present"] == true && facts["current_present"] == true {
        Some(facts["original_is_current"].as_bool().unwrap())
    } else {
        None
    };
    assert_eq!(
        engine.metadata_file_records_share_identity(original, current),
        expected,
        "{label}: live record alias identity"
    );
}
fn assert_owner(engine: &Engine, expected: &Value, selected: &[String], label: &str) {
    assert_state(
        &engine.metadata_lifecycle_snapshot(),
        expected,
        selected,
        label,
    );
    assert_record_alias(engine, expected, label);
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
        assert_lossless(
            &serde_json::to_string(&actual).unwrap(),
            &serde_json::to_string(&expected).unwrap(),
            &format!("{label}: callback {index}"),
        );
    }
}
struct Fixture(PathBuf);
impl Fixture {
    fn new(initial: &Value) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        loop {
            let root = std::env::temp_dir().join(format!(
                "mn-meta-queued-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&root) {
                Ok(()) => {
                    for (path, content) in initial.as_object().unwrap() {
                        fs::write(root.join(path), content.as_str().unwrap()).unwrap();
                    }
                    return Self(root);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create queued fixture {}: {error}", root.display()),
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
fn apply(engine: &mut Engine, root: &std::path::Path, path: &mut String, action: &Value) {
    match action["type"].as_str().unwrap() {
        "modify" => {
            let target = root.join(&*path);
            let previous = fs::metadata(&target).unwrap().modified().unwrap();
            engine
                .vault
                .modify(path, action["content"].as_str().unwrap())
                .unwrap();
            fs::OpenOptions::new()
                .write(true)
                .open(&target)
                .unwrap()
                .set_times(fs::FileTimes::new().set_modified(previous + Duration::from_millis(1)))
                .unwrap();
            engine.file_modified(path);
        }
        "rename" => {
            let to = action["to"].as_str().unwrap();
            fs::rename(root.join(&*path), root.join(to)).unwrap();
            engine.vault.reconcile_path_from_disk(path).unwrap();
            engine.vault.reconcile_path_from_disk(to).unwrap();
            engine.file_renamed(path, to);
            *path = to.into();
        }
        "delete" => {
            engine.vault.delete(path).unwrap();
            engine.file_deleted(path);
        }
        kind => panic!("unknown captured queued action {kind}"),
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
        if known_hashes.contains(hash) {
            let mut cache = cache.clone();
            expand_positions(&mut cache);
            native.insert(hash.clone(), cache);
        }
        // Native bootstrap-only hashes stay in raw evidence. They were never
        // authored operation inputs and are not imported into the candidate.
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
fn drain_observed_work(engine: &mut Engine) {
    // Eight native seconds are a bounded observation, not a clean-readiness
    // requirement. Ordinary task completion and the source finished timer are
    // driven without clearing a resolver that actually remains running.
    let baseline = Instant::now();
    for turn in 0..20 {
        engine
            .poll_metadata_lifecycle_at(baseline + Duration::from_millis(turn * 10))
            .unwrap();
    }
}
#[test]
fn five_native_queued_cases_preserve_current_file_paths_captured_keys_and_durable_snapshots() {
    let evidence: Value =
        serde_json::from_str(include_str!("fixtures/metadata_queued.json")).unwrap();
    let selected: Vec<String> = serde_json::from_value(evidence["selected_paths"].clone()).unwrap();
    let mut known_hashes = BTreeSet::new();
    for content in evidence["initial_files"].as_object().unwrap().values() {
        known_hashes.insert(hash(content.as_str().unwrap().as_bytes()));
    }
    for step in evidence["steps"].as_array().unwrap() {
        for action in step["operation"]["actions"].as_array().unwrap() {
            if let Some(content) = action["content"].as_str() {
                known_hashes.insert(hash(content.as_bytes()));
            }
        }
    }
    let fixture = Fixture::new(&evidence["initial_files"]);
    let mut engine = Engine::open(&fixture.0).unwrap();
    engine.enable_metadata_lifecycle().unwrap();
    for step in evidence["steps"].as_array().unwrap() {
        let id = step["id"].as_str().unwrap();
        assert_owner(
            &engine,
            &step["before"],
            &selected,
            &format!("{id}: before"),
        );
        let cursor = engine.start_lifecycle_journal();
        let operation = &step["operation"];
        let mut path = operation["path"].as_str().unwrap().to_string();
        for (index, action) in operation["actions"].as_array().unwrap().iter().enumerate() {
            apply(&mut engine, &fixture.0, &mut path, action);
            assert_owner(
                &engine,
                &step["action_frames"][index]["state"],
                &selected,
                &format!("{id}: action {index}"),
            );
            if index == 0 && operation["type"] == "controlled-read-delivery" {
                engine.poll_metadata_lifecycle().unwrap();
                assert!(
                    !engine
                        .lifecycle_journal_since(cursor)
                        .unwrap()
                        .iter()
                        .any(|event| event.name == "changed"),
                    "{id}: read phase must precede delivery"
                );
            }
        }
        // Native's independent resolver and requested persistence settle while
        // worker delivery remains held. These are the production owner pumps.
        engine.poll_metadata_resolution();
        engine.poll_metadata_persistence().unwrap();
        assert_owner(
            &engine,
            &step["mutation_gate"]["state"],
            &selected,
            &format!("{id}: before release"),
        );
        assert_store(
            &fixture.0,
            &step["mutation_gate"],
            &selected,
            &known_hashes,
            &format!("{id}: before release"),
        );
        drain_observed_work(&mut engine);
        assert_owner(
            &engine,
            &step["result"]["state"],
            &selected,
            &format!("{id}: endpoint"),
        );
        assert_events(
            engine.lifecycle_journal_since(cursor).unwrap(),
            &step["result"]["events"],
            &selected,
            id,
        );
        if !step["committed"].is_null() {
            assert_store(
                &fixture.0,
                &step["committed"],
                &selected,
                &known_hashes,
                &format!("{id}: committed"),
            );
        } else {
            assert_eq!(
                id, "read-delete",
                "only native row5 lacks the post-release IDB capture"
            );
        }
        if let Some(bytes) = step.get("file_bytes") {
            assert_bytes(&fixture.0, bytes, id);
        } else {
            assert_eq!(
                id, "read-delete",
                "only native row5 lacks endpoint byte capture"
            );
        }
    }
    engine.prepare_shutdown().unwrap();
    drop(engine);
    let engine = Engine::open(&fixture.0).unwrap();
    let restart = &evidence["restart_endpoint"];
    assert_owner(
        &engine,
        &restart["restart_endpoint"]["state"],
        &selected,
        "restart endpoint",
    );
    assert!(
        restart["restart_endpoint"]["events"]
            .as_array()
            .unwrap()
            .is_empty(),
        "native restart listeners capture endpoints, not bootstrap chronology"
    );
    assert_store(
        &fixture.0,
        &restart["committed"],
        &selected,
        &known_hashes,
        "restart committed",
    );
    assert_bytes(&fixture.0, &restart["file_bytes"], "restart endpoint");
}
