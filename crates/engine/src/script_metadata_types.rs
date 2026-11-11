//! Retained engine metadata manager in the owning script realm.
//!
//! Normal receiver fields, object identities, asynchronous writes and listener
//! callbacks are real. Full constructor/loadData, widget implementations, arbitrary
//! borrowed receivers and overridable save/trigger awaits remain separate work.
use crate::{
    json_units::units,
    live_metadata_types::{
        InferredProperty, LiveMetadataError, LiveMetadataTypes, PendingTypeChange,
        PendingWidgetSave, WidgetFileWriter, WidgetRealm, WidgetValue, WidgetWriteTicket,
    },
    metadata_types::PropertyInfoUnits,
    script_widget_realm::{Realm, Root},
};
use mnemosyne_runtime::{NodeChild, NodeHostDispatcher, RootedHostError, RuntimeError};
use mnemosyne_runtime_protocol::*;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct ScriptMetadataTypes {
    pub target: HostObjectId,
    pub receiver: HostValue,
    pub controller: LiveMetadataTypes<Root>,
    context: u64,
    writer: Arc<Mutex<WidgetFileWriter>>,
    pending: Arc<Mutex<Vec<Pending>>>,
}
struct Pending {
    save: Save,
    ticket: Option<WidgetWriteTicket>,
    error: Option<RootedHostError>,
    request: HostRequest,
}
enum Save {
    Change(PendingTypeChange<Root, RootedHostError>),
    Only(PendingWidgetSave<Root, RootedHostError>),
}
fn error(e: impl std::fmt::Display) -> RootedHostError {
    RootedHostError {
        error: HostError::new("Error", e.to_string()),
        roots: Vec::new(),
    }
}
fn live_error(e: LiveMetadataError<RootedHostError>) -> RootedHostError {
    match e {
        LiveMetadataError::Realm(e) => e,
        e => error(e),
    }
}

fn data(name: &str, value: HostValue) -> HostDescriptor {
    HostDescriptor {
        key: name.into(),
        enumerable: true,
        configurable: true,
        kind: HostDescriptorKind::Data {
            value,
            writable: true,
        },
    }
}
impl ScriptMetadataTypes {
    /// Call after the engine's initial config/cache initialization. This imports
    /// completed canonical assignments, not a claim to hot loadData lifecycle.
    pub fn install(
        child: &mut NodeChild,
        dispatcher: &mut dyn NodeHostDispatcher,
        context: u64,
        target: HostObjectId,
        receiver_id: HostObjectId,
        app: HostValue,
        path: std::path::PathBuf,
        assignments: Vec<PropertyInfoUnits>,
        inferred: Vec<InferredProperty>,
    ) -> Result<Self, RuntimeError> {
        let binding = |member: &str, delivery| HostBinding {
            target,
            operation: HostOperation::Call,
            member: Some(member.into()),
            delivery,
        };
        let members = [
            ("getAllProperties", 0, Delivery::Sync),
            ("getPropertyOccurrences", 0, Delivery::Sync),
            ("getAssignedWidget", 1, Delivery::Sync),
            ("getPropertyInfo", 1, Delivery::Sync),
            ("setType", 2, Delivery::Promise),
            ("unsetType", 1, Delivery::Promise),
            ("save", 0, Delivery::Promise),
            ("rebuildProperties", 0, Delivery::Sync),
            ("on", 3, Delivery::Sync),
            ("trigger", 1, Delivery::Sync),
        ];
        child.register_class(HostClassSpec {
            name: units("MetadataTypeEvents"),
            constructor_name: units("e"),
            constructor_length: 0,
            parent: None,
            constructor_binding: HostBinding {
                target,
                operation: HostOperation::Construct,
                member: Some("events-constructor".into()),
                delivery: Delivery::Sync,
            },
            constructor_shape: HostFunctionShape::strict_function(),
            prototype_members: [("on", 3), ("off", 2), ("offref", 1), ("trigger", 1)]
                .into_iter()
                .map(|(name, length)| HostDescriptor {
                    key: name.into(),
                    enumerable: true,
                    configurable: true,
                    kind: HostDescriptorKind::Method {
                        binding: binding(name, Delivery::Sync),
                        name: Vec::new(),
                        length,
                        writable: true,
                        shape: HostFunctionShape::strict_function(),
                    },
                })
                .collect(),
            static_members: Vec::new(),
        })?;
        child.register_class(HostClassSpec {
            name: units("MetadataTypeManager"),
            constructor_name: units("t"),
            constructor_length: 1,
            parent: Some(units("MetadataTypeEvents")),
            constructor_binding: HostBinding {
                target,
                operation: HostOperation::Construct,
                member: None,
                delivery: Delivery::Sync,
            },
            constructor_shape: HostFunctionShape::strict_function(),
            prototype_members: members
                .into_iter()
                .map(|(name, length, delivery)| HostDescriptor {
                    key: name.into(),
                    enumerable: true,
                    configurable: true,
                    kind: HostDescriptorKind::Method {
                        binding: binding(name, delivery),
                        name: Vec::new(),
                        length,
                        writable: true,
                        shape: HostFunctionShape::strict_function(),
                    },
                })
                .collect(),
            static_members: Vec::new(),
        })?;
        let mut realm = Realm {
            child,
            context,
            dispatcher: Some(dispatcher),
        };
        realm
            .query(ObjectQuery::DefineData {
                receiver: HostValue::HostPrototype {
                    key: units("MetadataTypeManager"),
                },
                key: "constructor".into(),
                value: HostValue::HostClass {
                    key: units("MetadataTypeManager"),
                },
            })
            .map_err(|e| RuntimeError::Initialization(format!("manager install stage 1: {e}")))?;
        let controller = LiveMetadataTypes::new(&mut realm)
            .map_err(|e| RuntimeError::Initialization(format!("manager install stage 2: {e}")))?;
        controller
            .import_canonical_assignments(&mut realm, assignments)
            .map_err(|e| RuntimeError::Initialization(format!("manager install stage 3: {e}")))?;
        controller
            .rebuild(&mut realm, inferred)
            .map_err(|e| RuntimeError::Initialization(format!("manager install stage 4: {e}")))?;
        let events = realm
            .ordinary_object()
            .map_err(|e| RuntimeError::Initialization(format!("manager install stage 5: {e}")))?;
        let assigned = WidgetValue::Reference(controller.assigned_container()).host_value();
        let properties = WidgetValue::Reference(controller.properties_container()).host_value();
        let receiver = HostValue::HostObject {
            id: receiver_id,
            class: units("MetadataTypeManager"),
        };
        realm.child.register_instance(
            receiver_id,
            "MetadataTypeManager",
            None,
            vec![
                data("_", WidgetValue::Reference(events.clone()).host_value()),
                data("assignedWidgets", assigned),
                data("lastSave", HostValue::Number(0.0)),
                data("properties", properties),
                data("_loaded", HostValue::Boolean(true)),
                data("app", app),
            ],
        )?;
        let root_value = realm
            .value(ObjectQuery::Materialize(receiver.clone()))
            .map_err(|e| RuntimeError::Initialization(format!("manager install stage 6: {e}")))?;
        let root = realm
            .root(root_value)
            .map_err(|e| RuntimeError::Initialization(format!("manager install stage 7: {e}")))?;
        controller
            .bind_receiver(&mut realm, root)
            .map_err(|e| RuntimeError::Initialization(format!("manager install stage 8: {e}")))?;
        // The completed service has the same real emitter listener as native
        // registerListeners. It is visible/removable through the writable `_`.
        let receiver_value = realm
            .retain(receiver.clone())
            .map_err(|e| RuntimeError::Initialization(format!("manager install stage 9: {e}")))?;
        let rebuild = realm
            .member(&receiver_value, "rebuildProperties")
            .map_err(|e| RuntimeError::Initialization(format!("manager install stage 10: {e}")))?;
        Self::on_values(
            &mut realm,
            &receiver_value,
            WidgetValue::Utf16(units("changed")),
            rebuild,
            receiver_value.clone(),
        )
        .map_err(|e| RuntimeError::Initialization(format!("manager listener installation: {e}")))?;
        Ok(Self {
            target,
            receiver,
            controller,
            context,
            writer: Arc::new(Mutex::new(WidgetFileWriter::new(path).map_err(|e| {
                RuntimeError::Initialization(format!("manager install stage 12: {e}"))
            })?)),
            pending: Arc::new(Mutex::new(Vec::new())),
        })
    }
    pub fn has_pending_jobs(&self) -> bool {
        !self.pending.lock().unwrap().is_empty()
    }
    /// Caller clones the service before lending its outer dispatcher to this
    /// function, so nested VM calls never borrow a mutable service recursively.
    pub fn dispatch(
        &self,
        child: &mut NodeChild,
        dispatcher: &mut dyn NodeHostDispatcher,
        request: HostRequest,
        inferred: Vec<InferredProperty>,
        occurrences: Vec<(Vec<u16>, usize)>,
    ) {
        let raw_args = request.call.arguments.clone();
        let member = request.call.binding.member.clone();
        let mut realm = Realm {
            child,
            context: self.context,
            dispatcher: Some(dispatcher),
        };
        let mut owned = match realm
            .callback_values(std::iter::once(request.call.receiver.clone()).chain(raw_args))
        {
            Ok(values) => values,
            Err(e) => {
                request.respond(Err(e.error));
                return;
            }
        };
        let receiver = owned.remove(0);
        let args = owned;
        let result = (|| -> Result<Option<Save>, RootedHostError> {
            if request.call.binding.operation == HostOperation::Construct {
                return Err(error(
                    "MetadataTypeManager constructor lifecycle is pending",
                ));
            }
            // Source arguments call their own toLowerCase. Execute that lookup/
            // call before the controller; it preserves ordinary thrown values.
            let name = |realm: &mut Realm<'_>| -> Result<Vec<u16>, RootedHostError> {
                let name = args.first().cloned().unwrap_or(WidgetValue::Undefined);
                let method = realm.member(&name, "toLowerCase")?;
                if !matches!(&method,WidgetValue::Reference(root) if root.0.reference().kind==ReferenceKind::Function)
                {
                    return Err(RootedHostError {
                        error: HostError::new("TypeError", "e.toLowerCase is not a function"),
                        roots: Vec::new(),
                    });
                }
                let lowered = realm.invoke(&method, &name, Vec::new())?;
                match lowered {
                    WidgetValue::Utf16(v) => Ok(v),
                    _ => Err(error(
                        "Nonstring lowercase result requires computed manager key boundary",
                    )),
                }
            };
            match &member {
                Some(HostKey::Utf16(m)) if *m == units("setType") => {
                    let lowered = name(&mut realm)?;
                    let original = match args.first() {
                        Some(WidgetValue::Utf16(v)) => v.clone(),
                        _ => lowered,
                    };
                    let widget = args.get(1).cloned().unwrap_or(WidgetValue::Undefined);
                    let pending = self
                        .controller
                        .begin_set_type(&mut realm, &original, widget, Realm::date_now)
                        .map_err(live_error)?;
                    Ok(Some(Save::Change(pending)))
                }
                Some(HostKey::Utf16(m)) if *m == units("unsetType") => {
                    let key = name(&mut realm)?;
                    Ok(Some(Save::Change(
                        self.controller
                            .begin_unset_type(&mut realm, &key, Realm::date_now)
                            .map_err(live_error)?,
                    )))
                }
                Some(HostKey::Utf16(m)) if *m == units("save") => {
                    let snapshot = self
                        .controller
                        .prepare_save(&mut realm)
                        .map_err(live_error)?;
                    let now = realm.date_now()?;
                    Ok(Some(Save::Only(
                        self.controller
                            .begin_save_prepared(&mut realm, snapshot, now)
                            .map_err(live_error)?,
                    )))
                }
                _ => Ok(None),
            }
        })();
        match result {
            Err(e) => {
                request.respond(Err(e.error));
                return;
            }
            Ok(Some(save)) => {
                let pending = match &save {
                    Save::Change(v) => v.save(),
                    Save::Only(v) => v,
                };
                let (ticket, e) = match pending.text() {
                    Some(text) => match self.writer.lock().unwrap().submit(text, pending.when()) {
                        Ok(t) => (Some(t), None),
                        Err(e) => (None, Some(error(e))),
                    },
                    None => (None, None),
                };
                self.pending.lock().unwrap().push(Pending {
                    save,
                    ticket,
                    error: e,
                    request,
                });
                return;
            }
            Ok(None) => {}
        }
        let result = (|| -> Result<HostValue, RootedHostError> {
            let m = match &member {
                Some(HostKey::Utf16(m)) => m.as_slice(),
                _ => return Err(error("Unknown manager operation")),
            };
            if m == units("getAllProperties") {
                return Ok(realm.member(&receiver, "properties")?.host_value());
            }
            if m == units("getPropertyOccurrences") {
                let object = realm.ordinary_object()?;
                for (k, v) in occurrences {
                    realm.define_data(&object, &k, &WidgetValue::Number(v as f64))?;
                }
                return Ok(WidgetValue::Reference(object).host_value());
            }
            if m == units("rebuildProperties") {
                self.controller
                    .rebuild(&mut realm, inferred)
                    .map_err(live_error)?;
                return Ok(HostValue::Undefined);
            }
            if m == units("getAssignedWidget") || m == units("getPropertyInfo") {
                let raw = args.first().cloned().unwrap_or(WidgetValue::Undefined);
                let method = realm.member(&raw, "toLowerCase")?;
                if !matches!(&method,WidgetValue::Reference(root) if root.0.reference().kind==ReferenceKind::Function)
                {
                    return Err(RootedHostError {
                        error: HostError::new("TypeError", "e.toLowerCase is not a function"),
                        roots: Vec::new(),
                    });
                }
                let lowered = realm.invoke(&method, &raw, Vec::new())?;
                let WidgetValue::Utf16(name) = lowered else {
                    return Err(error("Nonstring lowercase result is pending"));
                };
                let container = realm.member(
                    &receiver,
                    if m == units("getAssignedWidget") {
                        "assignedWidgets"
                    } else {
                        "properties"
                    },
                )?;
                let object = realm.global("Object")?;
                let has_own = realm.member(&object, "hasOwn")?;
                let own = realm.invoke(
                    &has_own,
                    &object,
                    vec![container.host_value(), HostValue::Utf16(name.clone())],
                )?;
                let value = if matches!(own, WidgetValue::Boolean(true)) {
                    let info = realm.get(&container, &name)?;
                    if m == units("getAssignedWidget") {
                        realm.member(&info, "widget")?
                    } else {
                        info
                    }
                } else if m == units("getAssignedWidget") {
                    WidgetValue::Null
                } else {
                    let info = realm.ordinary_object()?;
                    realm.define_data(&info, &units("name"), &raw)?;
                    realm.define_data(
                        &info,
                        &units("widget"),
                        &WidgetValue::Utf16(units("text")),
                    )?;
                    WidgetValue::Reference(info)
                };
                return Ok(value.host_value());
            }
            if m == units("on") {
                return Self::on(&mut realm, &receiver, &args);
            }
            if m == units("off") || m == units("offref") {
                Self::off(&mut realm, &receiver, &args, m == units("offref"))?;
                return Ok(HostValue::Undefined);
            }
            if m == units("trigger") {
                let event = args
                    .first()
                    .cloned()
                    .unwrap_or(WidgetValue::Undefined)
                    .host_value();
                Self::trigger(
                    &mut realm,
                    &receiver,
                    event,
                    args.iter().skip(1).map(WidgetValue::host_value).collect(),
                )?;
                return Ok(HostValue::Undefined);
            }
            Err(error("Manager method is not implemented"))
        })();
        request.respond(result.map_err(|e| e.error));
    }
    fn on(
        realm: &mut Realm<'_>,
        receiver: &WidgetValue<Root>,
        args: &[WidgetValue<Root>],
    ) -> Result<HostValue, RootedHostError> {
        let event = args.first().cloned().unwrap_or(WidgetValue::Undefined);
        let function = args.get(1).cloned().unwrap_or(WidgetValue::Undefined);
        let context = args.get(2).cloned().unwrap_or(WidgetValue::Undefined);
        Self::on_values(realm, receiver, event, function, context)
    }
    fn on_values(
        realm: &mut Realm<'_>,
        receiver: &WidgetValue<Root>,
        event: WidgetValue<Root>,
        function: WidgetValue<Root>,
        context: WidgetValue<Root>,
    ) -> Result<HostValue, RootedHostError> {
        let events = realm.member(receiver, "_")?;
        let current = realm.value(ObjectQuery::GetComputed {
            receiver: events.host_value(),
            key: event.host_value(),
        })?;
        let list = if matches!(
            current,
            HostValue::Undefined | HostValue::Null | HostValue::Boolean(false)
        ) {
            let value = realm.value(ObjectQuery::Materialize(HostValue::Array(Vec::new())))?;
            let list = realm.retain(value)?;
            realm.query(ObjectQuery::SetComputed {
                receiver: events.host_value(),
                key: event.host_value(),
                value: list.host_value(),
            })?;
            list
        } else {
            realm.retain(current)?
        };
        let info = realm.ordinary_object()?;
        for (k, v) in [
            ("e", receiver.clone()),
            ("name", event),
            ("fn", function),
            ("ctx", context),
        ] {
            realm.define_data(&info, &units(k), &v)?;
        }
        let push = realm.member(&list, "push")?;
        realm.invoke(
            &push,
            &list,
            vec![WidgetValue::Reference(info.clone()).host_value()],
        )?;
        Ok(WidgetValue::Reference(info).host_value())
    }
    fn off(
        realm: &mut Realm<'_>,
        receiver: &WidgetValue<Root>,
        args: &[WidgetValue<Root>],
        by_ref: bool,
    ) -> Result<(), RootedHostError> {
        let events = realm.member(receiver, "_")?;
        let supplied = args.first().cloned().unwrap_or(WidgetValue::Undefined);
        if by_ref
            && matches!(
                supplied,
                WidgetValue::Undefined | WidgetValue::Null | WidgetValue::Boolean(false)
            )
        {
            return Ok(());
        }
        let event = if by_ref {
            realm.member(&supplied, "name")?.host_value()
        } else {
            supplied.host_value()
        };
        let list = realm.value(ObjectQuery::GetComputed {
            receiver: events.host_value(),
            key: event.clone(),
        })?;
        let retained = realm.value(ObjectQuery::Materialize(HostValue::Array(Vec::new())))?;
        let retained = realm.retain(retained)?;
        let filter = if by_ref {
            supplied
        } else {
            args.get(1).cloned().unwrap_or(WidgetValue::Undefined)
        };
        if !matches!(list, HostValue::Undefined | HostValue::Null)
            && !matches!(
                filter,
                WidgetValue::Undefined | WidgetValue::Null | WidgetValue::Boolean(false)
            )
        {
            let list = realm.retain(list)?;
            let mut i = 0;
            loop {
                let WidgetValue::Number(length) = realm.member(&list, "length")? else {
                    return Err(error("Listener length is not a number"));
                };
                if i as f64 >= length {
                    break;
                }
                let entry = realm.get(&list, &units(&i.to_string()))?;
                let candidate = if by_ref {
                    entry.clone()
                } else {
                    realm.member(&entry, "fn")?
                };
                if candidate.host_value() != filter.host_value() {
                    let push = realm.member(&retained, "push")?;
                    realm.invoke(&push, &retained, vec![entry.host_value()])?;
                }
                i += 1;
            }
        }
        let length = realm.member(&retained, "length")?;
        if matches!(length, WidgetValue::Number(v) if v > 0.0) {
            realm.query(ObjectQuery::SetComputed {
                receiver: events.host_value(),
                key: event,
                value: retained.host_value(),
            })?;
        } else {
            realm.query(ObjectQuery::Delete {
                receiver: events.host_value(),
                key: event,
            })?;
        }
        Ok(())
    }
    fn trigger(
        realm: &mut Realm<'_>,
        receiver: &WidgetValue<Root>,
        event: HostValue,
        args: Vec<HostValue>,
    ) -> Result<(), RootedHostError> {
        let events = realm.member(receiver, "_")?;
        let list = realm.value(ObjectQuery::GetComputed {
            receiver: events.host_value(),
            key: event,
        })?;
        if matches!(list, HostValue::Undefined | HostValue::Null) {
            return Ok(());
        }
        let list = realm.retain(list)?;
        let slice = realm.member(&list, "slice")?;
        let copied = realm.invoke(&slice, &list, Vec::new())?;
        let length = realm.member(&copied, "length")?;
        let WidgetValue::Number(length) = length else {
            return Err(error("Nonarray listener length"));
        };
        for i in 0..length as usize {
            let entry = realm.get(&copied, &units(&i.to_string()))?;
            let function = realm.member(&entry, "fn")?;
            let context = realm.member(&entry, "ctx")?;
            realm.invoke(&function, &context, args.clone())?;
        }
        Ok(())
    }
    pub fn poll(
        &self,
        child: &mut NodeChild,
        dispatcher: &mut dyn NodeHostDispatcher,
        _inferred: Vec<InferredProperty>,
    ) {
        loop {
            let item = {
                let mut pending = self.pending.lock().unwrap();
                let index = pending
                    .iter()
                    .position(|p| p.ticket.as_ref().is_none_or(WidgetWriteTicket::is_ready));
                index.map(|i| pending.remove(i))
            };
            let Some(mut item) = item else { break };
            let write = match item.ticket.as_ref() {
                Some(ticket) => match ticket.try_complete() {
                    Ok(Some(result)) => result.map_err(error),
                    Ok(None) => {
                        self.pending.lock().unwrap().push(item);
                        break;
                    }
                    Err(e) => Err(error(e)),
                },
                None => item.error.take().map_or(Ok(()), Err),
            };
            let mut realm = Realm {
                child: &mut *child,
                context: self.context,
                dispatcher: Some(&mut *dispatcher),
            };
            let result = match item.save {
                Save::Only(p) => {
                    p.finish(write);
                    Ok(HostValue::Undefined)
                }
                Save::Change(p) => {
                    let (key, _) = p.finish_save(write);
                    let receiver = realm.retain(self.receiver.clone());
                    receiver
                        .and_then(|receiver| {
                            Self::trigger(
                                &mut realm,
                                &receiver,
                                HostValue::from("changed"),
                                vec![HostValue::Utf16(key)],
                            )
                        })
                        .map(|_| HostValue::Undefined)
                }
            };
            item.request.respond(result.map_err(|e| e.error));
        }
    }
}
