#![cfg(unix)]
use mnemosyne_runtime::{NodeChild, NodeChildConfig, NodeIdentity, RendererConfig};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// Explicit owned upstream-runtime replay; ordinary workspace tests never
/// launch an external application or require the downloaded distribution.
#[test]
#[ignore = "requires pinned upstream Electron43.7.7 and built Rust addon"]
fn actual_rust_ipc_replays_native_eval_corpus() {
    replay_native_eval(false);
}

#[test]
#[ignore = "requires pinned upstream Electron43.7.7, official remote2.1.2 and built Rust addon"]
fn actual_renderer_ipc_replays_native_eval_corpus() {
    replay_native_eval(true);
}

fn replay_native_eval(renderer: bool) {
    fn assert_send<T: Send>() {}
    assert_send::<NodeChild>();
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let proof = repo.join("target/rea/runtime-eval/electron-node");
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let report_dir = proof.join(format!(
        "{}-{stamp}",
        if renderer { "renderer-ipc" } else { "rust-ipc" }
    ));
    fs::create_dir(&report_dir).unwrap();
    let vault = report_dir.join("vault");
    fs::create_dir(&vault).unwrap();
    fs::write(vault.join("ParityReady.md"), "Isolated parity oracle.").unwrap();
    let addon = report_dir.join("mnemosyne-runtime-addon.node");
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
        vault,
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
        startup_timeout: Duration::from_secs(20),
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
    eprintln!("runtime ready PID={}", runtime.pid());
    let fixture: Value =
        serde_json::from_str(include_str!("../../engine/tests/fixtures/cli_eval.json")).unwrap();
    let prior: Value =
        serde_json::from_str(include_str!("fixtures/exact_electron_runtime.json")).unwrap();
    let pending: Vec<_> = prior["pending_case_ids"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .filter(|id| {
            if renderer {
                matches!(*id, "global-types" | "owned-file-read")
            } else {
                *id != "commonjs-types"
            }
        })
        .collect();
    let mut rows = Vec::new();
    let mut failures = Vec::new();
    let mut matched = 0;
    for case in fixture["cases"].as_array().unwrap() {
        let id = case["id"].as_str().unwrap();
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
        let output = runtime.evaluate(code).unwrap().pipe_output();
        let expected = case["expected"]["stdout"].as_str().unwrap();
        let equal = output == expected;
        if equal {
            matched += 1;
        } else if !pending.contains(&id) {
            failures.push(format!("{id}: actual={output:?}, expected={expected:?}"));
        }
        rows.push(json!({"id":id,"stdout":output,"matches":equal,"expected":expected,"required_pending":pending.contains(&id)}));
        std::thread::sleep(Duration::from_millis(5));
    }
    let promise_boundary_contract = if renderer {Some(promise_boundary_contract(&mut runtime))}else{None};
    let mutable_eval_contract = if renderer {
        Some(mutable_eval_contract(&mut runtime))
    } else {
        None
    };
    let host_contract = host_callback_contract(&mut runtime);
    let prefix_contract = prefix_and_iteration_contract(&mut runtime);
    let registry_contract = sdk_registry_contract(&mut runtime);
    let reentrancy_contract = reentrancy_contract(&mut runtime);
    let (realm_contract, close_lease) = realm_ownership_contract(&mut runtime);
    let actual_versions = runtime.versions().clone();
    let cleanup = runtime.stop().unwrap();
    assert!(!close_lease.is_alive());
    assert!(runtime.validate_lease(&close_lease).is_err());
    drop(close_lease);
    runtime.flush_root_releases().unwrap();
    fs::write(report_dir.join("report.json"),serde_json::to_vec_pretty(&json!({"scope":"Real Rust parent IPC and Rust addon; bounded SDK registry shapes, no SDK module/renderer facades", "renderer":renderer,"runtime_pid":cleanup.runtime_pid,"process_group_stopped":cleanup.process_group_stopped,"versions":actual_versions,"total":117,"matched":matched,"required_pending":pending,"cases":rows,"promise_boundary_contract":promise_boundary_contract,"mutable_eval_contract":mutable_eval_contract,"prefix_and_iteration_contract":prefix_contract,"authored_host_contract":host_contract,"sdk_registry_contract":registry_contract,"reentrancy_contract":reentrancy_contract,"realm_ownership_contract":realm_contract,"cleanup":{"pid":cleanup.pid,"exit_code":cleanup.exit_code,"reaped":cleanup.reaped}})).unwrap()).unwrap();
    eprintln!(
        "real Rust IPC matched={matched}/117; report={}",
        report_dir.display()
    );
    assert_eq!(cleanup.exit_code, Some(0));
    assert!(cleanup.reaped);
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    assert!(cleanup.process_group_stopped);
    assert_eq!(matched, if renderer { 115 } else { 110 });
}

fn realm_ownership_contract(runtime: &mut NodeChild) -> (Value, mnemosyne_runtime::RootLease) {
    use mnemosyne_runtime::*;
    fn value(runtime: &mut NodeChild, query: ObjectQuery) -> HostValue {
        let ObjectInspection::Value(value) = runtime.inspect_host(query).unwrap() else {
            panic!("Expected VM value")
        };
        value
    }
    fn get(runtime: &mut NodeChild, receiver: HostValue, name: &str) -> HostValue {
        value(runtime, ObjectQuery::GetValue(receiver, name.into()))
    }
    fn invoke(
        runtime: &mut NodeChild,
        function: HostValue,
        receiver: HostValue,
        arguments: Vec<HostValue>,
    ) -> HostValue {
        value(
            runtime,
            ObjectQuery::Invoke {
                function,
                receiver,
                arguments,
            },
        )
    }
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<RootLease>();
    let result = runtime.evaluate_with_context(Some("(()=>{let calls=0;Object.defineProperty(Array.prototype,'0',{set(){calls++},configurable:true});try{ownedSync({owned:true});return calls}finally{delete Array.prototype[0]}})()".encode_utf16().collect()), |runtime, call| {
        let ObjectInspection::Value(value) = runtime.inspect_host(ObjectQuery::GetValue(call.arguments[0].clone(), "owned".into()))? else { unreachable!() };
        assert_eq!(value, HostValue::Boolean(true));
        Ok(HostValue::Undefined)
    }, |_, _| {}).unwrap();
    assert_eq!(result.body, "=> 0");

    let record = value(
        runtime,
        ObjectQuery::Materialize(HostValue::Record(vec![
            ("10".encode_utf16().collect(), HostValue::Number(10.0)),
            ("2".encode_utf16().collect(), HostValue::Number(2.0)),
            ("score".encode_utf16().collect(), HostValue::Number(7.0)),
        ])),
    );
    let HostValue::Reference(reference) = record.clone() else {
        panic!("Record must root a genuine JS object")
    };
    let first = runtime.retain_reference(reference).unwrap();
    let clone = first.clone();
    let second = runtime.retain_reference(reference).unwrap();
    runtime.release_reference(reference.id).unwrap();
    drop(first);
    runtime.flush_root_releases().unwrap();
    assert!(runtime.validate_lease(&clone).is_ok());
    assert_eq!(get(runtime, clone.value(), "score"), HostValue::Number(7.0));
    drop(clone);
    runtime.flush_root_releases().unwrap();
    assert_eq!(
        get(runtime, second.value(), "score"),
        HostValue::Number(7.0)
    );
    assert_eq!(
        runtime
            .inspect_host(ObjectQuery::OwnEnumerableKeys(record.clone()))
            .unwrap(),
        ObjectInspection::Keys(vec!["2".into(), "10".into(), "score".into()])
    );

    let result = runtime
        .evaluate_with_context(
            Some(
                "ownedSync({toString(){console.log('key-coercion');return 'computed'}})"
                    .encode_utf16()
                    .collect(),
            ),
            |runtime, call| {
                let ObjectInspection::Value(value) =
                    runtime.inspect_host(ObjectQuery::SetComputed {
                        receiver: record.clone(),
                        key: call.arguments[0].clone(),
                        value: HostValue::Number(99.0),
                    })?
                else {
                    unreachable!()
                };
                Ok(value)
            },
            |_, _| {},
        )
        .unwrap();
    assert_eq!(result.body, "key-coercion\n=> 99");
    assert_eq!(
        get(runtime, record.clone(), "computed"),
        HostValue::Number(99.0)
    );
    assert_eq!(
        value(
            runtime,
            ObjectQuery::Delete {
                receiver: record.clone(),
                key: "computed".into()
            }
        ),
        HostValue::Boolean(true)
    );

    let prototype = value(
        runtime,
        ObjectQuery::Materialize(HostValue::Record(vec![(
            "marker".encode_utf16().collect(),
            HostValue::Boolean(true),
        )])),
    );
    let object = value(runtime, ObjectQuery::Materialize(HostValue::Record(vec![])));
    let HostValue::Reference(object_reference) = object.clone() else {
        unreachable!()
    };
    value(
        runtime,
        ObjectQuery::SetValue {
            receiver: object.clone(),
            key: "__proto__".into(),
            value: prototype.clone(),
        },
    );
    assert_eq!(
        value(runtime, ObjectQuery::Prototype(object_reference.id)),
        prototype
    );
    value(
        runtime,
        ObjectQuery::DefineData {
            receiver: object.clone(),
            key: "__proto__".into(),
            value: HostValue::Number(3.0),
        },
    );
    assert_eq!(
        get(runtime, object.clone(), "__proto__"),
        HostValue::Number(3.0)
    );
    assert_eq!(
        value(runtime, ObjectQuery::Prototype(object_reference.id)),
        prototype
    );

    let constructor = value(runtime, ObjectQuery::GlobalValue("WeakMap".into()));
    let weak_map = value(
        runtime,
        ObjectQuery::Construct {
            function: constructor,
            arguments: vec![],
        },
    );
    let set = get(runtime, weak_map.clone(), "set");
    assert_eq!(
        invoke(
            runtime,
            set,
            weak_map.clone(),
            vec![object.clone(), record.clone()]
        ),
        weak_map
    );
    let get_method = get(runtime, weak_map.clone(), "get");
    assert_eq!(invoke(runtime, get_method, weak_map, vec![object]), record);

    let regexp = value(runtime, ObjectQuery::GlobalValue("RegExp".into()));
    let regexp = value(
        runtime,
        ObjectQuery::Construct {
            function: regexp,
            arguments: vec!["^[A-Z]+$".into(), "u".into()],
        },
    );
    let test = get(runtime, regexp.clone(), "test");
    assert_eq!(
        invoke(runtime, test, regexp, vec!["ABC".into()]),
        HostValue::Boolean(true)
    );
    let array = value(
        runtime,
        ObjectQuery::Materialize(HostValue::Array(vec![HostValue::Number(1.0)])),
    );
    let array_ctor = value(runtime, ObjectQuery::GlobalValue("Array".into()));
    let is_array = get(runtime, array_ctor.clone(), "isArray");
    assert_eq!(
        invoke(runtime, is_array, array_ctor, vec![array]),
        HostValue::Boolean(true)
    );

    let json_object = value(runtime, ObjectQuery::GlobalValue("JSON".into()));
    let stringify = get(runtime, json_object.clone(), "stringify");
    assert_eq!(
        invoke(
            runtime,
            stringify.clone(),
            json_object.clone(),
            vec![record.clone(), HostValue::Undefined, HostValue::Number(2.0)]
        ),
        HostValue::from("{\n  \"2\": 2,\n  \"10\": 10,\n  \"score\": 7\n}")
    );
    runtime.evaluate(Some("globalThis.__ownedOriginalStringify=JSON.stringify;JSON.stringify=function(value){return 'mutable:'+value.score};undefined")).unwrap();
    let mutable_stringify = get(runtime, json_object.clone(), "stringify");
    assert_eq!(
        invoke(
            runtime,
            mutable_stringify,
            json_object.clone(),
            vec![record.clone()]
        ),
        HostValue::from("mutable:7")
    );
    runtime.evaluate(Some("JSON.stringify=__ownedOriginalStringify;delete globalThis.__ownedOriginalStringify;undefined")).unwrap();
    value(
        runtime,
        ObjectQuery::SetValue {
            receiver: record.clone(),
            key: "self".into(),
            value: record.clone(),
        },
    );
    let error = runtime
        .inspect_host(ObjectQuery::Invoke {
            function: stringify,
            receiver: json_object,
            arguments: vec![record.clone()],
        })
        .unwrap_err();
    let Some(thrown) = &error.thrown else {
        panic!("Must preserve original cyclic-JSON exception")
    };
    let HostValue::Reference(thrown_reference) = **thrown else {
        panic!("Error must remain rooted")
    };
    let rooted_error = runtime.retain_error(error).unwrap();
    runtime.release_reference(thrown_reference.id).unwrap();
    let message = get(runtime, HostValue::Reference(thrown_reference), "message");
    let HostValue::Utf16(message) = message else {
        unreachable!()
    };
    assert!(String::from_utf16_lossy(&message).contains("circular"));
    drop(rooted_error);
    runtime.flush_root_releases().unwrap();
    assert!(
        runtime
            .inspect_host(ObjectQuery::GetValue(
                HostValue::Reference(thrown_reference),
                "message".into()
            ))
            .is_err()
    );
    drop(second);
    // Inspecting WeakMap.get returned this reference, reacquiring its separate
    // borrowed transport ownership. Release that before the final lease drop.
    runtime.release_reference(reference.id).unwrap();
    runtime.flush_root_releases().unwrap();
    assert!(
        runtime
            .inspect_host(ObjectQuery::GetValue(record, "score".into()))
            .is_err()
    );

    let close_value = value(runtime, ObjectQuery::Materialize(HostValue::Record(vec![])));
    let HostValue::Reference(close_reference) = close_value else {
        unreachable!()
    };
    let close_lease = runtime.retain_reference(close_reference).unwrap();
    runtime.release_reference(close_reference.id).unwrap();
    (
        json!({"scope":"Authored genuine realm/root ownership invariants, no SDK manager binding claim","lease_clone_and_independent_owners":true,"borrowed_ownership_separate":true,"last_drop_releases":true,"close_invalidates_leases":true,"computed_key_coercion":true,"delete":true,"intrinsic_define_vs_proto_assignment":true,"weakmap_regexp_construction":true,"genuine_array":true,"own_key_order":true,"current_realm_json_mutation":true,"rooted_stringify_exception":true}),
        close_lease,
    )
}

fn reentrancy_contract(runtime: &mut NodeChild) -> Value {
    use mnemosyne_runtime::*;
    struct Dispatcher {
        calls: Vec<u64>,
    }
    impl NodeHostDispatcher for Dispatcher {
        fn dispatch(&mut self, runtime: &mut NodeChild, request: HostRequest) {
            self.calls.push(request.call.binding.target.0);
            let result = (|| -> HostResult {
                match request.call.binding.target.0 {
                    710 => {
                        let receiver = request.call.arguments[0].clone();
                        let set = runtime.inspect_with_dispatcher(
                            ObjectQuery::SetValue {
                                receiver: receiver.clone(),
                                key: "value".into(),
                                value: HostValue::Number(40.0),
                            },
                            self,
                        )?;
                        assert_eq!(set, ObjectInspection::Value(HostValue::Number(40.0)));
                        let ObjectInspection::Value(function) = runtime.inspect_with_dispatcher(
                            ObjectQuery::GetValue(receiver.clone(), "method".into()),
                            self,
                        )?
                        else {
                            unreachable!()
                        };
                        let ObjectInspection::Value(value) = runtime.inspect_with_dispatcher(
                            ObjectQuery::Invoke {
                                function,
                                receiver,
                                arguments: vec![],
                            },
                            self,
                        )?
                        else {
                            unreachable!()
                        };
                        Ok(value)
                    }
                    711 => {
                        let HostValue::Number(value) = request.call.arguments[0] else {
                            unreachable!()
                        };
                        Ok(HostValue::Number(value + 1.0))
                    }
                    _ => Err(HostError::unsupported(
                        "Owned backend binding not supported",
                    )),
                }
            })();
            request.respond(result);
        }
    }
    for (name, target) in [("ownedOuter", 710), ("ownedNested", 711)] {
        runtime
            .bind_function(
                name,
                HostBinding {
                    target: HostObjectId(target),
                    operation: HostOperation::Call,
                    member: None,
                    delivery: Delivery::Sync,
                },
            )
            .unwrap();
    }
    let mut dispatcher = Dispatcher { calls: Vec::new() };
    let mut logs = Vec::new();
    let output=runtime.evaluate_with_dispatcher(Some("globalThis.ownedReentrant={set value(n){console.log('setter');this.backing=ownedNested(n)},get method(){console.warn('getter');return function(){console.log('method');return ownedNested(this.backing)}}};ownedOuter(ownedReentrant)".encode_utf16().collect()),&mut dispatcher,|level,message|logs.push((level,String::from_utf16_lossy(message)))).unwrap();
    assert_eq!(output.body, "setter\n[warn] getter\nmethod\n=> 42");
    assert_eq!(dispatcher.calls, vec![710, 711, 711]);
    assert_eq!(
        logs,
        vec![
            (ConsoleLevel::Log, "setter".into()),
            (ConsoleLevel::Warn, "getter".into()),
            (ConsoleLevel::Log, "method".into())
        ]
    );
    let result=runtime.evaluate_with_context(Some("ownedSync(Object.defineProperty({},Symbol.toPrimitive,{value(hint){console.log('coerce '+hint);return ownedNested(40)}}))".encode_utf16().collect()),|runtime,call| {
        let ObjectInspection::Value(value)=runtime.inspect_with_dispatcher(ObjectQuery::Binary{operation:BinaryOperation::Add,left:call.arguments[0].clone(),right:"!".into()},&mut dispatcher)?else{unreachable!()};assert_eq!(value,HostValue::from("41!"));
        let ObjectInspection::Value(value)=runtime.inspect_with_dispatcher(ObjectQuery::Binary{operation:BinaryOperation::Subtract,left:call.arguments[0].clone(),right:HostValue::Number(1.0)},&mut dispatcher)?else{unreachable!()};Ok(value)
    },|_,_|{}).unwrap();
    assert_eq!(result.body, "coerce default\ncoerce number\n=> 40");
    let result = runtime.evaluate_with_context(
        Some("ownedSync({[Symbol.toPrimitive](hint){console.log('left '+hint);return '2'}},{[Symbol.toPrimitive](hint){console.log('right '+hint);return '10'}})".encode_utf16().collect()),
        |runtime, call| {
            let mut results = Vec::new();
            for operation in [BinaryOperation::LessThan, BinaryOperation::GreaterThan] {
                let ObjectInspection::Value(value) = runtime.inspect_with_dispatcher(
                    ObjectQuery::Binary { operation, left: call.arguments[0].clone(), right: call.arguments[1].clone() },
                    &mut dispatcher,
                )? else { unreachable!() };
                results.push(value);
            }
            Ok(HostValue::Array(results))
        }, |_, _| {},
    ).unwrap();
    assert_eq!(
        result.body,
        "left number\nright number\nleft number\nright number\n=> [\n  false,\n  true\n]"
    );
    for (operation, left, right, expected) in [
        (
            BinaryOperation::LessThan,
            HostValue::Number(2.0),
            HostValue::Number(10.0),
            true,
        ),
        (
            BinaryOperation::GreaterThan,
            HostValue::Number(f64::NAN),
            HostValue::Number(0.0),
            false,
        ),
        (
            BinaryOperation::LessThan,
            HostValue::Undefined,
            HostValue::Number(0.0),
            false,
        ),
        (
            BinaryOperation::GreaterThan,
            HostValue::BigInt {
                negative: false,
                words: vec![10],
            },
            HostValue::Number(2.0),
            true,
        ),
    ] {
        assert_eq!(
            runtime
                .inspect_host(ObjectQuery::Binary {
                    operation,
                    left,
                    right
                })
                .unwrap(),
            ObjectInspection::Value(HostValue::Boolean(expected))
        );
    }
    let result = runtime
        .inspect_host(ObjectQuery::InstanceOf {
            value: HostValue::HostObject {
                id: HostObjectId(601),
                class: "TFile".encode_utf16().collect(),
            },
            constructor: HostValue::HostClass {
                key: "TFile".encode_utf16().collect(),
            },
        })
        .unwrap();
    assert_eq!(result, ObjectInspection::Value(HostValue::Boolean(true)));
    assert_eq!(runtime.evaluate(Some("globalThis.ownedOriginalPrototype=ownedClasses.TFile.prototype;ownedClasses.TFile.prototype={newPrototype:true};undefined")).unwrap().body,"(no output)");
    runtime
        .register_instance(HostObjectId(610), "TFile", None, vec![])
        .unwrap();
    runtime
        .bind_function(
            "ownedNewInstance",
            HostBinding {
                target: HostObjectId(712),
                operation: HostOperation::Get,
                member: None,
                delivery: Delivery::Sync,
            },
        )
        .unwrap();
    let value=runtime.evaluate_with_context(Some("const newFile=ownedNewInstance();[Object.getPrototypeOf(newFile)===ownedClasses.TFile.prototype,Object.getPrototypeOf(ownedFile)===ownedOriginalPrototype]".encode_utf16().collect()),|_,_|Ok(HostValue::HostObject{id:HostObjectId(610),class:"TFile".encode_utf16().collect()}),|_,_|{}).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(value.body.strip_prefix("=> ").unwrap()).unwrap(),
        json!([true, true])
    );
    assert_eq!(
        runtime
            .evaluate(Some(
                "ownedClasses.TFile.prototype=ownedOriginalPrototype;undefined"
            ))
            .unwrap()
            .body,
        "(no output)"
    );
    json!({"scope":"Authored transport invariants, not SDK constructor-body certification","nested_calls":[710,711,711],"console_order":["setter","getter","method"],"setter_and_getter_invocation":true,"function_receiver_preserved":true,"arithmetic_custom_coercion":true,"relational_custom_coercion_and_order":true,"instanceof":true,"mutable_ctor_prototype":true})
}

fn sdk_registry_contract(runtime: &mut NodeChild) -> Value {
    use mnemosyne_runtime::*;
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/sdk_file_function_shapes.json")).unwrap();
    let shapes = fixture["shapes"].as_array().unwrap();
    let exported: Value = serde_json::from_str(include_str!(
        "../../engine/tests/fixtures/plugin_api_exports.json"
    ))
    .unwrap();
    for key in ["TAbstractFile", "TFile", "TFolder"] {
        let ctor = shapes
            .iter()
            .find(|row| row["owner"] == key && row["member"] == "constructor")
            .unwrap();
        let entry = exported["snapshot"]["exports"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["key"]["name"] == key)
            .unwrap();
        let proto = &exported["snapshot"]["objects"]
            [entry["shape"]["instance_prototype"].as_u64().unwrap() as usize];
        let mut constructor_shape = HostFunctionShape::strict_function();
        constructor_shape
            .prototype
            .as_mut()
            .unwrap()
            .constructor_flags
            .enumerable = key != "TAbstractFile";
        let members = proto["own_properties"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|p| p["key"]["name"] != "constructor")
            .map(|p| {
                let member = p["key"]["name"].as_str().unwrap();
                let captured = shapes
                    .iter()
                    .find(|row| row["owner"] == key && row["member"] == member)
                    .unwrap();
                assert_eq!(captured["constructible"], true);
                HostDescriptor {
                    key: member.into(),
                    enumerable: p["enumerable"].as_bool().unwrap(),
                    configurable: p["configurable"].as_bool().unwrap(),
                    kind: HostDescriptorKind::Method {
                        binding: HostBinding {
                            target: HostObjectId(500),
                            operation: HostOperation::Call,
                            member: Some(format!("{key}::{member}").as_str().into()),
                            delivery: Delivery::Sync,
                        },
                        name: captured["name"].as_str().unwrap().encode_utf16().collect(),
                        length: captured["length"].as_u64().unwrap() as u32,
                        writable: p["writable"].as_bool().unwrap(),
                        shape: HostFunctionShape::strict_function(),
                    },
                }
            })
            .collect();
        runtime
            .register_class(HostClassSpec {
                name: key.encode_utf16().collect(),
                constructor_name: ctor["name"].as_str().unwrap().encode_utf16().collect(),
                constructor_length: ctor["length"].as_u64().unwrap() as u32,
                parent: (key != "TAbstractFile").then(|| "TAbstractFile".encode_utf16().collect()),
                constructor_binding: HostBinding {
                    target: HostObjectId(500),
                    operation: HostOperation::Construct,
                    member: Some(format!("{key}::constructor").as_str().into()),
                    delivery: Delivery::Sync,
                },
                constructor_shape,
                prototype_members: members,
                static_members: Vec::new(),
            })
            .unwrap();
    }
    runtime
        .bind_function(
            "ownedClass",
            HostBinding {
                target: HostObjectId(501),
                operation: HostOperation::Get,
                member: None,
                delivery: Delivery::Sync,
            },
        )
        .unwrap();
    let initialize=runtime.evaluate_with_context(Some("globalThis.ownedClasses=Object.fromEntries(['TAbstractFile','TFile','TFolder'].map(name=>[name,ownedClass(name)]));undefined".encode_utf16().collect()),|_,call|{
        let HostValue::Utf16(key)=call.arguments[0].clone()else{panic!("class key")};Ok(HostValue::HostClass{key})
    },|_,_|{}).unwrap();
    assert_eq!(initialize.body, "(no output)");
    let reflect = r#"JSON.stringify((()=>{const rows=[],key=k=>({string:k});function shape(owner,member,fn){const own=Object.getOwnPropertyDescriptors(fn),p=own.prototype;const describe=v=>v===fn?{type:'function',same_function:true}:v===null?{type:'null'}:typeof v==='string'||typeof v==='number'||typeof v==='boolean'?{type:typeof v,value:v}:typeof v==='undefined'?{type:'undefined'}:{type:typeof v};const descriptors=o=>Reflect.ownKeys(o).map(k=>{const d=Object.getOwnPropertyDescriptor(o,k);return {key:key(k),enumerable:d.enumerable,configurable:d.configurable,...('value'in d?{kind:'data',writable:d.writable,value:describe(d.value)}:{kind:'accessor',get:describe(d.get),set:describe(d.set)})}});const row={owner,member,name:fn.name,length:fn.length,is_proxy:require('util').types.isProxy(fn),own:descriptors(fn)};if(p&&'value'in p&&p.value&&typeof p.value==='object')row.prototype={parent_is_object_prototype:Object.getPrototypeOf(p.value)===Object.prototype,own:descriptors(p.value)};try{Reflect.construct(function(){},[],fn);row.constructible=true}catch(e){row.constructible=false;row.classifier_error={name:e.name,message:e.message}}rows.push(row);}for(const name of ['TAbstractFile','TFile','TFolder']){const fn=ownedClasses[name];shape(name,'constructor',fn);for(const k of Reflect.ownKeys(fn.prototype)){if(k==='constructor')continue;const d=Object.getOwnPropertyDescriptor(fn.prototype,k);if('value'in d&&typeof d.value==='function')shape(name,k,d.value)}}return rows;})())"#;
    let actual: Value = serde_json::from_str(
        runtime
            .evaluate(Some(reflect))
            .unwrap()
            .body
            .strip_prefix("=> ")
            .unwrap(),
    )
    .unwrap();
    assert_eq!(actual, fixture["shapes"], "actual 15 SDK function shapes");
    let properties = vec![HostDescriptor {
        key: "path".into(),
        kind: HostDescriptorKind::Data {
            value: "Folder/A.md".into(),
            writable: true,
        },
        enumerable: true,
        configurable: true,
    }];
    runtime
        .register_instance(HostObjectId(601), "TFile", None, properties)
        .unwrap();
    runtime
        .register_instance(HostObjectId(602), "TFolder", None, vec![])
        .unwrap();
    runtime
        .register_instance(HostObjectId(603), "Array", None, vec![])
        .unwrap();
    runtime
        .register_instance(HostObjectId(604), "Object", None, vec![])
        .unwrap();
    runtime
        .bind_function(
            "ownedInstance",
            HostBinding {
                target: HostObjectId(502),
                operation: HostOperation::Get,
                member: None,
                delivery: Delivery::Sync,
            },
        )
        .unwrap();
    let output=runtime.evaluate_with_context(Some("globalThis.ownedFile=ownedInstance(601);globalThis.ownedFolder=ownedInstance(602);globalThis.ownedArray=ownedInstance(603);globalThis.ownedObject=ownedInstance(604);[ownedFile===ownedInstance(601),Object.getPrototypeOf(ownedFile)===ownedClasses.TFile.prototype,ownedFile.constructor===ownedClasses.TFile,ownedFile instanceof ownedClasses.TFile,ownedFile instanceof ownedClasses.TAbstractFile,Object.getPrototypeOf(ownedFolder)===ownedClasses.TFolder.prototype,ownedFolder.constructor===ownedClasses.TFolder,Array.isArray(ownedArray),Object.getPrototypeOf(ownedObject)===Object.prototype,Object.getPrototypeOf(ownedClasses.TFile)===ownedClasses.TAbstractFile,Object.getPrototypeOf(ownedClasses.TFile.prototype)===ownedClasses.TAbstractFile.prototype]".encode_utf16().collect()),|_,call| {
        let HostValue::Number(id)=call.arguments[0]else{panic!("instance ID")};let class=match id as u64{601=>"TFile",602=>"TFolder",603=>"Array",604=>"Object",_=>unreachable!()};Ok(HostValue::HostObject{id:HostObjectId(id as u64),class:class.encode_utf16().collect()})
    },|_,_|{}).unwrap();
    let identity: Value = serde_json::from_str(output.body.strip_prefix("=> ").unwrap()).unwrap();
    assert_eq!(identity, Value::Array(vec![Value::Bool(true); 11]));
    runtime
        .update_instance(
            HostObjectId(601),
            vec![HostDescriptor {
                key: "path".into(),
                kind: HostDescriptorKind::Data {
                    value: "Renamed.md".into(),
                    writable: true,
                },
                enumerable: true,
                configurable: true,
            }],
        )
        .unwrap();
    assert_eq!(
        runtime
            .evaluate(Some("ownedFile.path='plugin mutation';ownedFile.path"))
            .unwrap()
            .body,
        "=> plugin mutation"
    );
    json!({"scope":"Actual registered constructor/prototype/function shapes and controlled backend wrapper identities; constructor/method bodies, SDK module and lifecycle not certified","function_shapes":actual,"identity_checks":identity,"class_count":3,"function_count":15,"no_proxy":true,"writable_data_field":"plugin mutation"})
}

fn host_callback_contract(runtime: &mut NodeChild) -> Value {
    use mnemosyne_runtime::*;
    for (name, delivery) in [
        ("ownedSync", Delivery::Sync),
        ("ownedPromise", Delivery::Promise),
        ("ownedVoid", Delivery::VoidAsync),
    ] {
        runtime
            .bind_function(
                name,
                HostBinding {
                    target: HostObjectId(91),
                    operation: HostOperation::Call,
                    member: Some(name.into()),
                    delivery,
                },
            )
            .unwrap();
    }
    let caller = std::thread::current().id();
    let mut saved = None;
    let output=runtime.evaluate_with_context(Some("globalThis.ownedGetterHits=0;globalThis.ownedArgument=Object.create({inherited:41});Object.defineProperty(ownedArgument,'accessor',{get(){ownedGetterHits++;return 42},enumerable:true});ownedArgument.self=ownedArgument;ownedArgument[Symbol.for('owned-key')]='symbol value';const ownedReceiver={mutable:'actual receiver'};ownedSync.call(ownedReceiver,ownedArgument,ownedArgument,Symbol.for('owned-key'))===ownedArgument && ownedGetterHits===1".encode_utf16().collect()),|runtime,call| {
        assert_eq!(std::thread::current().id(),caller);
        assert_eq!(call.binding.target,HostObjectId(91));
        let HostValue::Reference(receiver)=call.receiver else{panic!("receiver identity lost")};
        assert_eq!(runtime.inspect(ObjectQuery::Get(receiver.id,"mutable".into())).unwrap(),ObjectInspection::Value("actual receiver".into()));
        assert_eq!(call.arguments[0],call.arguments[1]);
        let HostValue::Reference(object)=call.arguments[0] else{panic!("object flattened")};saved=Some(object.id);
        let ObjectInspection::Keys(keys)=runtime.inspect(ObjectQuery::OwnKeys(object.id)).unwrap()else{panic!("keys")};
        assert_eq!(keys.len(),3);assert!(matches!(&keys[2],HostKey::Symbol(_)));
        let ObjectInspection::Descriptor(Some(descriptor))=runtime.inspect(ObjectQuery::OwnDescriptor(object.id,"accessor".into())).unwrap()else{panic!("descriptor")};
        assert!(matches!(descriptor.kind,JsDescriptorKind::Accessor{get:HostValue::Reference(JsReference{kind:ReferenceKind::Function,..}),set:HostValue::Undefined}));
        assert_eq!(runtime.inspect(ObjectQuery::Get(object.id,"self".into())).unwrap(),ObjectInspection::Value(call.arguments[0].clone()));
        assert_eq!(runtime.inspect(ObjectQuery::Get(object.id,"accessor".into())).unwrap(),ObjectInspection::Value(HostValue::Number(42.0)));
        assert!(matches!(runtime.inspect(ObjectQuery::Prototype(object.id)).unwrap(),ObjectInspection::Value(HostValue::Reference(_))));
        assert_eq!(runtime.inspect(ObjectQuery::OwnDescriptor(object.id,"absent".into())).unwrap(),ObjectInspection::Descriptor(None));
        Ok(call.arguments[0].clone())
    },|_,_|{}).unwrap();
    assert_eq!(output.pipe_output(), "=> true\n");
    let output=runtime.evaluate_with_context(Some("ownedSync(-0,123456789012345678901234567890n,'\\ud800',()=>42,Symbol.for('owned-key'))".encode_utf16().collect()),|_,call| {
        assert!(matches!(call.arguments[0],HostValue::Number(n) if n.to_bits()==(-0.0f64).to_bits()));
        assert!(matches!(call.arguments[1],HostValue::BigInt{negative:false,..}));
        assert_eq!(call.arguments[2],HostValue::Utf16(vec![0xd800]));
        assert!(matches!(call.arguments[3],HostValue::Reference(JsReference{kind:ReferenceKind::Function,..})));
        assert!(matches!(call.arguments[4],HostValue::Reference(JsReference{kind:ReferenceKind::Symbol,..})));
        Ok(call.arguments[1].clone())
    },|_,_|{}).unwrap();
    assert_eq!(output.body, "=> 123456789012345678901234567890");
    let output = runtime
        .evaluate_with_context(
            Some(
                "ownedPromise(41).then(value=>value+1)"
                    .encode_utf16()
                    .collect(),
            ),
            |_, call| {
                assert_eq!(call.binding.delivery, Delivery::Promise);
                Ok(call.arguments[0].clone())
            },
            |_, _| {},
        )
        .unwrap();
    assert_eq!(output.body, "=> 42");
    let output = runtime
        .evaluate_with_requests(
            Some("ownedPromise(40).then(v=>v+2)".encode_utf16().collect()),
            |_, request| {
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(15));
                    request.respond(Ok(HostValue::Number(40.0)));
                });
            },
            |_, _| {},
        )
        .unwrap();
    assert_eq!(output.body, "=> 42");
    let output = runtime
        .evaluate_with_requests(
            Some("ownedPromise()".encode_utf16().collect()),
            |_, request| drop(request),
            |_, _| {},
        )
        .unwrap();
    assert_eq!(output.body, "Error: Rust backend request was dropped");
    for thrown in [
        "({identity:'actual thrown object'})",
        "Symbol.for('actual thrown symbol')",
        "'\\ud800'",
        "null",
        "undefined",
    ] {
        let code = format!(
            "globalThis.ownedThrown={thrown};(()=>{{const object={{get path(){{throw ownedThrown}}}};try{{ownedSync(object);return false}}catch(e){{return e===ownedThrown}}}})()"
        );
        let output = runtime
            .evaluate_with_context(
                Some(code.encode_utf16().collect()),
                |runtime, call| match runtime.inspect_host(ObjectQuery::GetValue(
                    call.arguments[0].clone(),
                    "path".into(),
                )) {
                    Err(error) => {
                        assert!(error.thrown.is_some());
                        Err(error)
                    }
                    Ok(_) => panic!("getter did not throw"),
                },
                |_, _| {},
            )
            .unwrap();
        assert_eq!(output.body, "=> true", "{thrown}");
    }
    let mut queued = 0;
    let output = runtime
        .evaluate_with_context(
            Some("typeof ownedVoid('queued')".encode_utf16().collect()),
            |_, call| {
                assert_eq!(call.binding.delivery, Delivery::VoidAsync);
                queued += 1;
                Ok(HostValue::Number(999.0))
            },
            |_, _| {},
        )
        .unwrap();
    assert_eq!(output.body, "=> undefined");
    assert_eq!(queued, 1);
    for (code, expected) in [
        ("ownedSync()", "Error: actual backend failure"),
        ("ownedPromise()", "Error: actual backend failure"),
    ] {
        let output = runtime
            .evaluate_with_context(
                Some(code.encode_utf16().collect()),
                |_, _| Err(HostError::unsupported("actual backend failure")),
                |_, _| {},
            )
            .unwrap();
        assert_eq!(output.body, expected);
    }
    let mut logs = Vec::new();
    let output = runtime
        .evaluate_with_context(
            Some("console.warn('observer');42".encode_utf16().collect()),
            |_, _| unreachable!(),
            |level, message| logs.push((level, String::from_utf16_lossy(message))),
        )
        .unwrap();
    assert_eq!(output.body, "[warn] observer\n=> 42");
    assert_eq!(logs, vec![(ConsoleLevel::Warn, "observer".into())]);
    let mut logs = Vec::new();
    let output=runtime.evaluate_with_context(Some("const x=ownedSync({get path(){console.warn('getter event');return 42}});console.log('after');x".encode_utf16().collect()),|runtime,call|{
        match runtime.inspect_host(ObjectQuery::GetValue(call.arguments[0].clone(),"path".into()))? {ObjectInspection::Value(value)=>Ok(value),_=>unreachable!()}
    },|level,message|logs.push((level,String::from_utf16_lossy(message)))).unwrap();
    assert_eq!(output.body, "[warn] getter event\nafter\n=> 42");
    assert_eq!(
        logs,
        vec![
            (ConsoleLevel::Warn, "getter event".into()),
            (ConsoleLevel::Log, "after".into())
        ]
    );
    let id = saved.unwrap();
    runtime.release_reference(id).unwrap();
    assert!(runtime.release_reference(id).is_err());
    assert!(runtime.inspect(ObjectQuery::Prototype(id)).is_err());
    let mut receiver_rows = Vec::new();
    for source in [
        "ownedSync.call(undefined,globalThis)",
        "ownedSync.call(null,globalThis)",
        "ownedSync.call(1,globalThis)",
    ] {
        let output=runtime.evaluate_with_context(Some(source.encode_utf16().collect()),|_,call| {
            let kind=match &call.receiver {HostValue::Undefined=>"undefined",HostValue::Null=>"null",HostValue::Number(_)=>"number",HostValue::Reference(_)=>"reference",_=>"other"};
            assert_eq!(kind,if source.contains("undefined") {"undefined"}else if source.contains("null"){"null"}else{"number"});
            assert_eq!(call.new_target,HostValue::Undefined);
            receiver_rows.push(json!({"source":source,"receiver_kind":kind,"receiver_equals_global":call.receiver==call.arguments[0]}));
            Ok(HostValue::Undefined)
        },|_,_|{}).unwrap();
        assert_eq!(output.body, "(no output)");
    }
    json!({"scope":"Authored Rust backend protocol invariants; not native SDK conformance", "passed":["actual non-null receiver identity","caller-thread callback","cycles and repeated references","symbol keys","accessor descriptor without invocation then explicit get","prototype identity","absent descriptor","negative zero","large BigInt","lone surrogate","function references","Sync","Promise","VoidAsync","backend thrown/rejected errors","console observer","released references rejected"],"receiver_edge_observations":receiver_rows})
}

fn prefix_and_iteration_contract(runtime: &mut NodeChild) -> Value {
    use mnemosyne_runtime::*;
    struct Dispatcher {
        pending: Option<HostRequest>,
        trace: Vec<&'static str>,
    }
    impl NodeHostDispatcher for Dispatcher {
        fn dispatch(&mut self, runtime: &mut NodeChild, request: HostRequest) {
            self.trace.push("prefix");
            runtime
                .inspect_with_dispatcher(
                    ObjectQuery::SetValue {
                        receiver: request.call.arguments[0].clone(),
                        key: HostKey::Utf16("prefix".encode_utf16().collect()),
                        value: HostValue::Boolean(true),
                    },
                    self,
                )
                .unwrap();
            self.pending = Some(request);
        }
        fn has_pending_jobs(&self) -> bool {
            self.pending.is_some()
        }
        fn poll(&mut self, runtime: &mut NodeChild) {
            let request = self.pending.take().unwrap();
            // This getter throws if a continuation is executed before the
            // source callback returns and its surrounding synchronous stack ends.
            let ObjectInspection::Value(value) = runtime
                .inspect_with_dispatcher(
                    ObjectQuery::GetValue(
                        request.call.arguments[0].clone(),
                        HostKey::Utf16("finished".encode_utf16().collect()),
                    ),
                    self,
                )
                .unwrap()
            else {
                panic!()
            };
            assert_eq!(value, HostValue::Boolean(true));
            self.trace.push("completion");
            request.respond(Ok(HostValue::from("settled")));
        }
    }
    runtime
        .bind_function(
            "ownedPrefix",
            HostBinding {
                target: HostObjectId(799),
                operation: HostOperation::Call,
                member: Some("prefix".into()),
                delivery: Delivery::Promise,
            },
        )
        .unwrap();
    let mut dispatcher = Dispatcher {
        pending: None,
        trace: Vec::new(),
    };
    let output=runtime.evaluate_with_dispatcher(Some("(async()=>{let synchronous=true;const state={get finished(){if(synchronous)throw Error('premature continuation');return true}};const promise=ownedPrefix(state);if(!state.prefix)throw Error('missing synchronous prefix');synchronous=false;return await promise})()".encode_utf16().collect()),&mut dispatcher,|_,_|{}).unwrap().pipe_output();
    assert_eq!(output, "=> settled\n");
    assert_eq!(dispatcher.trace, vec!["prefix", "completion"]);
    let ObjectInspection::Value(object) = runtime
        .inspect_host(ObjectQuery::GlobalValue("Object".into()))
        .unwrap()
    else {
        panic!()
    };
    // An authored ordinary realm object: inherited key included; deletion
    // between yields suppresses a later own key without snapshot filtering.
    runtime.evaluate(Some("globalThis.ownedIteration=Object.assign(Object.create({inherited:3}),{first:1,later:2})")).unwrap();
    let ObjectInspection::Value(value) = runtime
        .inspect_host(ObjectQuery::GlobalValue("ownedIteration".into()))
        .unwrap()
    else {
        panic!()
    };
    let ObjectInspection::Value(HostValue::Reference(iterator)) = runtime
        .inspect_host(ObjectQuery::BeginForIn(value.clone()))
        .unwrap()
    else {
        panic!()
    };
    let first = runtime
        .inspect_host(ObjectQuery::NextForIn(iterator.id))
        .unwrap();
    assert_eq!(
        first,
        ObjectInspection::ForInKey(Some("first".encode_utf16().collect()))
    );
    runtime
        .inspect_host(ObjectQuery::Delete {
            receiver: value,
            key: HostValue::from("later"),
        })
        .unwrap();
    assert_eq!(
        runtime
            .inspect_host(ObjectQuery::NextForIn(iterator.id))
            .unwrap(),
        ObjectInspection::ForInKey(Some("inherited".encode_utf16().collect()))
    );
    assert_eq!(
        runtime
            .inspect_host(ObjectQuery::NextForIn(iterator.id))
            .unwrap(),
        ObjectInspection::ForInKey(None)
    );
    runtime.release_reference(iterator.id).unwrap();
    if let HostValue::Reference(reference) = object {
        runtime.release_reference(reference.id).unwrap();
    }
    json!({"scope":"Authored protocol invariants; native widget phases replayed separately","synchronous_prefix_before_promise_return":true,"continuation_after_js_stack":true,"continuation_inspects_actual_getter":true,"for_in_inherited_and_deleted_keys":true})
}

fn mutable_eval_contract(runtime: &mut NodeChild) -> Value {
    use mnemosyne_runtime::*;
    let setup = "globalThis.__reaMutableOriginal={eval:Object.getOwnPropertyDescriptor(window,'eval'),String:Object.getOwnPropertyDescriptor(window,'String'),Promise:Object.getOwnPropertyDescriptor(window,'Promise'),then:Object.getOwnPropertyDescriptor(Promise.prototype,'then'),resolve:Object.getOwnPropertyDescriptor(Promise,'resolve'),stringify:Object.getOwnPropertyDescriptor(JSON,'stringify')};undefined";
    runtime.evaluate(Some(setup)).unwrap();
    let ObjectInspection::Value(HostValue::Reference(original)) = runtime
        .inspect_host(ObjectQuery::GlobalValue("eval".into()))
        .unwrap()
    else {
        panic!()
    };
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/cli_eval_mutable_globals.json")).unwrap();
    let mut rows = Vec::new();
    for case in fixture["cases"].as_array().unwrap() {
        let id = case["id"].as_str().unwrap();
        if id == "promise-constructor-proxy" {
            continue;
        }
        for stage in ["setup", "probe"] {
            let code = case[stage]["args"][1]
                .as_str()
                .unwrap()
                .strip_prefix("code=")
                .unwrap();
            let actual = runtime.evaluate(Some(code)).unwrap().pipe_output();
            assert_eq!(
                actual,
                case[stage]["stdout"].as_str().unwrap(),
                "{id}/{stage}"
            );
            rows.push(json!({"id":id,"stage":stage,"stdout":actual}));
        }
        // This owned recovery operation invokes the rooted original eval,
        // independently of the deliberately replaced current eval property.
        runtime.inspect_host(ObjectQuery::Invoke{function:HostValue::Reference(original),receiver:HostValue::Undefined,arguments:vec![HostValue::from("Object.defineProperty(window,'eval',__reaMutableOriginal.eval);Object.defineProperty(window,'String',__reaMutableOriginal.String);Object.defineProperty(window,'Promise',__reaMutableOriginal.Promise);Object.defineProperty(Promise.prototype,'then',__reaMutableOriginal.then);Object.defineProperty(Promise,'resolve',__reaMutableOriginal.resolve);Object.defineProperty(JSON,'stringify',__reaMutableOriginal.stringify);undefined")]}).unwrap();
    }
    runtime.release_reference(original.id).unwrap();
    runtime
        .evaluate(Some("delete globalThis.__reaMutableOriginal;undefined"))
        .unwrap();
    json!({"scope":"11 deterministic native mutable-global paired cases; globalPromise constructor and frameworkPromise.resolve still required","matched_cases":12,"matched_pipes":24,"cases":rows,"pending":["promise-constructor-proxy"]})
}

fn promise_boundary_contract(runtime:&mut NodeChild)->Value {
    use mnemosyne_runtime::*;
    let addon=runtime.profile().parent().unwrap().join("mnemosyne-runtime-addon.node");
    let driver=include_str!("fixtures/promise_boundary_cases.js").replace("ADDON_PATH",&serde_json::to_string(addon.to_str().unwrap()).unwrap());
    runtime.evaluate(Some(&driver)).unwrap();
    let fixture:Value=serde_json::from_str(include_str!("fixtures/cli_promise_boundaries.json")).unwrap();
    let mut rows=Vec::new();
    for row in fixture["cases"].as_array().unwrap(){
        let code=format!("__reaPromiseProbe({})",row["spec"]);
        let actual=runtime.evaluate(Some(&code)).unwrap();
        let(_,payload)=actual.body.split_once("=> ").unwrap();
        let value:Value=serde_json::from_str(payload).unwrap();
        assert_eq!(value,row["expected"],"{}",row["spec"]["id"]);
        rows.push(value);
    }
    // Genuine early CLI rejection. The old handler resumes through SDK Rust
    // callback after a new request has already begun, without leaking its logs.
    runtime.bind_function("ownedRetiredCallback",HostBinding{target:HostObjectId(899),operation:HostOperation::Call,member:None,delivery:Delivery::Sync}).unwrap();
    runtime.evaluate(Some("Promise.resolve=3;undefined")).unwrap();
    let start=std::time::Instant::now();
    let early=runtime.evaluate(Some("globalThis.__reaPromiseTouched=41;new __reaPromiseOriginal(resolve=>setTimeout(()=>{ownedRetiredCallback(41);console.log('retired handler');resolve(42)},200))")).unwrap().pipe_output();
    assert_eq!(early,"TypeError: Promise.resolve is not a function\n");
    assert!(start.elapsed()<Duration::from_millis(150));
    assert!(runtime.pending_handlers()>0);
    let calls=std::cell::RefCell::new(Vec::new());
    let mut dispatch=|_:&mut NodeChild,request:HostRequest|{calls.borrow_mut().push(request.call.arguments.clone());request.respond(Ok(HostValue::Undefined));};
    let outcome=runtime.evaluate_with_dispatcher(Some("Promise.resolve=__reaPromiseOriginalResolve;new __reaPromiseOriginal(resolve=>setTimeout(()=>resolve(84),300))".encode_utf16().collect()),&mut dispatch,|_,_|{}).unwrap().pipe_output();
    assert_eq!(outcome,"=> 84\n");
    assert_eq!(*calls.borrow(),vec![vec![HostValue::Number(41.0)]]);
    let deadline=std::time::Instant::now()+Duration::from_secs(1);
    while runtime.pending_handlers()>0&&std::time::Instant::now()<deadline {runtime.poll_events(&mut dispatch,|_,_,_|{}).unwrap();std::thread::sleep(Duration::from_millis(2));}
    assert_eq!(runtime.pending_handlers(),0);
    let late=runtime.take_console_events();
    // The later request's snapshot is taken before old completion; native
    // overlapping console wrappers may record old logs in both live captures.
    runtime.evaluate(Some("delete globalThis.__reaPromiseProbe;delete globalThis.__reaPromiseOriginal;delete globalThis.__reaPromiseOriginalResolve;undefined")).unwrap();
    json!({"scope":"11 deterministic native source handler/handleCli constructor/adoption fixtures plus authored stateful retired-handler SDK demux","cases":rows,"early_framework_error":early,"retired_callback":calls.into_inner(),"remaining_handlers":0,"late_console_events":late.len()})
}
