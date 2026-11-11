#![cfg(unix)]
//! Actual prefix/deferred timing + normal filesystem writes. This controlled
//! controller binding is not installation of App's public writable manager.
use mnemosyne_core::CONFIG_DIR;
#[path = "support/widget_node.rs"]
mod widget_node;
#[path = "support/widget_realm.rs"]
mod widget_realm;
use mnemosyne_engine::{
    json_units::units,
    live_metadata_types::{
        LiveMetadataTypes, PendingTypeChange, WidgetFileWriter, WidgetRealm, WidgetSaveOutcome,
        WidgetValue, WidgetWriteTicket,
    },
    metadata_types::MetadataTypeManager,
};
use mnemosyne_runtime::{NodeChild, NodeHostDispatcher, RootedHostError};
use mnemosyne_runtime_protocol::{
    Delivery, HostBinding, HostError, HostObjectId, HostOperation, HostRequest, HostValue,
};
use serde_json::{json, Value};
use std::fs;
use widget_realm::{Realm, Root};

struct DeferredChange {
    pending: PendingTypeChange<Root, RootedHostError>,
    ticket: Option<WidgetWriteTicket>,
    submission_error: Option<RootedHostError>,
    request: HostRequest,
}
struct TimingDispatcher {
    manager: LiveMetadataTypes<Root>,
    context: u64,
    writer: WidgetFileWriter,
    pending: Vec<DeferredChange>,
    completed: Vec<(Vec<u16>, &'static str)>,
}
fn io_error(error: std::io::Error) -> RootedHostError {
    RootedHostError {
        error: HostError::new("Error", error.to_string()),
        roots: Vec::new(),
    }
}
fn units_argument(value: &HostValue) -> Vec<u16> {
    let HostValue::Utf16(value) = value else {
        panic!("bounded fixture uses string names")
    };
    value.clone()
}
impl NodeHostDispatcher for TimingDispatcher {
    fn dispatch(&mut self, child: &mut NodeChild, request: HostRequest) {
        let manager = self.manager.clone();
        let context = self.context;
        let target = request.call.binding.target.0;
        let args = request.call.arguments.clone();
        if matches!(target, 1 | 5) {
            let name = units_argument(&args[0]);
            let prefix = {
                let mut realm = Realm {
                    child,
                    context,
                    dispatcher: Some(self),
                };
                if target == 1 {
                    let widget = realm.retain(args[1].clone()).unwrap();
                    manager.begin_set_type(&mut realm, &name, widget, Realm::date_now)
                } else {
                    manager.begin_unset_type(&mut realm, &name, Realm::date_now)
                }
            }
            .unwrap();
            // Real I/O is submitted before dispatch returns/PrefixReady. The
            // deferred continuation runs only at runtime's real JS task turn.
            let (ticket, submission_error) = match prefix.save().text() {
                Some(text) => match self.writer.submit(text, prefix.save().when()) {
                    Ok(ticket) => (Some(ticket), None),
                    Err(error) => (None, Some(io_error(error))),
                },
                None => (None, None), // captured swallowed BigInt serialize
            };
            self.pending.push(DeferredChange {
                pending: prefix,
                ticket,
                submission_error,
                request,
            });
            return;
        }
        let value = {
            let mut realm = Realm {
                child,
                context,
                dispatcher: Some(self),
            };
            match target {
                2 => manager
                    .assigned_widget(&mut realm, &units_argument(&args[0]))
                    .unwrap()
                    .host_value(),
                3 => WidgetValue::Reference(manager.properties_container()).host_value(),
                4 => WidgetValue::Reference(manager.assigned_container()).host_value(),
                _ => panic!("unexpected fixture binding"),
            }
        };
        request.respond(Ok(value));
    }
    fn has_pending_jobs(&self) -> bool {
        !self.pending.is_empty()
    }
    fn poll(&mut self, child: &mut NodeChild) {
        let mut index = 0;
        while index < self.pending.len() {
            let ready = self.pending[index]
                .ticket
                .as_ref()
                .is_none_or(WidgetWriteTicket::is_ready);
            if !ready {
                index += 1;
                continue;
            }
            let write = if let Some(ticket) = &self.pending[index].ticket {
                match ticket.try_complete() {
                    Ok(Some(result)) => result.map_err(io_error),
                    Ok(None) => {
                        index += 1;
                        continue;
                    }
                    Err(error) => Err(io_error(error)),
                }
            } else {
                match self.pending[index].submission_error.take() {
                    Some(error) => Err(error),
                    None => Ok(()),
                }
            };
            let DeferredChange {
                pending, request, ..
            } = self.pending.remove(index);
            let key = pending.key().to_vec();
            let manager = self.manager.clone();
            let context = self.context;
            let result = {
                let mut realm = Realm {
                    child: &mut *child,
                    context,
                    dispatcher: Some(self),
                };
                manager.finish_type_change(
                    &mut realm,
                    pending,
                    write,
                    std::iter::empty(),
                    |realm, key| {
                        realm.call_global("ownedChanged", vec![HostValue::Utf16(key.to_vec())])?;
                        Ok(())
                    },
                )
            }
            .unwrap();
            let outcome = match result {
                WidgetSaveOutcome::Written => "written",
                WidgetSaveOutcome::StringifyFailed(_) => "stringify-failed",
                WidgetSaveOutcome::WriteFailed(_) => "write-failed",
            };
            self.completed.push((key, outcome));
            // Normal setType/unsetType settles undefined only AFTER changed.
            request.respond(Ok(HostValue::Undefined));
        }
    }
}

#[test]
#[ignore = "requires pinned upstream Electron43.7.7/current protocol5 addon"]
fn actual_nested_setters_run_prefix_before_promise_and_defer_events_after_real_io() {
    let (mut child, context, report_dir) = widget_node::launch();
    let config = report_dir.join("vault").join(CONFIG_DIR);
    fs::create_dir(&config).unwrap();
    let file = config.join("types.json");
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
    let mut dispatcher = TimingDispatcher {
        manager,
        context,
        writer: WidgetFileWriter::new(file.clone()).unwrap(),
        pending: Vec::new(),
        completed: Vec::new(),
    };
    for (name, id, delivery) in [
        ("ownedSetType", 1, Delivery::Promise),
        ("ownedAssigned", 2, Delivery::Sync),
        ("ownedProperties", 3, Delivery::Sync),
        ("ownedAssignments", 4, Delivery::Sync),
        ("ownedUnsetType", 5, Delivery::Promise),
    ] {
        realm
            .child
            .bind_function(
                name,
                HostBinding {
                    target: HostObjectId(id),
                    operation: HostOperation::Call,
                    member: Some(name.into()),
                    delivery,
                },
            )
            .unwrap();
    }
    realm.child.evaluate(Some("globalThis.m={setType:ownedSetType,unsetType:ownedUnsetType,getAssignedWidget:ownedAssigned,getAllProperties:ownedProperties,get properties(){return ownedProperties()},get assignedWidgets(){return ownedAssignments()}};globalThis.events=[];globalThis.ownedChanged=k=>events.push(k);undefined")).unwrap();
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/reference_widget_reentrancy.json")).unwrap();
    let file_json = serde_json::to_string(file.to_str().unwrap()).unwrap();
    let mut records = Vec::new();
    for (index, case) in fixture["cases"]
        .as_array()
        .unwrap()
        .iter()
        .take(2)
        .enumerate()
    {
        let source = r#"(async()=>{const fs=require('fs'),raw=()=>fs.readFileSync(FILE,'utf8'),calls=[],outer='ReentrantOuterINDEX',nested='ReentrantNestedINDEX';globalThis.events=[];let pending,busy=false;const widget={toJSON(key){calls.push({stage:'toJSON-enter',key,nestedOwn:Object.hasOwn(m.assignedWidgets,nested.toLowerCase()),nestedProperty:Object.hasOwn(m.properties,nested.toLowerCase())});if(!busy){busy=true;pending=m.setType(nested,INNER);calls.push({stage:'after-nested-call',promise:pending instanceof Promise,nestedOwn:Object.hasOwn(m.assignedWidgets,nested.toLowerCase()),nestedProperty:Object.hasOwn(m.properties,nested.toLowerCase())})}return{marker:1}}};const result=await m.setType(outer,widget);await pending;return JSON.stringify({resultType:typeof result,outerShared:m.getAssignedWidget(outer)===widget,nestedType:typeof m.getAssignedWidget(nested),nestedString:String(m.getAssignedWidget(nested)),stored:raw(),events:[...events],calls})})()"#
            .replace("FILE", &file_json).replace("INDEX", &index.to_string()).replace("INNER", case["inner_expression"].as_str().unwrap());
        let outcome = realm
            .child
            .evaluate_with_dispatcher(
                Some(source.encode_utf16().collect()),
                &mut dispatcher,
                |_, _| {},
            )
            .unwrap();
        let printed = outcome.body.strip_prefix("=> ").unwrap();
        let actual: Value = serde_json::from_str(printed).unwrap();
        assert_eq!(
            actual, case["result"]["immediate"],
            "{} exact native trace/persistence",
            case["id"]
        );
        assert!(dispatcher.pending.is_empty());
        let expected_keys = vec![
            units(&format!("reentrantnested{index}")),
            units(&format!("reentrantouter{index}")),
        ];
        let last = &dispatcher.completed[dispatcher.completed.len() - 2..];
        assert_eq!(
            last.iter().map(|(key, _)| key.clone()).collect::<Vec<_>>(),
            expected_keys
        );
        assert_eq!(
            last[0].1,
            if index == 0 {
                "written"
            } else {
                "stringify-failed"
            }
        );
        assert_eq!(last[1].1, "written");
        records.push(json!({"id":case["id"],"actual":actual,"outcomes":last.iter().map(|(_, outcome)|*outcome).collect::<Vec<_>>()}));
        // Real unset/persist/event completion, clearing only our two names.
        let cleanup = format!("(async()=>{{await m.unsetType('ReentrantOuter{index}');await m.unsetType('ReentrantNested{index}');return undefined}})()");
        realm
            .child
            .evaluate_with_dispatcher(
                Some(cleanup.encode_utf16().collect()),
                &mut dispatcher,
                |_, _| {},
            )
            .unwrap();
        assert!(dispatcher.pending.is_empty());
    }
    dispatcher.writer.close().unwrap();
    let receipt = realm.child.stop().unwrap();
    assert!(receipt.reaped && receipt.process_group_stopped);
    fs::write(report_dir.join("timing-replay.json"), serde_json::to_vec_pretty(&json!({"cases":records,"normal_filesystem_io":true,"fifo_snapshot_writes":true,"real_js_promises":true,"synchronous_prefix":true,"owning_thread_events_at_js_task_boundary":true,"installed_sdk_manager":false,"fault_injected":false,"cleanup":{"pid":receipt.pid,"exit_code":receipt.exit_code,"reaped":receipt.reaped}})).unwrap()).unwrap();
}
