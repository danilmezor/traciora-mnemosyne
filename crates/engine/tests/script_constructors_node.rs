#![cfg(unix)]
//! Differential replay of real constructor/member wrappers with Rust bodies.
//! No installed desktop app or personal vault is launched by this opt-in test.
use mnemosyne_engine::{
    script_file_classes,
    script_methods::{Arithmetic, Objects, Properties},
};
use mnemosyne_runtime::{
    NodeChild, NodeChildConfig, NodeHostDispatcher, NodeIdentity, RendererConfig,
};
use mnemosyne_runtime_protocol::{
    BinaryOperation, Delivery, HostBinding, HostError, HostKey, HostObjectId, HostOperation,
    HostRequest, HostResult, HostValue, ObjectInspection, ObjectQuery,
};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const FILE_DISPATCH: HostObjectId = HostObjectId(20);
const EXPORT_DISPATCH: HostObjectId = HostObjectId(21);
struct Backend;
struct Vm<'a> {
    runtime: &'a mut NodeChild,
    backend: &'a mut Backend,
}
impl Vm<'_> {
    fn inspect(&mut self, query: ObjectQuery) -> HostResult {
        match self.runtime.inspect_with_dispatcher(query, self.backend)? {
            ObjectInspection::Value(value) => Ok(value),
            _ => Err(HostError::unsupported(
                "VM operation did not return a value",
            )),
        }
    }
}
impl Properties for Vm<'_> {
    fn get(&mut self, receiver: &HostValue, name: &[u16]) -> HostResult {
        self.inspect(ObjectQuery::GetValue(
            receiver.clone(),
            HostKey::Utf16(name.to_vec()),
        ))
    }
}
impl Objects for Vm<'_> {
    fn set(&mut self, receiver: &HostValue, name: &[u16], value: HostValue) -> HostResult {
        self.inspect(ObjectQuery::SetValue {
            receiver: receiver.clone(),
            key: HostKey::Utf16(name.to_vec()),
            value,
        })
    }
    fn invoke(
        &mut self,
        function: HostValue,
        receiver: HostValue,
        arguments: Vec<HostValue>,
    ) -> HostResult {
        self.inspect(ObjectQuery::Invoke {
            function,
            receiver,
            arguments,
        })
    }
    fn arithmetic(
        &mut self,
        operation: Arithmetic,
        left: HostValue,
        right: HostValue,
    ) -> HostResult {
        self.inspect(ObjectQuery::Binary {
            operation: match operation {
                Arithmetic::Add => BinaryOperation::Add,
                Arithmetic::Subtract => BinaryOperation::Subtract,
                Arithmetic::LessThan => BinaryOperation::LessThan,
                Arithmetic::GreaterThan => BinaryOperation::GreaterThan,
            },
            left,
            right,
        })
    }
    fn instance_of(&mut self, value: HostValue, constructor: HostValue) -> HostResult {
        self.inspect(ObjectQuery::InstanceOf { value, constructor })
    }
}
impl NodeHostDispatcher for Backend {
    fn dispatch(&mut self, runtime: &mut NodeChild, request: HostRequest) {
        let result = if request.call.binding.target == EXPORT_DISPATCH {
            Ok(HostValue::Record(
                ["TAbstractFile", "TFile", "TFolder"]
                    .into_iter()
                    .map(|name| {
                        (
                            name.encode_utf16().collect(),
                            HostValue::HostClass {
                                key: name.encode_utf16().collect(),
                            },
                        )
                    })
                    .collect(),
            ))
        } else if request.call.binding.target == FILE_DISPATCH {
            script_file_classes::dispatch(
                &request.call,
                &mut Vm {
                    runtime,
                    backend: self,
                },
            )
        } else {
            Err(HostError::unsupported("Unknown fixture dispatch target"))
        };
        if let Some(completion) = request.completion {
            let _ = completion.send(result);
        }
    }
}

#[test]
#[ignore = "requires pinned upstream Electron43.7.7 and built Rust addon"]
fn rust_sdk_constructor_and_path_bodies_match_actual_native_cases() {
    replay(false);
}

#[test]
#[ignore = "requires pinned upstream Electron43.7.7, remote2.1.2 and built Rust addon"]
fn rust_sdk_bodies_in_real_renderer_match_actual_native_cases() {
    replay(true);
}

fn replay(renderer: bool) {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let proof = repo.join("target/rea/runtime-eval/electron-node");
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let mode = if renderer {
        "renderer-constructors"
    } else {
        "rust-constructors"
    };
    let report_dir = repo.join(format!("target/rea/sdk-api/{mode}-{stamp}"));
    fs::create_dir(&report_dir).unwrap();
    fs::create_dir(report_dir.join("vault")).unwrap();
    let addon = report_dir.join("addon.node");
    fs::copy(
        repo.join("target/runtime-build/debug/libmnemosyne_runtime_addon.dylib"),
        &addon,
    )
    .unwrap();
    let config = NodeChildConfig {
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
    };
    let mut runtime = if renderer {
        let provenance: Value =
            serde_json::from_slice(&fs::read(proof.join("remote-2.1.2/provenance.json")).unwrap())
                .unwrap();
        let remote_files_sha256 = provenance["files"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(name, digest)| (name.clone(), digest.as_str().unwrap().to_owned()))
            .collect();
        NodeChild::launch_renderer(
            config,
            RendererConfig {
                remote_package: proof.join("remote-2.1.2/package"),
                remote_files_sha256,
            },
        )
    } else {
        NodeChild::launch(config)
    }
    .unwrap();
    for class in script_file_classes::specifications(FILE_DISPATCH) {
        runtime.register_class(class).unwrap();
    }
    runtime
        .bind_function(
            "ownedSdk",
            HostBinding {
                target: EXPORT_DISPATCH,
                operation: HostOperation::Get,
                member: None,
                delivery: Delivery::Sync,
            },
        )
        .unwrap();
    let source = format!(
        "globalThis.__ownedSdk=ownedSdk();\n{}",
        include_str!("fixtures/sdk_constructor_cases.js")
    );
    let outcome = runtime.evaluate_with_dispatcher(
        Some(source.encode_utf16().collect()),
        &mut Backend,
        |_, _| panic!("unexpected console in constructor fixture"),
    );
    let folders = runtime.evaluate_with_dispatcher(
        Some(
            include_str!("fixtures/sdk_folder_cases.js")
                .encode_utf16()
                .collect(),
        ),
        &mut Backend,
        |_, _| panic!("unexpected console in folder fixture"),
    );
    let overrides = runtime.evaluate_with_dispatcher(
        Some(
            include_str!("fixtures/sdk_constructor_overrides.js")
                .encode_utf16()
                .collect(),
        ),
        &mut Backend,
        |_, _| panic!("unexpected console in override fixture"),
    );
    let cleanup = runtime.stop().unwrap();
    let outcome = outcome.unwrap();
    let rows: Value = serde_json::from_str(outcome.body.strip_prefix("=> ").unwrap()).unwrap();
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/plugin_api_constructors.json")).unwrap();
    let expected = &fixture["cases"];
    let parse = |outcome: mnemosyne_runtime::EvalOutcome| -> Value {
        serde_json::from_str(outcome.body.strip_prefix("=> ").unwrap()).unwrap()
    };
    let folder_rows = parse(folders.unwrap());
    let override_rows = parse(overrides.unwrap());
    let folder_fixture: Value =
        serde_json::from_str(include_str!("fixtures/plugin_api_folder_methods.json")).unwrap();
    fs::write(report_dir.join("report.json"),serde_json::to_vec_pretty(&json!({"scope":"66 native constructor/setPath cases,37 folder observations and13 base-call/prototype override cases through registered classes and real Rust IPC bodies; supported subset, no Vault/module/fullSDK lifecycle", "renderer":renderer,"rows":rows,"expected":expected,"folders":folder_rows,"expected_folders":folder_fixture["cases"],"overrides":override_rows,"expected_overrides":folder_fixture["overrides"],"cleanup":{"pid":cleanup.pid,"runtime_pid":cleanup.runtime_pid,"process_group_stopped":cleanup.process_group_stopped,"exit_code":cleanup.exit_code,"reaped":cleanup.reaped}})).unwrap()).unwrap();
    assert_eq!(rows, *expected);
    assert_eq!(folder_rows, folder_fixture["cases"]);
    assert_eq!(override_rows, folder_fixture["overrides"]);
    assert_eq!(cleanup.exit_code, Some(0));
    assert!(cleanup.reaped);
    assert!(cleanup.process_group_stopped);
}
