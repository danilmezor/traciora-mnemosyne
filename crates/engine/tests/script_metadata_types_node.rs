#![cfg(unix)]
//! Opt-in real production service replay; no fixture globals/product JS handlers.
use mnemosyne_core::CONFIG_DIR;
#[path = "support/widget_node.rs"]
mod widget_node;
use mnemosyne_engine::{script_application::ScriptApplication, Engine};
use serde_json::{json, Value};
use std::fs;

#[test]
#[ignore = "requires pinned upstream Electron/current protocol5 addon"]
fn production_metadata_service_replays_native_receiver_fields_events_and_normal_io() {
    let (child, _context, report) = widget_node::launch();
    let vault = report.join("vault");
    fs::create_dir(vault.join(CONFIG_DIR)).unwrap();
    let engine = Engine::open(&vault).unwrap();
    let mut app = ScriptApplication::install(engine, child).unwrap();
    if let Err(error) = app.install_metadata_types() {
        let stopped = app.stop().unwrap();
        fs::write(report.join("service-replay.json"),serde_json::to_vec_pretty(&json!({"installation_error":error.to_string(),"cleanup":{"pid":stopped.pid,"reaped":stopped.reaped,"exit_code":stopped.exit_code}})).unwrap()).unwrap();
        panic!("Manager installation failed after owned cleanup: {error}");
    }
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/reference_manager_service.json")).unwrap();
    let mut rows = Vec::new();
    let mut failures = Vec::new();
    for case in fixture["cases"].as_array().unwrap() {
        let code = case["code"].as_str().unwrap().replace(
            "Object.entries(m.registeredTypeWidgets)",
            "Object.entries(m.registeredTypeWidgets||{})",
        );
        let actual = match app.evaluate(Some(code.encode_utf16().collect())) {
            Ok(value) => value,
            Err(error) => {
                failures.push(json!({"id":case["id"],"evaluation_error":error.to_string()}));
                break;
            }
        };
        let actual: Value = match actual
            .body
            .strip_prefix("=> ")
            .and_then(|text| serde_json::from_str(text).ok())
        {
            Some(value) => value,
            None => {
                failures.push(json!({"id":case["id"],"unexpected_output":actual.body}));
                break;
            }
        };
        if case["id"] == "layout" {
            // Partial class registration is explicit: compare all installed
            // members/fields to native descriptors, never invent full layout.
            for level in 0..3 {
                let expected = &case["result"]["chain"][level];
                if actual["chain"][level]["constructor"] != expected["constructor"] {
                    failures.push(json!({"id":"layout","level":level,"constructor_actual":actual["chain"][level]["constructor"],"expected":expected["constructor"]}));
                }
                for descriptor in actual["chain"][level]["descriptors"].as_array().unwrap() {
                    let Some(native) = expected["descriptors"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|v| v["key"] == descriptor["key"])
                    else {
                        failures.push(json!({"id":"layout","extra_descriptor":descriptor}));
                        continue;
                    };
                    if descriptor != native {
                        failures.push(json!({"id":"layout","actual":descriptor,"expected":native}));
                    }
                }
            }
            for key in ["assignedKeys", "propertiesKeys", "loaded", "lastSave"] {
                if actual[key] != case["result"][key] {
                    failures.push(json!({"id":"layout","key":key,"actual":actual[key],"expected":case["result"][key]}));
                }
            }
            let builtin = &actual["events"][0]["records"][0];
            if builtin != &case["result"]["events"][0]["records"][0] {
                failures.push(json!({"id":"layout","builtin":builtin,"expected":case["result"]["events"][0]["records"][0]}));
            }
        } else if actual != case["result"] {
            failures.push(json!({"id":case["id"],"actual":actual,"expected":case["result"]}));
        }
        rows.push(json!({"id":case["id"],"actual":actual}));
    }
    // The native nested setters now pass through the public production service,
    // actual emitter, engine config path and independently deferred real I/O.
    let timing: Value =
        serde_json::from_str(include_str!("fixtures/reference_widget_reentrancy.json")).unwrap();
    for (index, case) in timing["cases"]
        .as_array()
        .unwrap()
        .iter()
        .take(2)
        .enumerate()
    {
        let code=r#"(async()=>{const m=app.metadataTypeManager,fs=require('fs'),raw=()=>fs.readFileSync(require('path').join(app.vault.adapter.basePath,app.vault.configDir,'types.json'),'utf8'),calls=[],events=[],outer='ReentrantOuterINDEX',nested='ReentrantNestedINDEX';let pending,busy=false;const ref=m.on('changed',k=>events.push(k));const widget={toJSON(key){calls.push({stage:'toJSON-enter',key,nestedOwn:Object.hasOwn(m.assignedWidgets,nested.toLowerCase()),nestedProperty:Object.hasOwn(m.properties,nested.toLowerCase())});if(!busy){busy=true;pending=m.setType(nested,INNER);calls.push({stage:'after-nested-call',promise:pending instanceof Promise,nestedOwn:Object.hasOwn(m.assignedWidgets,nested.toLowerCase()),nestedProperty:Object.hasOwn(m.properties,nested.toLowerCase())})}return{marker:1}}};try{const result=await m.setType(outer,widget);await pending;return JSON.stringify({resultType:typeof result,outerShared:m.getAssignedWidget(outer)===widget,nestedType:typeof m.getAssignedWidget(nested),nestedString:String(m.getAssignedWidget(nested)),stored:raw(),events:[...events],calls})}finally{m.offref(ref);await m.unsetType(outer);await m.unsetType(nested)}})()"#.replace("INDEX",&index.to_string()).replace("INNER",case["inner_expression"].as_str().unwrap());
        match app.evaluate(Some(code.encode_utf16().collect())) {
            Ok(value) => match value
                .body
                .strip_prefix("=> ")
                .and_then(|text| serde_json::from_str::<Value>(text).ok())
            {
                Some(actual) => {
                    if actual != case["result"]["immediate"] {
                        failures.push(json!({"id":case["id"],"actual":actual,"expected":case["result"]["immediate"]}));
                    }
                    rows.push(json!({"id":case["id"],"actual":actual}));
                }
                None => failures.push(json!({"id":case["id"],"unexpected_output":value.body})),
            },
            Err(error) => {
                failures.push(json!({"id":case["id"],"evaluation_error":error.to_string()}))
            }
        }
    }
    let stopped = app.stop().unwrap();
    assert!(stopped.reaped && stopped.process_group_stopped);
    fs::write(report.join("service-replay.json"),serde_json::to_vec_pretty(&json!({"cases":rows,"failures":failures,"production_service":true,"installed_via_script_application":true,"normal_owned_config_io":true,"partial_layout":true,"full_lifecycle":false,"canonical_cli_unified":false,"cleanup":{"pid":stopped.pid,"reaped":stopped.reaped,"exit_code":stopped.exit_code}})).unwrap()).unwrap();
    assert!(
        failures.is_empty(),
        "native service mismatches: {}",
        serde_json::to_string_pretty(&failures).unwrap()
    );
}
