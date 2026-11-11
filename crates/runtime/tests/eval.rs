#![cfg(feature = "embedded-v8")]
use base64::Engine;
use mnemosyne_runtime::{ConsoleLevel, HostCallbacks, Runtime, RuntimeHandle, backend_version};
use mnemosyne_runtime::{
    Delivery, HostBinding, HostError, HostObjectId, HostOperation, HostValue, ReferenceKind,
};
use serde_json::Value;
use std::{cell::RefCell, rc::Rc};

#[derive(Default)]
struct Observer(RefCell<Vec<(ConsoleLevel, String)>>);
impl HostCallbacks for Observer {
    fn console(&self, level: ConsoleLevel, message: &str) {
        self.0.borrow_mut().push((level, message.into()));
    }
}

#[test]
fn native_eval_core_contract_and_explicit_host_requirements() {
    let fixture: Value =
        serde_json::from_str(include_str!("../../engine/tests/fixtures/cli_eval.json")).unwrap();
    let observer = Rc::new(Observer::default());
    let mut runtime = Runtime::with_host(observer.clone()).unwrap();
    let cases = fixture["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 117);
    let mut matched = 0;
    let mut pending = Vec::new();
    let mut failures = Vec::new();
    for case in cases {
        let id = case["id"].as_str().unwrap();
        if case["scope"] == "native-host" || id.starts_with("node-nexttick-") {
            pending.push(id);
            continue;
        }
        let args = case["args"].as_array().unwrap();
        let code = args.iter().skip(1).find_map(|value| {
            let arg = value.as_str().unwrap();
            if arg == "code" {
                Some("true")
            } else {
                arg.strip_prefix("code=")
            }
        });
        let actual = runtime.eval_command(code).unwrap().pipe_output();
        let expected = case["expected"]["stdout"].as_str().unwrap();
        if actual != expected {
            failures.push(format!("{id}: actual{actual:?}, expected{expected:?}"));
        } else {
            matched += 1;
        }
    }
    eprintln!(
        "backend={}, native target={}; matched={matched}, pending Node/Electron/SDK={}",
        backend_version(),
        mnemosyne_runtime::TARGET_V8_VERSION,
        pending.len()
    );
    assert_eq!(
        pending.len(),
        15,
        "Host requirements must remain visible, not silently removed"
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    assert_eq!(matched, 102);
    assert!(
        observer
            .0
            .borrow()
            .iter()
            .any(|(level, line)| *level == ConsoleLevel::Warn && line == "warn")
    );
    // These APIs are deliberately absent until real host bindings exist.
    assert_eq!(
        runtime
            .eval("[typeof app,typeof require,typeof Buffer,typeof process,typeof Deno]")
            .unwrap()
            .pipe_output(),
        "=> [\n  \"undefined\",\n  \"undefined\",\n  \"undefined\",\n  \"undefined\",\n  \"undefined\"\n]\n"
    );
    drop(runtime);
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<RuntimeHandle>();
    let handle = RuntimeHandle::spawn().unwrap();
    let mut worker_matches = 0;
    for case in cases {
        let id = case["id"].as_str().unwrap();
        if case["scope"] == "native-host" || id.starts_with("node-nexttick-") {
            continue;
        }
        let code = case["args"]
            .as_array()
            .unwrap()
            .iter()
            .skip(1)
            .find_map(|value| {
                let arg = value.as_str().unwrap();
                if arg == "code" {
                    Some("true")
                } else {
                    arg.strip_prefix("code=")
                }
            });
        let output = handle.eval_command(code).unwrap().pipe_output();
        assert_eq!(
            output,
            case["expected"]["stdout"].as_str().unwrap(),
            "worker {id}"
        );
        assert_eq!(
            base64::engine::general_purpose::STANDARD.encode(output.as_bytes()),
            case["expected"]["stdout_base64"].as_str().unwrap(),
            "raw bytes {id}"
        );
        worker_matches += 1;
    }
    assert_eq!(worker_matches, 102);
    assert_eq!(
        handle
            .eval("var retainedWorker=40;retainedWorker")
            .unwrap()
            .body,
        "=> 40"
    );
    let moved = handle.clone();
    std::thread::spawn(move || {
        let caller = std::thread::current().id();
        let mut logs = Vec::new();
        let result = moved
            .eval_command_with_console(
                Some("console.warn('worker');retainedWorker+=2"),
                |level, message| {
                    assert_eq!(std::thread::current().id(), caller);
                    logs.push((level, message.to_owned()));
                },
            )
            .unwrap();
        assert_eq!(result.body, "[warn] worker\n=> 42");
        assert_eq!(logs, vec![(ConsoleLevel::Warn, "worker".into())]);
    })
    .join()
    .unwrap();
    assert_eq!(handle.eval("retainedWorker").unwrap().body, "=> 42");
    backend_contract(&handle);
}

fn backend_contract(handle: &RuntimeHandle) {
    for (name, delivery) in [
        ("ownedSync", Delivery::Sync),
        ("ownedPromise", Delivery::Promise),
        ("ownedVoid", Delivery::VoidAsync),
    ] {
        handle
            .bind_function(
                name,
                HostBinding {
                    target: HostObjectId(7),
                    operation: HostOperation::Call,
                    member: Some(name.into()),
                    delivery,
                },
            )
            .unwrap();
    }
    // This is our test function bound to Rust, not an SDK/app placeholder.
    let source = r#"(()=>{let reads=0; const symbol=Symbol('owned'), proto={owned:true}, getter=()=>++reads;
      const value=Object.create(proto,{accessor:{get:getter,enumerable:true}});
      value.self=value; value[symbol]='symbol value'; const fn=()=>1;
      const result=ownedSync(value,value,symbol,symbol,fn,fn,'\ud800',18446744073709551617n,-0);
      return [result===value,result.self===value,Object.getPrototypeOf(result)===proto,
        Reflect.ownKeys(result).includes(symbol),Object.getOwnPropertyDescriptor(result,'accessor').get===getter,reads===0]})()"#;
    let caller = std::thread::current().id();
    let mut calls = 0;
    let mut saved = None;
    let result=handle.eval_command_with_host(Some(source),|_,_|{},|request| {
        assert_eq!(std::thread::current().id(),caller);
        assert_eq!(request.call.binding.target,HostObjectId(7));
        let values=&request.call.arguments;
        assert_eq!(values[0],values[1]);
        assert_eq!(values[2],values[3]);
        assert_eq!(values[4],values[5]);
        assert!(matches!(&values[0],HostValue::Reference(value) if value.kind==ReferenceKind::Object));
        assert!(matches!(&values[2],HostValue::Reference(value) if value.kind==ReferenceKind::Symbol));
        assert!(matches!(&values[4],HostValue::Reference(value) if value.kind==ReferenceKind::Function));
        assert_eq!(values[6],HostValue::Utf16(vec![0xd800]));
        assert_eq!(values[7],HostValue::BigInt{negative:false,words:vec![1,1]});
        assert!(matches!(&values[8],HostValue::Number(value) if value.to_bits()==(-0.0f64).to_bits()));
        calls+=1;
        let result=values[0].clone();
        saved=Some(result.clone());
        request.respond(Ok(result));
    }).unwrap();
    assert_eq!(calls, 1);
    assert_eq!(
        result.body,
        "=> [\n  true,\n  true,\n  true,\n  true,\n  true,\n  true\n]"
    );
    let result = handle
        .eval_command_with_host(
            Some("ownedPromise(42)"),
            |_, _| {},
            |request| {
                assert_eq!(request.call.binding.delivery, Delivery::Promise);
                std::thread::spawn(move || request.respond(Ok(HostValue::Number(42.0))));
            },
        )
        .unwrap();
    assert_eq!(result.body, "=> 42");
    let result = handle
        .eval_command_with_host(
            Some("ownedPromise(1)"),
            |_, _| {},
            |request| {
                request.respond(Err(HostError::new("TypeError", "owned rejection")));
            },
        )
        .unwrap();
    assert_eq!(result.body, "Error: owned rejection");
    let result = handle
        .eval_command_with_host(
            Some("ownedSync()"),
            |_, _| {},
            |request| {
                request.respond(Err(HostError::new("TypeError", "owned throw")));
            },
        )
        .unwrap();
    assert_eq!(result.body, "Error: owned throw");
    let mut queued = 0;
    let result = handle
        .eval_command_with_host(
            Some("typeof ownedVoid(1)"),
            |_, _| {},
            |request| {
                assert_eq!(request.call.binding.delivery, Delivery::VoidAsync);
                assert!(request.completion.is_none());
                queued += 1;
            },
        )
        .unwrap();
    assert_eq!(queued, 1);
    assert_eq!(result.body, "=> undefined");
    let result = handle
        .eval_command_with_host(Some("ownedPromise()"), |_, _| {}, drop)
        .unwrap();
    assert_eq!(result.body, "Error: Rust backend request was dropped");
    let reference = saved.unwrap();
    let HostValue::Reference(identity) = reference.clone() else {
        panic!("expected rooted object")
    };
    let other = RuntimeHandle::spawn().unwrap();
    other
        .bind_function(
            "ownedForeign",
            HostBinding {
                target: HostObjectId(7),
                operation: HostOperation::Call,
                member: None,
                delivery: Delivery::Sync,
            },
        )
        .unwrap();
    let result = other
        .eval_command_with_host(
            Some("ownedForeign()"),
            |_, _| {},
            |request| request.respond(Ok(reference.clone())),
        )
        .unwrap();
    assert_eq!(result.body, "Error: Invalid Rust backend value/reference");
    assert!(!other.release_reference(identity.id).unwrap());
    assert!(handle.release_reference(identity.id).unwrap());
    assert!(!handle.release_reference(identity.id).unwrap());
    let result = handle
        .eval_command_with_host(
            Some("ownedSync()"),
            |_, _| {},
            |request| request.respond(Ok(reference.clone())),
        )
        .unwrap();
    assert_eq!(result.body, "Error: Invalid Rust backend value/reference");
}
