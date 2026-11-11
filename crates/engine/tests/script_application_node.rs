#![cfg(unix)]
//! Opt-in real-renderer replay. Ordinary Cargo tests never launch it.
use mnemosyne_engine::{script_application::ScriptApplication, Engine};
use mnemosyne_runtime::{NodeChild, NodeChildConfig, NodeIdentity, RendererConfig};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[test]
#[ignore = "requires pinned upstream Electron43.7.7, remote2.1.2 and built Rust addon"]
fn engine_application_services_replay_native_eval_and_adapter_receivers() {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let proof = repo.join("target/rea/runtime-eval/electron-node");
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let report = repo.join(format!("target/rea/sdk-api/engine-application-{stamp}"));
    fs::create_dir(&report).unwrap();
    let vault = report.join("vault");
    fs::create_dir(&vault).unwrap();
    fs::write(vault.join("ParityReady.md"), "Isolated parity oracle.").unwrap();
    let engine = Engine::open(&vault).unwrap();
    let addon = report.join("addon.node");
    fs::copy(
        repo.join("target/runtime-build/debug/libmnemosyne_runtime_addon.dylib"),
        &addon,
    )
    .unwrap();
    let remote: Value =
        serde_json::from_slice(&fs::read(proof.join("remote-2.1.2/provenance.json")).unwrap())
            .unwrap();
    let runtime = NodeChild::launch_renderer(NodeChildConfig {
        executable: proof.join("dist/Electron.app/Contents/MacOS/Electron"),
        framework: proof.join("dist/Electron.app/Contents/Frameworks/Electron Framework.framework/Electron Framework"),
        addon,
        profile: report.join("profile"),
        vault: vault.clone(),
        identity: NodeIdentity {
            executable_sha256: "1121571fb0838d50519c2f002780b2b69f285d37993cf5622ee4b88c6f4b5ce4".into(),
            framework_sha256: "be1b9531ce2218021027aab470e1c188c975d1525b176992c160499cb860838c".into(),
            versions: BTreeMap::from([
                ("node".into(), "24.21.0".into()), ("electron".into(), "43.7.7".into()),
                ("v8".into(), "15.0.245.31-electron.0".into()), ("icu".into(), "78.2".into()),
            ]),
        },
        context: stamp as u64,
        startup_timeout: Duration::from_secs(20),
    }, RendererConfig {
        remote_package: proof.join("remote-2.1.2/package"),
        remote_files_sha256: remote["files"].as_object().unwrap().iter()
            .map(|(key, value)| (key.clone(), value.as_str().unwrap().to_owned())).collect(),
    }).unwrap();
    let mut app = ScriptApplication::install(engine, runtime).unwrap();
    let corpus: Value = serde_json::from_str(include_str!("fixtures/cli_eval.json")).unwrap();
    let mut rows = Vec::new();
    let mut failures = Vec::new();
    for case in corpus["cases"].as_array().unwrap() {
        let code = case["args"]
            .as_array()
            .unwrap()
            .iter()
            .skip(1)
            .filter_map(Value::as_str)
            .find_map(|arg| {
                if arg == "code" {
                    Some("true")
                } else {
                    arg.strip_prefix("code=")
                }
            });
        let actual = app
            .evaluate(code.map(|code| code.encode_utf16().collect()))
            .map(|result| result.pipe_output());
        let expected = case["expected"]["stdout"].as_str().unwrap();
        let equal = actual.as_ref().is_ok_and(|actual| actual == expected);
        if !equal {
            failures.push(case["id"].clone());
        }
        rows.push(json!({"id":case["id"],"actual":actual.map_err(|error|error.to_string()),"expected":expected,"matches":equal}));
    }
    let service_source = include_str!("fixtures/application_adapter_cases.js");
    let services = app
        .evaluate(Some(service_source.encode_utf16().collect()))
        .map(|result| result.body);
    // This additional integration assertion uses the same real engine's config
    // writer and reads its bytes through the genuine renderer filesystem.
    let written = app
        .engine_mut()
        .write_config_json("script-bridge", &json!({"engine":true}));
    let persisted = app.evaluate(Some(format!("require('fs').readFileSync(require('path').join(app.vault.adapter.getBasePath(),'{}/script-bridge.json'),'utf8')", mnemosyne_core::CONFIG_DIR).encode_utf16().collect())).map(|result|result.body);
    let cleanup = app.stop().unwrap();
    fs::write(report.join("report.json"), serde_json::to_vec_pretty(&json!({
        "scope":"117 native eval outputs plus8 borrowed adapter getter cases andmethodshape through an actualEngine-owned partial application facade; full App/Vault/adapter/SDK/CLI routing remains required",
        "rows":rows,"failures":failures,
        "services":services.as_ref().map_err(|error|error.to_string()),
        "engine_write":written.as_ref().map_err(|error|error.to_string()),
        "persisted":persisted.as_ref().map_err(|error|error.to_string()),
        "cleanup":{"pid":cleanup.pid,"runtime_pid":cleanup.runtime_pid,"exit_code":cleanup.exit_code,"reaped":cleanup.reaped,"process_group_stopped":cleanup.process_group_stopped}
    })).unwrap()).unwrap();
    assert!(
        failures.is_empty(),
        "native eval mismatches: {failures:?}; report={}",
        report.display()
    );
    let actual: Value =
        serde_json::from_str(services.unwrap().strip_prefix("=> ").unwrap()).unwrap();
    let native: Value =
        serde_json::from_str(include_str!("fixtures/plugin_api_application_services.json")).unwrap();
    assert_eq!(actual["cases"], native["cases"]["cases"]);
    assert_eq!(actual["method"], native["cases"]["method"]);
    assert_eq!(
        actual["fields"],
        json!([{"key":"vault","enumerable":true,"configurable":true,"writable":true},{"key":"adapter","enumerable":true,"configurable":true,"writable":true},{"key":"basePath","enumerable":true,"configurable":true,"writable":true}])
    );
    written.unwrap();
    assert_eq!(persisted.unwrap(), "=> {\n  \"engine\": true\n}");
    assert_eq!(cleanup.exit_code, Some(0));
    assert!(cleanup.reaped && cleanup.process_group_stopped);
}
