use mnemosyne_engine::{
    external::{ExternalOpenResult, ExternalStat},
    host::{FilesystemOpenEffect, VaultHost},
    vault_registry::{FilesystemOpenAction, VaultRegistry},
    Engine, FileTarget, OpenOptions,
};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "mnemosyne-fileopen-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(path.join("vault")).unwrap();
        Self(path)
    }
    fn receipt(&self, name: &str) -> Value {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name);
        serde_json::from_str(
            &std::fs::read_to_string(path)
                .unwrap()
                .replace("$RUN", self.0.to_str().unwrap()),
        )
        .unwrap()
    }
    fn write(&self, path: &str, text: &str) {
        let path = self.0.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn strings(value: &Value) -> Vec<&str> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect()
}
fn snapshot(engine: &Engine) -> Value {
    let leaf = engine
        .session()
        .leaves
        .iter()
        .find(|leaf| Some(leaf.id.as_str()) == engine.session().active_leaf.as_deref());
    json!({"active":engine.active_file(),"type":leaf.map(|leaf|leaf.view_type.as_str()),
        "editor":engine.editor().map(|editor|json!({"text":editor.text,"cursor":editor.selection.head,"anchor":editor.selection.anchor,"head":editor.selection.head})),
        "leaves":engine.session().leaves.iter().map(|leaf|json!({"file":leaf.file,"type":leaf.view_type})).collect::<Vec<_>>(),"recents":engine.recents()})
}

#[test]
fn actual_twelve_filesystem_requests_preserve_all_three_vault_sessions() {
    let fixture = Fixture::new();
    let data = fixture.receipt("reference_filesystem_open.json");
    for (path, text) in data["seed_files"].as_object().unwrap() {
        fixture.write(path, text.as_str().unwrap());
    }
    std::fs::create_dir_all(fixture.0.join("Unregistered")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(
        fixture.0.join("External.md"),
        fixture.0.join("External-link.md"),
    )
    .unwrap();
    let registry = VaultRegistry::from_document(
        json!({"cli":true,"vaults":data["registry"]}),
        &fixture.0,
        None,
    )
    .unwrap();
    let mut host = VaultHost::from_registry(registry);
    for state in data["filesystem_cases"][0]["before"].as_array().unwrap() {
        let engine = host.open_vault(state["id"].as_str().unwrap()).unwrap();
        engine
            .open_file(FileTarget::path("ParityReady.md"), OpenOptions::default())
            .unwrap();
        let mut session = engine.session().clone();
        session.last_open_files = serde_json::from_value(state["recents"].clone()).unwrap();
        engine.restore(session).unwrap();
    }
    for case in data["filesystem_cases"].as_array().unwrap() {
        let id = case["id"].as_str().unwrap();
        if id == "outside-file-focused-peer" {
            // Controlled native focus emission was used in the fixture. This is
            // an imported event, not an assumption that a focus request landed.
            assert!(host.record_focus("f1ea000000000001", 100));
            assert!(host.record_focus(data["ids"]["peer"].as_str().unwrap(), 200));
        }
        let path = Path::new(case["path"].as_str().unwrap());
        let plan = host.plan_filesystem_open(path);
        match id {
            "nested-file" | "relative-nested-file" => assert!(
                matches!(plan.action,FilesystemOpenAction::OpenVaultFile{ref vault_id,..} if vault_id==data["ids"]["nested"].as_str().unwrap())
            ),
            "unregistered-dir" => assert_eq!(plan.action, FilesystemOpenAction::Noop),
            _ => {}
        }
        let result = host.open_filesystem_path(path).unwrap();
        if id == "plain-text-file" {
            assert!(matches!(
                result.effect,
                FilesystemOpenEffect::OpenWithDefaultApplication { .. }
            ));
        }
        if id == "outside-text-focused-peer" {
            assert!(matches!(
                result.effect,
                FilesystemOpenEffect::UnsupportedExternalExtension { .. }
            ));
        }
        if id == "outside-symlink-no-transient-file" {
            assert!(matches!(
                result.effect,
                FilesystemOpenEffect::ExternalFileMissing { .. }
            ));
        }
        for expected in case["after"].as_array().unwrap() {
            let actual = snapshot(host.engine(expected["id"].as_str().unwrap()).unwrap());
            let mut expected = expected.clone();
            expected.as_object_mut().unwrap().shift_remove("id");
            // The literal-hash nonexisting subpath is retained as an explicit
            // pending input. It produced no cursor displacement in the capture.
            assert_eq!(actual, expected, "{id}");
        }
    }
}

#[test]
fn actual_external_markdown_lifecycle_metadata_and_saved_read_boundaries() {
    let fixture = Fixture::new();
    let data = fixture.receipt("reference_external_graph.json");
    fixture.write("vault/ParityReady.md", "Isolated parity oracle.\n");
    fixture.write(
        "External.md",
        data["initial_external_source"].as_str().unwrap(),
    );
    let mut engine = Engine::open(fixture.0.join("vault")).unwrap();
    let real = fixture.0.join("External.md");
    let identity = format!("file:{}", real.display());
    engine
        .open_file(FileTarget::path("ParityReady.md"), OpenOptions::default())
        .unwrap();
    assert_eq!(
        engine
            .open_external_file(&real, OpenOptions::default())
            .unwrap(),
        ExternalOpenResult::Opened(identity.clone())
    );
    // Internal read_saved is the raw adapter boundary; native CLI/read wrappers
    // strip exactly one initial BOM, already covered by their process fixtures.
    assert_eq!(
        engine.read_saved(FileTarget::default()).unwrap(),
        data["initial_external_source"].as_str().unwrap()
    );
    assert_eq!(
        engine.read_saved(FileTarget::path(&identity)).unwrap(),
        data["initial_external_source"].as_str().unwrap()
    );
    for case in data["external_cases"].as_array().unwrap() {
        let id = case["id"].as_str().unwrap();
        match id {
            "opened" => {}
            "saved-modify" => engine
                .modify_external_saved(&identity, "# Updated\nsaved\n")
                .unwrap(),
            "external-write-immediate" => {
                engine
                    .editor_mut()
                    .unwrap()
                    .set_value("dirty body\n".into());
                fixture.write("External.md", "# External edit\nnew saved\n");
                engine.refresh_external_files().unwrap();
            }
            "external-write-after-monitor" => engine.refresh_external_files().unwrap(),
            "closed-external-leaf" => {
                engine
                    .open_file(FileTarget::path("ParityReady.md"), OpenOptions::default())
                    .unwrap();
            }
            "reopened" => {
                engine
                    .open_external_file(&real, OpenOptions::default())
                    .unwrap();
            }
            _ => panic!("{id}"),
        }
        let state = &case["state"];
        assert_eq!(engine.active_file(), state["active"].as_str(), "{id}");
        assert_eq!(
            engine.editor().unwrap().text,
            state["editor"]["text"],
            "{id}"
        );
        assert_eq!(
            serde_json::to_value(engine.editor().unwrap().selection.head).unwrap(),
            state["editor"]["cursor"],
            "{id}"
        );
        let file = engine.external_file(&identity).unwrap();
        assert_eq!(
            file.users,
            state["external_users"].as_u64().unwrap() as usize,
            "{id}"
        );
        assert_eq!(
            serde_json::to_value(&file.metadata).unwrap(),
            state["external_metadata"],
            "{id}"
        );
        assert_eq!(
            serde_json::to_value(engine.file_cache(&identity)).unwrap(),
            state["cache"],
            "{id}"
        );
        assert_eq!(
            engine.cached_file_paths(),
            strings(&state["fileCacheKeys"]),
            "{id}"
        );
        assert_eq!(
            engine.resolved_link_paths(),
            strings(&state["resolvedKeys"]),
            "{id}"
        );
        assert_eq!(
            file.stat,
            ExternalStat::from_metadata(&std::fs::symlink_metadata(&real).unwrap()).unwrap(),
            "{id}"
        );
        assert_eq!(
            file.stat.size,
            state["fs"]["size"]
                .as_str()
                .unwrap()
                .parse::<u64>()
                .unwrap(),
            "{id}"
        );
    }
}

#[test]
fn private_session_restore_rebinds_external_users_and_retains_unsaved_buffer() {
    // Our implementation format, not a native workspace/crash-save fixture.
    let fixture = Fixture::new();
    fixture.write("vault/Note.md", "saved internal\n");
    fixture.write("External.md", "saved external\n");
    let real = fixture.0.join("External.md");
    let identity = format!("file:{}", real.display());
    let mut engine = Engine::open(fixture.0.join("vault")).unwrap();
    engine
        .open_external_file(&real, OpenOptions::default())
        .unwrap();
    engine
        .editor_mut()
        .unwrap()
        .set_value("dirty private buffer\n".into());
    let mut restored = Engine::open(fixture.0.join("vault")).unwrap();
    restored.restore(engine.session().clone()).unwrap();
    assert_eq!(restored.active_file(), Some(identity.as_str()));
    assert_eq!(restored.editor().unwrap().text, "dirty private buffer\n");
    assert_eq!(
        restored.read_saved(FileTarget::default()).unwrap(),
        "saved external\n"
    );
    assert_eq!(restored.external_file(&identity).unwrap().users, 1);
    assert!(restored.file_cache(&identity).is_some());
    assert!(!restored.cached_file_paths().contains(&identity.as_str()));
    restored
        .open_file(FileTarget::path("Note.md"), OpenOptions::default())
        .unwrap();
    assert_eq!(restored.external_file(&identity).unwrap().users, 0);
    assert!(restored.file_cache(&identity).is_none());
}

#[test]
fn actual_sixteen_filesystem_hash_subpaths_apply_markdown_cursor_or_retain_missing_target_cursor() {
    let fixture = Fixture::new();
    let data = fixture.receipt("reference_subpath_open.json");
    fixture.write("vault/Target.md", data["source"].as_str().unwrap());
    let registry = VaultRegistry::from_document(
        json!({"vaults":{"owned":{"path":fixture.0.join("vault")}}}),
        &fixture.0,
        None,
    )
    .unwrap();
    let mut host = VaultHost::from_registry(registry);
    for case in data["cases"].as_array().unwrap() {
        let physical = case["physical_file"].as_str().unwrap();
        fixture.write(&format!("vault/{physical}"), "literal physical filename\n");
        let engine = host.open_vault("owned").unwrap();
        engine
            .open_file(FileTarget::path("Target.md"), OpenOptions::default())
            .unwrap();
        let selection = mnemosyne_engine::EditorSelection {
            anchor: serde_json::from_value(case["before"]["editor"]["anchor"].clone()).unwrap(),
            head: serde_json::from_value(case["before"]["editor"]["head"].clone()).unwrap(),
        };
        engine
            .editor_mut()
            .unwrap()
            .set_selection(selection)
            .unwrap();
        let outcome = host
            .open_filesystem_path(&fixture.0.join("vault").join(physical))
            .unwrap();
        assert!(
            matches!(outcome.effect,FilesystemOpenEffect::Opened{ref file,pending_subpath:None,..} if file=="Target.md"),
            "{}",
            case["fragment"]
        );
        let engine = host.engine("owned").unwrap();
        let editor = engine.editor().unwrap();
        let after = &case["after"];
        assert_eq!(
            engine.active_file(),
            after["active"].as_str(),
            "{}",
            case["fragment"]
        );
        assert_eq!(editor.text, after["editor"]["text"], "{}", case["fragment"]);
        assert_eq!(
            serde_json::to_value(editor.selection.head).unwrap(),
            after["editor"]["cursor"],
            "{}",
            case["fragment"]
        );
        assert_eq!(
            serde_json::to_value(editor.selection.anchor).unwrap(),
            after["editor"]["anchor"],
            "{}",
            case["fragment"]
        );
        assert_eq!(
            serde_json::to_value(engine.file_cache("Target.md")).unwrap(),
            after["metadata"],
            "{} saved metadata",
            case["fragment"]
        );
    }
}
