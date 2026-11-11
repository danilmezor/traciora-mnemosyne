#![cfg(unix)]
//! Real Rust selectors over caller-owned JS getters, through the selected
//! runtime's IPC. This is not installation of the SDK export/class registry.
use mnemosyne_engine::script_methods::{call, FileMethod, Properties};
use mnemosyne_runtime::{NodeChild, NodeChildConfig, NodeIdentity};
use mnemosyne_runtime_protocol::{
    Delivery, HostBinding, HostError, HostKey, HostObjectId, HostOperation, HostResult, HostValue,
    ObjectInspection, ObjectQuery,
};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

struct Reader<'a>(&'a mut NodeChild);
impl Properties for Reader<'_> {
    fn get(&mut self, receiver: &HostValue, name: &[u16]) -> HostResult {
        match self.0.inspect_host(ObjectQuery::GetValue(
            receiver.clone(),
            HostKey::Utf16(name.to_vec()),
        ))? {
            ObjectInspection::Value(value) => Ok(value),
            _ => Err(HostError::unsupported(
                "Property inspection did not return a value",
            )),
        }
    }
}
#[test]
#[ignore = "requires pinned upstream Electron43.7.7 and built Rust addon"]
fn actual_node_inspection_executes_rust_sdk_selectors_against_native_receiver_cases() {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let proof = repo.join("target/rea/runtime-eval/electron-node");
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let report_dir = repo.join(format!("target/rea/sdk-api/rust-methods-{stamp}"));
    fs::create_dir(&report_dir).unwrap();
    fs::create_dir(report_dir.join("vault")).unwrap();
    let addon = report_dir.join("addon.node");
    fs::copy(
        repo.join("target/runtime-build/debug/libmnemosyne_runtime_addon.dylib"),
        &addon,
    )
    .unwrap();
    let mut runtime = NodeChild::launch(NodeChildConfig {
        executable: proof.join("dist/Electron.app/Contents/MacOS/Electron"),
        framework: proof.join(
            "dist/Electron.app/Contents/Frameworks/Electron Framework.framework/Electron Framework",
        ),
        addon,
        profile: report_dir.join("profile"),
        vault: report_dir.join("vault"),
        identity: NodeIdentity {
            executable_sha256: "1121571fb0838d50519c2f002780b2b69f285d37993cf5622ee4b88c6f4b5ce4"
                .into(),
            framework_sha256: "be1b9531ce2218021027aab470e1c188c975d1525b176992c160499cb860838c"
                .into(),
            versions: BTreeMap::from([
                ("node".into(), "24.21.0".into()),
                ("electron".into(), "43.7.7".into()),
                ("v8".into(), "15.0.245.31-electron.0".into()),
                ("icu".into(), "78.2".into()),
            ]),
        },
        context: stamp as u64,
        startup_timeout: Duration::from_secs(15),
    })
    .unwrap();
    for (name, id) in [("ownedShort", 1), ("ownedString", 2), ("ownedRoot", 3)] {
        runtime
            .bind_function(
                name,
                HostBinding {
                    target: HostObjectId(id),
                    operation: HostOperation::Call,
                    member: Some(name.into()),
                    delivery: Delivery::Sync,
                },
            )
            .unwrap();
    }
    let source = include_str!("fixtures/sdk_receiver_cases.js");
    let outcome = runtime.evaluate_with_context(
        Some(source.encode_utf16().collect()),
        |runtime, request| {
            let method = match request.binding.target.0 {
                1 => FileMethod::GetShortName,
                2 => FileMethod::ToString,
                3 => FileMethod::IsRoot,
                _ => panic!("unexpected method target"),
            };
            call(method, &request.receiver, &mut Reader(runtime))
        },
        |_, _| panic!("unexpected console in receiver fixture"),
    );
    let cleanup = runtime.stop().unwrap();
    let body = outcome.unwrap().body;
    let rows: Value = serde_json::from_str(body.strip_prefix("=> ").unwrap()).unwrap();
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/plugin_api_receivers.json")).unwrap();
    let expected: Vec<_> = fixture["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| {
            matches!(
                row["method"].as_str(),
                Some("getShortName" | "toString" | "isRoot")
            )
        })
        .cloned()
        .collect();
    fs::write(report_dir.join("report.json"),serde_json::to_vec_pretty(&json!({"scope":"16 real Node IPC/property-inspection calls to Rust selectors; not installed SDK classes", "rows":rows,"expected":expected,"cleanup":{"pid":cleanup.pid,"exit_code":cleanup.exit_code,"reaped":cleanup.reaped}})).unwrap()).unwrap();
    assert_eq!(rows, json!(expected));
    assert_eq!(cleanup.exit_code, Some(0));
    assert!(cleanup.reaped);
}
