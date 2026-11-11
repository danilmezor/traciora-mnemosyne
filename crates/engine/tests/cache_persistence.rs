//! Native cache lifecycle replay plus authored private-store recovery and typed
//! value invariants. Private corruption/revision controls are not native proof.
use mnemosyne_core::yaml_value::Node;
use mnemosyne_engine::{json_units::JsonUnits, Engine};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        loop {
            let path = std::env::temp_dir().join(format!(
                "mn-cache-{}-{}",
                std::process::id(),
                NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("cannot create cache fixture {}: {error}", path.display()),
            }
        }
    }
    fn store(&self) -> PathBuf {
        self.0.join(".mnemosyne/metadata-cache.json")
    }
    fn write(&self, path: &str, content: &str) {
        fs::write(self.0.join(path), content).unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn cache(engine: &Engine, path: &str) -> Value {
    serde_json::to_value(engine.file_cache(path).unwrap()).unwrap()
}
fn store(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

// Compare every JSON key/value losslessly; only object field order is normalized.
fn lossless_json(text: &str) -> JsonUnits {
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
    let mut value = JsonUnits::parse(text).unwrap();
    canonical(&mut value);
    value
}
#[derive(Deserialize)]
struct TypedFixture {
    identity: Value,
    initial_files: std::collections::BTreeMap<String, String>,
    steps: Vec<TypedStage>,
}
#[derive(Deserialize)]
struct TypedStage {
    id: String,
    operation: Value,
    file_bytes: std::collections::BTreeMap<String, Vec<u8>>,
    typed: TypedState,
}
#[derive(Deserialize)]
struct TypedState {
    cache_json: String,
    live_descriptor: Box<serde_json::value::RawValue>,
    aliases: Value,
}
#[derive(Deserialize)]
struct NativeDescriptor {
    #[serde(rename = "type")]
    kind: String,
    constructor: Option<String>,
    value: Option<Value>,
    bytes: Option<Vec<u8>>,
    #[serde(default)]
    properties: Vec<NativeProperty>,
}
#[derive(Deserialize)]
struct NativeProperty {
    key_units: Vec<u16>,
    value: Box<serde_json::value::RawValue>,
}

#[test]
fn native_typed_cache_json_clone_changes_constructors_and_aliases_after_restart() {
    // RawValue/explicit key_units avoid decoding the native lone surrogate into
    // a Rust String or replacing it with U+FFFD.
    let evidence: TypedFixture =
        serde_json::from_str(include_str!("fixtures/cache_typed.json")).unwrap();
    assert_eq!(evidence.identity["reference_version"], "1.14.4");
    assert_eq!(evidence.steps.len(), 2);
    let fixture = Fixture::new();
    let mut engine = None;
    for stage in evidence.steps {
        match stage.operation["type"].as_str().unwrap() {
            "create-initial-files" => {
                for (path, content) in &evidence.initial_files {
                    fixture.write(path, content);
                }
                engine = Some(Engine::open(&fixture.0).unwrap());
            }
            "restart" => {
                drop(engine.take());
                engine = Some(Engine::open(&fixture.0).unwrap());
            }
            kind => panic!("unknown typed fixture operation {kind}"),
        }
        let engine = engine.as_ref().unwrap();
        let cached = engine.file_cache("Typed.md").unwrap();
        let actual_json = serde_json::to_string(cached).unwrap();
        assert_eq!(
            lossless_json(&actual_json),
            lossless_json(&stage.typed.cache_json),
            "{}: full public cache including lone UTF16 key",
            stage.id
        );
        for (path, expected) in &stage.file_bytes {
            assert_eq!(
                fs::read(fixture.0.join(path)).unwrap(),
                *expected,
                "{}: saved bytes {path}",
                stage.id
            );
        }
        let graph = cached.native_frontmatter.as_ref().unwrap();
        let native: NativeDescriptor =
            serde_json::from_str(stage.typed.live_descriptor.get()).unwrap();
        let fm = native
            .properties
            .iter()
            .find(|property| property.key_units == "frontmatter".encode_utf16().collect::<Vec<_>>())
            .unwrap();
        let fm: NativeDescriptor = serde_json::from_str(fm.value.get()).unwrap();
        assert_eq!(
            graph.own_keys_units(),
            fm.properties
                .iter()
                .map(|property| property.key_units.clone())
                .collect::<Vec<_>>(),
            "{}: all frontmatter key units",
            stage.id
        );
        assert_eq!(
            graph.own_property("shared") == graph.own_property("alias"),
            stage.typed.aliases["shared_alias_strict"]
                .as_bool()
                .unwrap(),
            "{}: alias identity",
            stage.id
        );
        for property in fm.properties {
            let expected: NativeDescriptor = serde_json::from_str(property.value.get()).unwrap();
            let id = graph.own_property_units(&property.key_units).unwrap();
            let node = &graph.nodes[id];
            match expected.kind.as_str() {
                "ref" => {} // Strict alias identity is checked above.
                "object" => {
                    let constructor = match node {
                        Node::Date(_) => "Date",
                        Node::Binary(_) => "Uint8Array",
                        Node::Set(_) => "Set",
                        Node::Map(_) => "Map",
                        Node::Object(_) | Node::ObjectWithUtf16Keys(_) => "Object",
                        node => panic!("{}: expected object, got {node:?}", stage.id),
                    };
                    assert_eq!(
                        Some(constructor),
                        expected.constructor.as_deref(),
                        "{}: constructor for {:?}",
                        stage.id,
                        property.key_units
                    );
                    if let Some(bytes) = expected.bytes {
                        assert!(matches!(node, Node::Binary(actual) if *actual == bytes));
                    }
                }
                "number" => {
                    let Node::Number(number) = node else {
                        panic!("{}: expected number {node:?}", stage.id)
                    };
                    let number = number.as_f64().unwrap();
                    match expected.value.unwrap().as_str().unwrap() {
                        "Infinity" => assert_eq!(number, f64::INFINITY),
                        "-Infinity" => assert_eq!(number, f64::NEG_INFINITY),
                        "NaN" => assert!(number.is_nan()),
                        value => panic!("unexpected native numeric descriptor {value}"),
                    }
                }
                "null" => assert!(matches!(node, Node::Null)),
                "string" => assert_eq!(
                    graph.javascript_units(id).unwrap(),
                    expected
                        .value
                        .unwrap()
                        .as_str()
                        .unwrap()
                        .encode_utf16()
                        .collect::<Vec<_>>()
                ),
                kind => panic!("unexpected native descriptor kind {kind}"),
            }
        }
    }
}

#[test]
fn authored_corrupt_or_incompatible_private_store_recovers_from_saved_source() {
    for damage in [
        "truncated",
        "schema",
        "parser",
        "record",
        "frontmatter-json",
        "frontmatter-yaml",
        "frontmatter-type",
    ] {
        let fixture = Fixture::new();
        let source = "---\nname: saved\n---\n# Saved\n[[Target]] #saved\n";
        fixture.write("A.md", source);
        let engine = Engine::open(&fixture.0).unwrap();
        let expected = cache(&engine, "A.md");
        assert!(expected.get("v").is_none(), "fresh parsed metadata");
        drop(engine);
        let mut persisted = store(&fixture.store());
        match damage {
            "truncated" => fs::write(fixture.store(), b"{\"schema_version\":").unwrap(),
            "schema" => {
                persisted["schema_version"] = json!(999);
                fs::write(fixture.store(), serde_json::to_vec(&persisted).unwrap()).unwrap();
            }
            "parser" => {
                persisted["parser_revision"] = json!(999);
                fs::write(fixture.store(), serde_json::to_vec(&persisted).unwrap()).unwrap();
            }
            "record" => {
                let metadata = persisted["metadata"].as_object_mut().unwrap();
                assert_eq!(metadata.len(), 1);
                *metadata.values_mut().next().unwrap() = Value::Null;
                fs::write(fixture.store(), serde_json::to_vec(&persisted).unwrap()).unwrap();
            }
            "frontmatter-json" | "frontmatter-yaml" | "frontmatter-type" => {
                let frontmatter = persisted["metadata"]
                    .as_object_mut()
                    .unwrap()
                    .values_mut()
                    .next()
                    .unwrap()
                    .get_mut("native_frontmatter")
                    .unwrap();
                *frontmatter = match damage {
                    "frontmatter-json" => json!("{"),
                    "frontmatter-yaml" => json!("name: saved"),
                    "frontmatter-type" => json!({"name":"saved"}),
                    _ => unreachable!(),
                };
                fs::write(fixture.store(), serde_json::to_vec(&persisted).unwrap()).unwrap();
            }
            _ => unreachable!(),
        }
        let reopened = Engine::open(&fixture.0).unwrap();
        assert_eq!(
            cache(&reopened, "A.md"),
            expected,
            "{damage}: recover saved metadata without a false hydration marker"
        );
        assert_eq!(fs::read_to_string(fixture.0.join("A.md")).unwrap(), source);
        let repaired = store(&fixture.store());
        assert_eq!(repaired["schema_version"], 1);
        assert_eq!(repaired["parser_revision"], 1);
        assert!(repaired["metadata"]
            .as_object()
            .is_some_and(|records| !records.is_empty()));
    }
}

fn apply_offline_operation(root: &Path, operation: &Value) {
    let path = root.join(operation["path"].as_str().unwrap());
    match operation["type"].as_str().unwrap() {
        "modify-preserve-stat" => {
            // Preserve the candidate's own stat, never import a native timestamp.
            let before = fs::metadata(&path).unwrap();
            fs::write(&path, operation["content"].as_str().unwrap()).unwrap();
            assert_eq!(fs::metadata(&path).unwrap().len(), before.len());
            let times = fs::FileTimes::new()
                .set_accessed(before.accessed().unwrap())
                .set_modified(before.modified().unwrap());
            fs::OpenOptions::new()
                .write(true)
                .open(path)
                .unwrap()
                .set_times(times)
                .unwrap();
        }
        "modify" | "create" => fs::write(path, operation["content"].as_str().unwrap()).unwrap(),
        "delete" => fs::remove_file(path).unwrap(),
        kind => panic!("unknown offline fixture operation {kind}"),
    }
}

#[test]
fn native_eight_stage_cache_lifecycle_matches_full_cache_objects_and_saved_bytes() {
    let evidence: Value =
        serde_json::from_str(include_str!("fixtures/cache_persistence.json")).unwrap();
    assert_eq!(evidence["identity"]["reference_version"], "1.14.4");
    assert_eq!(
        evidence["identity"]["reference_app_js_sha256"],
        "958ce57fe22f87fdf121546e46dfccd9c8ee971ce3fd8f801d60d4ece39a2df9"
    );
    assert_eq!(evidence["steps"].as_array().unwrap().len(), 8);
    let fixture = Fixture::new();
    let mut engine = None;
    for stage in evidence["steps"].as_array().unwrap() {
        let id = stage["id"].as_str().unwrap();
        let operation = &stage["operation"];
        match operation["type"].as_str().unwrap() {
            "create-initial-files" => {
                for (path, content) in evidence["initial_files"].as_object().unwrap() {
                    fixture.write(path, content.as_str().unwrap());
                }
                engine = Some(Engine::open(&fixture.0).unwrap());
            }
            "restart" => {
                drop(engine.take());
                engine = Some(Engine::open(&fixture.0).unwrap());
            }
            "external-touch" => {
                let path = fixture.0.join(operation["path"].as_str().unwrap());
                let metadata = fs::metadata(&path).unwrap();
                let times = fs::FileTimes::new()
                    .set_modified(metadata.modified().unwrap() + std::time::Duration::from_secs(1));
                fs::OpenOptions::new()
                    .write(true)
                    .open(path)
                    .unwrap()
                    .set_times(times)
                    .unwrap();
                engine.as_mut().unwrap().refresh_vault().unwrap();
            }
            "modify" => {
                let engine = engine.as_mut().unwrap();
                let path = operation["path"].as_str().unwrap();
                engine
                    .vault
                    .modify(path, operation["content"].as_str().unwrap())
                    .unwrap();
                engine.file_modified(path);
            }
            "rename" => {
                let engine = engine.as_mut().unwrap();
                let path = operation["path"].as_str().unwrap();
                let to = operation["to"].as_str().unwrap();
                engine.vault.rename(path, to).unwrap();
                engine.file_renamed(path, to);
            }
            "delete" => {
                let engine = engine.as_mut().unwrap();
                let path = operation["path"].as_str().unwrap();
                engine.vault.delete(path).unwrap();
                engine.file_deleted(path);
            }
            "offline-and-restart" => {
                drop(engine.take());
                for offline in operation["operations"].as_array().unwrap() {
                    apply_offline_operation(&fixture.0, offline);
                }
                engine = Some(Engine::open(&fixture.0).unwrap());
            }
            kind => panic!("unknown fixture operation {kind}"),
        }
        let engine = engine.as_ref().unwrap();
        let mut actual_cache = serde_json::Map::new();
        let mut actual_bytes = serde_json::Map::new();
        for path in evidence["selected_paths"].as_array().unwrap() {
            let path = path.as_str().unwrap();
            actual_cache.insert(
                path.into(),
                serde_json::to_value(engine.file_cache(path)).unwrap(),
            );
            let bytes = match fs::read(fixture.0.join(path)) {
                Ok(bytes) => serde_json::to_value(bytes).unwrap(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Value::Null,
                Err(error) => panic!("{id}: cannot read {path}: {error}"),
            };
            actual_bytes.insert(path.into(), bytes);
        }
        assert_eq!(
            Value::Object(actual_cache),
            stage["result"]["cache"],
            "{id}: full selected cache objects"
        );
        assert_eq!(
            Value::Object(actual_bytes),
            stage["file_bytes"],
            "{id}: exact selected saved file bytes"
        );
    }
}

#[test]
fn authored_fresh_hash_uses_the_same_saved_bytes_as_metadata_despite_stale_vault_snapshot() {
    let fixture = Fixture::new();
    fixture.write("A.md", "# Old\n");
    let mut engine = Engine::open(&fixture.0).unwrap();
    fixture.write("A.md", "# New saved heading\n");
    assert_eq!(engine.vault.get("A.md").unwrap().source, "# Old\n");
    // This deliberately retains the old Note while the metadata store reads
    // new saved bytes. Different lengths force stat invalidation without timing.
    engine.refresh_cache();
    assert_eq!(
        cache(&engine, "A.md")["headings"][0]["heading"],
        "New saved heading"
    );
    drop(engine);
    let reopened = Engine::open(&fixture.0).unwrap();
    assert_eq!(
        cache(&reopened, "A.md")["headings"][0]["heading"],
        "New saved heading"
    );
    assert_eq!(
        fs::read_to_string(fixture.0.join("A.md")).unwrap(),
        "# New saved heading\n"
    );
}
