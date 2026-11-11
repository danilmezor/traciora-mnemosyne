#![cfg(unix)]
//! Bounded native19 replay through the Rust controller and actual upstream VM.
//! This is not an installed manager SDK binding or manager-event delivery test.
#[path = "support/widget_realm.rs"]
mod widget_realm;
use mnemosyne_engine::{
    json_units::units,
    live_metadata_types::{
        LiveMetadataTypes, WidgetRealm, WidgetRoot, WidgetSaveOutcome, WidgetValue,
    },
    metadata_types::MetadataTypeManager,
};
use mnemosyne_runtime::NodeChild;
#[path = "support/widget_node.rs"]
mod widget_node;
use mnemosyne_runtime_protocol::HostValue;
use serde_json::{json, Value};
use std::fs;
use widget_node::launch;
use widget_realm::{Realm, Root};

fn same(a: &WidgetValue<Root>, b: &WidgetValue<Root>) -> bool {
    match (a, b) {
        (WidgetValue::Number(a), WidgetValue::Number(b)) => {
            a.to_bits() == b.to_bits() || (a.is_nan() && b.is_nan())
        }
        _ => a.host_value() == b.host_value(),
    }
}
fn root(value: &WidgetValue<Root>) -> &Root {
    let WidgetValue::Reference(root) = value else {
        panic!("expected owned reference")
    };
    root
}
fn info_widget(realm: &mut Realm<'_>, info: &WidgetValue<Root>) -> WidgetValue<Root> {
    realm.get(info, &units("widget")).unwrap()
}

struct SwapDispatcher {
    manager: LiveMetadataTypes<Root>,
    context: u64,
}
impl mnemosyne_runtime::NodeHostDispatcher for SwapDispatcher {
    fn dispatch(
        &mut self,
        child: &mut NodeChild,
        request: mnemosyne_runtime_protocol::HostRequest,
    ) {
        // Clone before reborrowing the dispatcher. No manager guard or mutable
        // manager borrow survives any getter, nested callback or VM request.
        let manager = self.manager.clone();
        let context = self.context;
        let mut realm = Realm {
            child,
            context,
            dispatcher: Some(self),
        };
        let replacement = realm.global("replacement").unwrap();
        manager
            .replace_assigned_container(&mut realm, root(&replacement).clone())
            .unwrap();
        request.respond(Ok(HostValue::Undefined));
    }
}

#[test]
#[ignore = "requires pinned upstream Electron43.7.7 and current Rust addon"]
fn rust_controller_replays_native_for_in_deletion_and_reentrant_container_swap() {
    let (mut child, context, report_dir) = launch();
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/reference_widget_reentrancy.json")).unwrap();
    let mut realm = Realm {
        child: &mut child,
        context,
        dispatcher: None,
    };
    let manager = LiveMetadataTypes::new(&mut realm).unwrap();
    realm
        .child
        .bind_function(
            "__ownedSwap",
            mnemosyne_runtime_protocol::HostBinding {
                target: mnemosyne_runtime_protocol::HostObjectId(1001),
                operation: mnemosyne_runtime_protocol::HostOperation::Call,
                member: Some("swap".into()),
                delivery: mnemosyne_runtime_protocol::Delivery::Sync,
            },
        )
        .unwrap();
    let mut records = Vec::new();
    for swap in [false, true] {
        let id = if swap {
            "swap-container-during-name"
        } else {
            "delete-enumerated-key-during-name"
        };
        let setup = format!("globalThis.calls=[];globalThis.replacement={{gate:{{name:'ChangedGate',widget:'gate-new'}},later:{{name:'ChangedLater',widget:'later-new'}},added:{{name:'Added',widget:'added-new'}}}};globalThis.temporary={{gate:{{name:'Gate',widget:'gate-old'}},later:{{name:'Later',widget:'later-old'}}}};Object.defineProperty(temporary,'hasOwnProperty',{{value:function(k){{calls.push('own:'+k);return Object.prototype.hasOwnProperty.call(this,k)}},enumerable:false}});Object.defineProperty(temporary.gate,'name',{{get(){{calls.push('name:gate');{};return'Gate'}}}});undefined", if swap { "__ownedSwap()" } else { "delete temporary.later" });
        realm.child.evaluate(Some(&setup)).unwrap();
        let temporary = realm.global("temporary").unwrap();
        manager
            .replace_assigned_container(&mut realm, root(&temporary).clone())
            .unwrap();
        let mut dispatcher = SwapDispatcher {
            manager: manager.clone(),
            context,
        };
        let mut stored = String::new();
        {
            let mut frame = Realm {
                child: &mut *realm.child,
                context,
                dispatcher: Some(&mut dispatcher),
            };
            manager
                .save(&mut frame, 1.0, |text, _| {
                    stored = String::from_utf16(text).unwrap();
                    Ok(())
                })
                .unwrap();
        }
        let expected = fixture["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|case| case["id"] == id)
            .unwrap();
        let calls = realm.global("calls").unwrap();
        let calls = realm.stringify_value(&calls).unwrap();
        let calls: Value = serde_json::from_str(&String::from_utf16(&calls).unwrap()).unwrap();
        assert_eq!(
            calls, expected["result"]["calls"],
            "{id} exact callback/enumeration order"
        );
        assert_eq!(
            stored,
            expected["result"]["stored"].as_str().unwrap(),
            "{id} exact persisted snapshot"
        );
        let keys = realm
            .own_enumerable_keys(&manager.assigned_container())
            .unwrap();
        let keys: Vec<String> = keys
            .iter()
            .map(|key| String::from_utf16(key).unwrap())
            .collect();
        assert_eq!(
            serde_json::to_value(&keys).unwrap(),
            expected["result"]["assignedKeys"],
            "{id} current container after reentrant swap"
        );
        records.push(json!({"id":id,"calls":calls,"stored":stored,"assigned_keys":keys}));
        realm.child.flush_root_releases().unwrap();
    }
    let receipt = realm.child.stop().unwrap();
    assert!(receipt.reaped && receipt.process_group_stopped);
    fs::write(report_dir.join("reentrant-replay.json"), serde_json::to_vec_pretty(&json!({"cases":records,"shared_handle_reentrancy":true,"fixture_only_language_generator":true,"installed_sdk_manager":false,"native_nested_promises_replayed":false,"cleanup":{"pid":receipt.pid,"reaped":receipt.reaped,"exit_code":receipt.exit_code}})).unwrap()).unwrap();
}

#[test]
#[ignore = "requires pinned upstream Electron43.7.7 and current Rust addon"]
fn rust_controller_replays_native_widget19_and_shared_mutation_identity() {
    let (mut child, context, report_dir) = launch();
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/reference_widget_identity.json")).unwrap();
    let mut realm = Realm {
        child: &mut child,
        context,
        dispatcher: None,
    };
    let manager = LiveMetadataTypes::new(&mut realm).unwrap();
    manager
        .import_canonical_assignments(&mut realm, MetadataTypeManager::load(None).infos_units())
        .unwrap();
    manager.rebuild(&mut realm, std::iter::empty()).unwrap();
    let mut stored: Option<String> = None;
    let mut records = Vec::new();
    let mut clock = 1.0;
    for case in fixture["cases"].as_array().unwrap() {
        let id = case["id"].as_str().unwrap();
        let name = units(case["name"].as_str().unwrap());
        let setup = format!(
            "globalThis.calls=[]; globalThis.widget=({}); undefined",
            case["expression"].as_str().unwrap()
        );
        realm.child.evaluate(Some(&setup)).unwrap();
        let widget = realm.global("widget").unwrap();
        let unknown_a = manager.property_info(&mut realm, &name).unwrap();
        let unknown_b = manager.property_info(&mut realm, &name).unwrap();
        assert_ne!(
            root(&unknown_a).reference(),
            root(&unknown_b).reference(),
            "{id} unknown info fresh"
        );
        let prior = manager.properties_container();
        let (changed, saved) = manager
            .set_type(
                &mut realm,
                &name,
                widget.clone(),
                clock,
                |text, _| {
                    stored = Some(String::from_utf16(text).unwrap());
                    Ok(())
                },
                std::iter::empty(),
            )
            .unwrap();
        let expected_changed: Vec<u16> = case["result"]["events"][0]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_u64().unwrap() as u16)
            .collect();
        assert_eq!(
            changed, expected_changed,
            "{id} source changed key (delivery not tested)"
        );
        let expected_stored = &case["result"]["immediate"]["stored"];
        let expected_stored = expected_stored.as_str().map(str::to_owned);
        assert_eq!(stored, expected_stored, "{id} exact JSON persistence bytes");
        assert_eq!(manager.last_save(), clock);
        assert_ne!(
            manager.properties_container().reference(),
            prior.reference(),
            "{id} rebuild container replacement"
        );
        let assigned = manager.assigned_widget(&mut realm, &name).unwrap();
        assert!(
            same(&assigned, &widget),
            "{id} assigned retains original value/root"
        );
        let info = manager.property_info(&mut realm, &name).unwrap();
        let info_again = manager.property_info(&mut realm, &name).unwrap();
        assert_eq!(
            root(&info).reference(),
            root(&info_again).reference(),
            "{id} known info stable"
        );
        let property = info_widget(&mut realm, &info);
        if matches!(widget, WidgetValue::Undefined | WidgetValue::Null) {
            assert!(
                same(&property, &WidgetValue::Utf16(units("text"))),
                "{id} property nullish fallback"
            );
        } else {
            assert!(
                same(&property, &widget),
                "{id} properties share original widget"
            );
        }
        let calls = realm.global("calls").unwrap();
        let calls_text = realm.stringify_value(&calls).unwrap();
        let actual_calls: Value =
            serde_json::from_str(&String::from_utf16(&calls_text).unwrap()).unwrap();
        assert_eq!(
            actual_calls, case["result"]["calls"],
            "{id} serialization receiver/order"
        );
        let swallowed = matches!(saved, WidgetSaveOutcome::StringifyFailed(_));
        assert_eq!(
            swallowed,
            matches!(
                id,
                "bigint" | "cycle" | "throwing-to-json" | "throwing-getter"
            ),
            "{id} swallowed stringify outcome"
        );
        records.push(json!({"id":id,"persistence":stored,"swallowed_stringify":swallowed,"changed_key":changed}));
        clock += 1.0;
        manager
            .unset_type(
                &mut realm,
                &name,
                clock,
                |text, _| {
                    stored = Some(String::from_utf16(text).unwrap());
                    Ok(())
                },
                std::iter::empty(),
            )
            .unwrap();
        assert_eq!(
            stored.as_deref(),
            case["result"]["cleanup"]["stored"].as_str(),
            "{id} cleanup bytes"
        );
        realm.child.flush_root_releases().unwrap();
        clock += 1.0;
    }

    let native = &fixture["mutation_identity"];
    realm
        .child
        .evaluate(Some("globalThis.widget={marker:1}; undefined"))
        .unwrap();
    let widget = realm.global("widget").unwrap();
    let name = units("WidgetMutate");
    manager
        .set_type(
            &mut realm,
            &name,
            widget.clone(),
            clock,
            |text, _| {
                stored = Some(String::from_utf16(text).unwrap());
                Ok(())
            },
            std::iter::empty(),
        )
        .unwrap();
    assert_eq!(stored.as_deref(), native["storedInitial"].as_str());
    let old_info = manager.property_info(&mut realm, &name).unwrap();
    let old_properties = manager.properties_container();
    let old_assigned = realm
        .get(
            &WidgetValue::Reference(manager.assigned_container()),
            &units("widgetmutate"),
        )
        .unwrap();
    realm
        .set(&widget, &units("marker"), &WidgetValue::Number(2.0))
        .unwrap();
    assert!(same(&info_widget(&mut realm, &old_info), &widget));
    let assigned_widget = manager.assigned_widget(&mut realm, &name).unwrap();
    assert_eq!(
        realm
            .get(&assigned_widget, &units("marker"))
            .unwrap()
            .host_value(),
        HostValue::Number(2.0)
    );
    assert_eq!(stored.as_deref(), native["mutated"]["stored"].as_str());
    manager
        .save(&mut realm, clock + 1.0, |text, _| {
            stored = Some(String::from_utf16(text).unwrap());
            Ok(())
        })
        .unwrap();
    assert_eq!(stored.as_deref(), native["saved"]["stored"].as_str());
    assert_eq!(
        root(&manager.property_info(&mut realm, &name).unwrap()).reference(),
        root(&old_info).reference()
    );
    assert_eq!(
        manager.properties_container().reference(),
        old_properties.reference()
    );
    manager
        .set_type(
            &mut realm,
            &units("WIDGETMUTATE"),
            widget.clone(),
            clock + 2.0,
            |text, _| {
                stored = Some(String::from_utf16(text).unwrap());
                Ok(())
            },
            std::iter::empty(),
        )
        .unwrap();
    let new_assigned = realm
        .get(
            &WidgetValue::Reference(manager.assigned_container()),
            &units("widgetmutate"),
        )
        .unwrap();
    assert_eq!(
        root(&new_assigned).reference(),
        root(&old_assigned).reference()
    );
    assert_eq!(
        realm
            .get(&new_assigned, &units("name"))
            .unwrap()
            .host_value(),
        HostValue::Utf16(name.clone())
    );
    assert_ne!(
        manager.properties_container().reference(),
        old_properties.reference()
    );
    assert_ne!(
        root(&manager.property_info(&mut realm, &name).unwrap()).reference(),
        root(&old_info).reference()
    );
    assert_eq!(stored.as_deref(), native["repeated"]["stored"].as_str());
    manager
        .unset_type(
            &mut realm,
            &name,
            clock + 3.0,
            |text, _| {
                stored = Some(String::from_utf16(text).unwrap());
                Ok(())
            },
            std::iter::empty(),
        )
        .unwrap();
    assert!(matches!(
        manager.assigned_widget(&mut realm, &name).unwrap(),
        WidgetValue::Null
    ));
    assert!(
        same(&info_widget(&mut realm, &old_info), &widget),
        "caller-held old info still shares widget"
    );
    let held_info = realm
        .get(
            &WidgetValue::Reference(old_properties.clone()),
            &units("widgetmutate"),
        )
        .unwrap();
    assert_eq!(
        root(&held_info).reference(),
        root(&old_info).reference(),
        "caller-held old map still owns info"
    );
    assert_eq!(stored.as_deref(), native["unset"]["stored"].as_str());
    let write_failure = manager
        .set_type(
            &mut realm,
            &units("WriteFailure"),
            WidgetValue::Boolean(false),
            clock + 4.0,
            |_, _| {
                Err(mnemosyne_runtime::RootedHostError {
                    error: mnemosyne_runtime_protocol::HostError::new(
                        "Error",
                        "injected owned write failure",
                    ),
                    roots: Vec::new(),
                })
            },
            std::iter::empty(),
        )
        .unwrap();
    assert!(matches!(write_failure.1, WidgetSaveOutcome::WriteFailed(_)));
    assert!(same(
        &manager
            .assigned_widget(&mut realm, &units("WriteFailure"))
            .unwrap(),
        &WidgetValue::Boolean(false)
    ));
    let write_failure_info = manager
        .property_info(&mut realm, &units("WriteFailure"))
        .unwrap();
    assert!(same(
        &info_widget(&mut realm, &write_failure_info),
        &WidgetValue::Boolean(false)
    ));
    // Authored scheduler boundary: snapshot construction precedes the clock,
    // and mutations after construction do not add entries to that snapshot.
    let before_prepare = manager.last_save();
    let prepared = manager.prepare_save(&mut realm).unwrap();
    assert_eq!(manager.last_save(), before_prepare);
    manager
        .assign_type(
            &mut realm,
            &units("LaterAssignment"),
            WidgetValue::Boolean(true),
        )
        .unwrap();
    manager
        .save_prepared(&mut realm, &prepared, clock + 5.0, |text, _| {
            stored = Some(String::from_utf16(text).unwrap());
            Ok(())
        })
        .unwrap();
    assert!(!stored.as_ref().unwrap().contains("LaterAssignment"));
    assert_eq!(manager.last_save(), clock + 5.0);
    let receipt = realm.child.stop().unwrap();
    assert!(receipt.reaped && receipt.process_group_stopped);
    assert!(
        !root(&widget).0.is_alive(),
        "closing realm invalidates caller-held root leases"
    );
    let mut closed_write_called = false;
    assert!(manager
        .save_prepared(&mut realm, &prepared, clock + 6.0, |_, _| {
            closed_write_called = true;
            Ok(())
        })
        .is_err());
    assert!(!closed_write_called);
    assert_eq!(manager.last_save(), clock + 5.0);
    fs::write(report_dir.join("replay.json"), serde_json::to_vec_pretty(&json!({"cases":records,"mutation_identity":true,"injected_write_failure":true,"installed_sdk_binding":false,"event_delivery":false,"prototype_overrides":false,"cleanup":{"pid":receipt.pid,"reaped":receipt.reaped,"exit_code":receipt.exit_code}})).unwrap()).unwrap();
}
