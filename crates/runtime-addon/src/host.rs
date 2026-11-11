//! Opaque JS identities and actual Rust callbacks. No arbitrary object is
//! flattened; own-descriptor inspection does not invoke accessor functions.
use crate::{evaluate as js, ipc};
use mnemosyne_runtime_protocol::*;
use napi::sys;
use std::{cell::RefCell, collections::HashMap, ptr, rc::Rc};
pub(crate) struct Root {
    env: sys::napi_env,
    reference: sys::napi_ref,
}
impl Root {
    pub(crate) fn new(env: sys::napi_env, value: js::Value) -> js::JsResult<Self> {
        // A holder also roots Symbols without relying on primitive reference
        // support or coercing them into string keys.
        let mut holder = ptr::null_mut();
        js::check(unsafe { sys::napi_create_array_with_length(env, 1, &mut holder) })?;
        // Rooting is private machinery: inherited setters must not observe it.
        let descriptor = sys::napi_property_descriptor {
            utf8name: c"0".as_ptr(),
            name: ptr::null_mut(),
            method: None,
            getter: None,
            setter: None,
            value,
            attributes: 0,
            data: ptr::null_mut(),
        };
        js::check(unsafe { sys::napi_define_properties(env, holder, 1, &descriptor) })?;
        let mut reference = ptr::null_mut();
        js::check(unsafe { sys::napi_create_reference(env, holder, 1, &mut reference) })?;
        Ok(Self { env, reference })
    }
    pub(crate) fn value(&self) -> js::JsResult<js::Value> {
        let mut holder = ptr::null_mut();
        let mut value = ptr::null_mut();
        js::check(unsafe { sys::napi_get_reference_value(self.env, self.reference, &mut holder) })?;
        js::check(unsafe { sys::napi_get_element(self.env, holder, 0, &mut value) })?;
        Ok(value)
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        unsafe { sys::napi_delete_reference(self.env, self.reference) };
    }
}
struct State {
    next: u64,
    roots: HashMap<u64, RootEntry>,
    bindings: HashMap<u64, HostBinding>,
    promises: HashMap<u64, sys::napi_deferred>,
    own_keys: Root,
    own_descriptor: Root,
    get_prototype: Root,
    enumerable_keys: Root,
    factory: Option<Root>,
    property_read: Option<Root>,
    property_write: Option<Root>,
    binary: Option<Root>,
    instance_of: Option<Root>,
    property_delete: Option<Root>,
    for_in: Option<Root>,
}
struct RootEntry {
    root: Root,
    borrowed: bool,
    leases: u64,
}
thread_local! {static STATES:RefCell<HashMap<usize,Rc<RefCell<State>>>>=RefCell::new(HashMap::new());}
fn state(env: sys::napi_env) -> js::JsResult<Rc<RefCell<State>>> {
    if let Some(state) = STATES.with(|s| s.borrow().get(&(env as usize)).cloned()) {
        return Ok(state);
    }
    let global = js::global(env)?;
    let reflect = js::get(env, global, "Reflect")?;
    let object = js::get(env, global, "Object")?;
    let state = Rc::new(RefCell::new(State {
        next: 1,
        roots: HashMap::new(),
        bindings: HashMap::new(),
        promises: HashMap::new(),
        own_keys: Root::new(env, js::get(env, reflect, "ownKeys")?)?,
        own_descriptor: Root::new(env, js::get(env, object, "getOwnPropertyDescriptor")?)?,
        get_prototype: Root::new(env, js::get(env, object, "getPrototypeOf")?)?,
        enumerable_keys: Root::new(env, js::get(env, object, "keys")?)?,
        factory: None,
        property_read: None,
        property_write: None,
        binary: None,
        instance_of: None,
        property_delete: None,
        for_in: None,
    }));
    STATES.with(|s| s.borrow_mut().insert(env as usize, state.clone()));
    js::check(unsafe { sys::napi_add_env_cleanup_hook(env, Some(cleanup), env.cast()) })?;
    Ok(state)
}
pub(crate) fn install_factory(
    env: sys::napi_env,
    factory: js::Value,
    property_read: js::Value,
    property_write: js::Value,
    binary: js::Value,
    instance_of: js::Value,
    property_delete: Option<js::Value>,
    for_in: Option<js::Value>,
) -> js::JsResult<()> {
    if js::kind(env, factory)? != sys::ValueType::napi_function {
        return Err(());
    }
    state(env)?.borrow_mut().factory = Some(Root::new(env, factory)?);
    state(env)?.borrow_mut().property_read = Some(Root::new(env, property_read)?);
    state(env)?.borrow_mut().property_write = Some(Root::new(env, property_write)?);
    state(env)?.borrow_mut().binary = Some(Root::new(env, binary)?);
    state(env)?.borrow_mut().instance_of = Some(Root::new(env, instance_of)?);
    state(env)?.borrow_mut().property_delete = property_delete
        .map(|value| Root::new(env, value))
        .transpose()?;
    state(env)?.borrow_mut().for_in = for_in.map(|value| Root::new(env, value)).transpose()?;
    Ok(())
}
unsafe extern "C" fn cleanup(data: *mut std::ffi::c_void) {
    STATES.with(|s| s.borrow_mut().remove(&(data as usize)));
}
fn reference(
    env: sys::napi_env,
    value: js::Value,
    kind: ReferenceKind,
) -> js::JsResult<JsReference> {
    let state = state(env)?;
    for (index, root) in &mut state.borrow_mut().roots {
        let mut equal = false;
        js::check(unsafe { sys::napi_strict_equals(env, value, root.root.value()?, &mut equal) })?;
        if equal {
            root.borrowed = true;
            return Ok(JsReference {
                id: JsValueId {
                    context: ipc::identity(env)?,
                    index: *index,
                },
                kind,
            });
        }
    }
    let mut state = state.borrow_mut();
    let index = state.next;
    state.next += 1;
    state.roots.insert(
        index,
        RootEntry {
            root: Root::new(env, value)?,
            borrowed: true,
            leases: 0,
        },
    );
    Ok(JsReference {
        id: JsValueId {
            context: ipc::identity(env)?,
            index,
        },
        kind,
    })
}
pub(crate) fn lookup(env: sys::napi_env, id: JsValueId) -> js::JsResult<js::Value> {
    if id.context != ipc::identity(env)? {
        return Err(());
    }
    state(env)?
        .borrow()
        .roots
        .get(&id.index)
        .ok_or(())?
        .root
        .value()
}
pub(crate) fn release(env: sys::napi_env, id: JsValueId) -> js::JsResult<()> {
    if id.context != ipc::identity(env)? {
        return Err(());
    }
    let state = state(env)?;
    let mut state = state.borrow_mut();
    let entry = state.roots.get_mut(&id.index).ok_or(())?;
    if !entry.borrowed {
        return Err(());
    }
    entry.borrowed = false;
    if entry.leases == 0 {
        state.roots.remove(&id.index);
    }
    Ok(())
}
pub(crate) fn retain(env: sys::napi_env, id: JsValueId) -> js::JsResult<()> {
    if id.context != ipc::identity(env)? {
        return Err(());
    }
    let state = state(env)?;
    let mut state = state.borrow_mut();
    let entry = state.roots.get_mut(&id.index).ok_or(())?;
    entry.leases = entry.leases.checked_add(1).ok_or(())?;
    Ok(())
}
pub(crate) fn release_lease(env: sys::napi_env, id: JsValueId) -> js::JsResult<()> {
    if id.context != ipc::identity(env)? {
        return Err(());
    }
    let state = state(env)?;
    let mut state = state.borrow_mut();
    let entry = state.roots.get_mut(&id.index).ok_or(())?;
    entry.leases = entry.leases.checked_sub(1).ok_or(())?;
    if entry.leases == 0 && !entry.borrowed {
        state.roots.remove(&id.index);
    }
    Ok(())
}
pub(crate) fn from_js(env: sys::napi_env, value: js::Value) -> js::JsResult<HostValue> {
    Ok(match js::kind(env, value)? {
        sys::ValueType::napi_undefined => HostValue::Undefined,
        sys::ValueType::napi_null => HostValue::Null,
        sys::ValueType::napi_boolean => {
            let mut b = false;
            js::check(unsafe { sys::napi_get_value_bool(env, value, &mut b) })?;
            HostValue::Boolean(b)
        }
        sys::ValueType::napi_number => {
            let mut n = 0.0;
            js::check(unsafe { sys::napi_get_value_double(env, value, &mut n) })?;
            HostValue::Number(n)
        }
        sys::ValueType::napi_string => HostValue::Utf16(js::read_units(env, value)?),
        sys::ValueType::napi_bigint => {
            let mut count = 0;
            js::check(unsafe {
                sys::napi_get_value_bigint_words(
                    env,
                    value,
                    ptr::null_mut(),
                    &mut count,
                    ptr::null_mut(),
                )
            })?;
            let mut sign = 0;
            let mut words = vec![0; count];
            js::check(unsafe {
                sys::napi_get_value_bigint_words(
                    env,
                    value,
                    &mut sign,
                    &mut count,
                    words.as_mut_ptr(),
                )
            })?;
            words.truncate(count);
            HostValue::BigInt {
                negative: sign != 0,
                words,
            }
        }
        sys::ValueType::napi_symbol => {
            HostValue::Reference(reference(env, value, ReferenceKind::Symbol)?)
        }
        sys::ValueType::napi_function => {
            HostValue::Reference(reference(env, value, ReferenceKind::Function)?)
        }
        sys::ValueType::napi_object => {
            HostValue::Reference(reference(env, value, ReferenceKind::Object)?)
        }
        _ => return Err(()),
    })
}
pub(crate) fn to_js(env: sys::napi_env, value: HostValue) -> js::JsResult<js::Value> {
    let mut result = ptr::null_mut();
    match value {
        HostValue::Undefined => return Ok(js::undefined(env)),
        HostValue::Null => js::check(unsafe { sys::napi_get_null(env, &mut result) })?,
        HostValue::Boolean(b) => js::check(unsafe { sys::napi_get_boolean(env, b, &mut result) })?,
        HostValue::Number(n) => js::check(unsafe { sys::napi_create_double(env, n, &mut result) })?,
        HostValue::Utf16(s) => return js::text(env, &s),
        HostValue::BigInt { negative, words } => js::check(unsafe {
            sys::napi_create_bigint_words(
                env,
                i32::from(negative),
                words.len(),
                words.as_ptr(),
                &mut result,
            )
        })?,
        HostValue::Reference(reference) => return lookup(env, reference.id),
        HostValue::HostObject { id, class } => return crate::registry::object(env, id, class),
        HostValue::HostClass { key } => return crate::registry::class(env, &key),
        HostValue::HostPrototype { key } => return crate::registry::prototype(env, &key),
        HostValue::Array(values) => {
            js::check(unsafe {
                sys::napi_create_array_with_length(env, values.len(), &mut result)
            })?;
            for (index, value) in values.into_iter().enumerate() {
                let value = to_js(env, value)?;
                js::check(unsafe { sys::napi_set_element(env, result, index as u32, value) })?;
            }
        }
        HostValue::Record(values) => {
            js::check(unsafe { sys::napi_create_object(env, &mut result) })?;
            for (key, value) in values {
                let key = js::text(env, &key)?;
                let value = to_js(env, value)?;
                // Define own data, preserving even __proto__ as a data property.
                let descriptor = sys::napi_property_descriptor {
                    utf8name: ptr::null(),
                    name: key,
                    method: None,
                    getter: None,
                    setter: None,
                    value,
                    attributes: sys::PropertyAttributes::writable
                        | sys::PropertyAttributes::enumerable
                        | sys::PropertyAttributes::configurable,
                    data: ptr::null_mut(),
                };
                js::check(unsafe { sys::napi_define_properties(env, result, 1, &descriptor) })?;
            }
        }
    }
    Ok(result)
}
fn key(env: sys::napi_env, key: HostKey) -> js::JsResult<js::Value> {
    match key {
        HostKey::Utf16(s) => js::text(env, &s),
        HostKey::Symbol(id) => lookup(env, id),
    }
}
fn boolean(env: sys::napi_env, value: js::Value) -> js::JsResult<bool> {
    let mut b = false;
    js::check(unsafe { sys::napi_get_value_bool(env, value, &mut b) })?;
    Ok(b)
}
pub(crate) fn inspect(env: sys::napi_env, query: ObjectQuery) -> js::JsResult<ObjectInspection> {
    let state = state(env)?;
    Ok(match query {
        ObjectQuery::BeginForIn(value) => {
            let helpers = state.borrow().for_in.as_ref().ok_or(())?.value()?;
            let mut function = ptr::null_mut();
            js::check(unsafe { sys::napi_get_element(env, helpers, 0, &mut function) })?;
            let value = to_js(env, value)?;
            ObjectInspection::Value(from_js(
                env,
                js::call(env, function, js::undefined(env), &[value])?,
            )?)
        }
        ObjectQuery::NextForIn(id) => {
            let helpers = state.borrow().for_in.as_ref().ok_or(())?.value()?;
            let mut function = ptr::null_mut();
            js::check(unsafe { sys::napi_get_element(env, helpers, 1, &mut function) })?;
            let iterator = lookup(env, id)?;
            let result = js::call(env, function, js::undefined(env), &[iterator])?;
            let mut done = false;
            js::check(unsafe {
                sys::napi_get_value_bool(env, js::get(env, result, "done")?, &mut done)
            })?;
            ObjectInspection::ForInKey(if done {
                None
            } else {
                Some(js::read_units(env, js::get(env, result, "value")?)?)
            })
        }
        ObjectQuery::OwnKeys(id) => {
            let function = state.borrow().own_keys.value()?;
            let object = lookup(env, id)?;
            let keys = js::call(env, function, js::undefined(env), &[object])?;
            let mut length = 0;
            js::check(unsafe { sys::napi_get_array_length(env, keys, &mut length) })?;
            let mut result = Vec::new();
            for index in 0..length {
                let mut k = ptr::null_mut();
                js::check(unsafe { sys::napi_get_element(env, keys, index, &mut k) })?;
                result.push(if js::kind(env, k)? == sys::ValueType::napi_symbol {
                    HostKey::Symbol(reference(env, k, ReferenceKind::Symbol)?.id)
                } else {
                    HostKey::Utf16(js::read_units(env, k)?)
                });
            }
            ObjectInspection::Keys(result)
        }
        ObjectQuery::OwnDescriptor(id, k) => {
            let function = state.borrow().own_descriptor.value()?;
            let object = lookup(env, id)?;
            let k = key(env, k)?;
            let descriptor = js::call(env, function, js::undefined(env), &[object, k])?;
            if js::kind(env, descriptor)? == sys::ValueType::napi_undefined {
                ObjectInspection::Descriptor(None)
            } else {
                let mut data = false;
                js::check(unsafe {
                    sys::napi_has_own_property(
                        env,
                        descriptor,
                        js::string(env, "value")?,
                        &mut data,
                    )
                })?;
                let kind = if data {
                    JsDescriptorKind::Data {
                        value: from_js(env, js::get(env, descriptor, "value")?)?,
                        writable: boolean(env, js::get(env, descriptor, "writable")?)?,
                    }
                } else {
                    JsDescriptorKind::Accessor {
                        get: from_js(env, js::get(env, descriptor, "get")?)?,
                        set: from_js(env, js::get(env, descriptor, "set")?)?,
                    }
                };
                ObjectInspection::Descriptor(Some(JsOwnDescriptor {
                    enumerable: boolean(env, js::get(env, descriptor, "enumerable")?)?,
                    configurable: boolean(env, js::get(env, descriptor, "configurable")?)?,
                    kind,
                }))
            }
        }
        ObjectQuery::Prototype(id) => {
            let function = state.borrow().get_prototype.value()?;
            ObjectInspection::Value(from_js(
                env,
                js::call(env, function, js::undefined(env), &[lookup(env, id)?])?,
            )?)
        }
        ObjectQuery::Get(id, k) => {
            let mut value = ptr::null_mut();
            js::check(unsafe {
                sys::napi_get_property(env, lookup(env, id)?, key(env, k)?, &mut value)
            })?;
            ObjectInspection::Value(from_js(env, value)?)
        }
        ObjectQuery::GetValue(value, k) => {
            let function = state.borrow().property_read.as_ref().ok_or(())?.value()?;
            ObjectInspection::Value(from_js(
                env,
                js::call(
                    env,
                    function,
                    js::undefined(env),
                    &[to_js(env, value)?, key(env, k)?],
                )?,
            )?)
        }
        ObjectQuery::GlobalValue(k) => {
            let mut value = ptr::null_mut();
            js::check(unsafe {
                sys::napi_get_property(env, js::global(env)?, key(env, k)?, &mut value)
            })?;
            ObjectInspection::Value(from_js(env, value)?)
        }
        ObjectQuery::DefineData {
            receiver,
            key,
            value,
        } => {
            crate::registry::define(
                env,
                to_js(env, receiver)?,
                vec![HostDescriptor {
                    key,
                    kind: HostDescriptorKind::Data {
                        value,
                        writable: true,
                    },
                    enumerable: true,
                    configurable: true,
                }],
            )?;
            ObjectInspection::Value(HostValue::Undefined)
        }
        ObjectQuery::Materialize(value) => {
            ObjectInspection::Value(from_js(env, to_js(env, value)?)?)
        }
        ObjectQuery::Construct {
            function,
            arguments,
        } => {
            let arguments = arguments
                .into_iter()
                .map(|value| to_js(env, value))
                .collect::<Result<Vec<_>, _>>()?;
            let mut value = ptr::null_mut();
            js::check(unsafe {
                sys::napi_new_instance(
                    env,
                    to_js(env, function)?,
                    arguments.len(),
                    arguments.as_ptr(),
                    &mut value,
                )
            })?;
            ObjectInspection::Value(from_js(env, value)?)
        }
        ObjectQuery::GetComputed { receiver, key: k } => {
            let function = state.borrow().property_read.as_ref().ok_or(())?.value()?;
            ObjectInspection::Value(from_js(
                env,
                js::call(
                    env,
                    function,
                    js::undefined(env),
                    &[to_js(env, receiver)?, to_js(env, k)?],
                )?,
            )?)
        }
        ObjectQuery::SetComputed {
            receiver,
            key: k,
            value,
        } => {
            let function = state.borrow().property_write.as_ref().ok_or(())?.value()?;
            ObjectInspection::Value(from_js(
                env,
                js::call(
                    env,
                    function,
                    js::undefined(env),
                    &[to_js(env, receiver)?, to_js(env, k)?, to_js(env, value)?],
                )?,
            )?)
        }
        ObjectQuery::Delete { receiver, key: k } => {
            let function = state.borrow().property_delete.as_ref().ok_or(())?.value()?;
            ObjectInspection::Value(from_js(
                env,
                js::call(
                    env,
                    function,
                    js::undefined(env),
                    &[to_js(env, receiver)?, to_js(env, k)?],
                )?,
            )?)
        }
        ObjectQuery::OwnEnumerableKeys(value) => {
            let function = state.borrow().enumerable_keys.value()?;
            let keys = js::call(env, function, js::undefined(env), &[to_js(env, value)?])?;
            let mut length = 0;
            js::check(unsafe { sys::napi_get_array_length(env, keys, &mut length) })?;
            let mut result = Vec::with_capacity(length as usize);
            for index in 0..length {
                let mut value = ptr::null_mut();
                js::check(unsafe { sys::napi_get_element(env, keys, index, &mut value) })?;
                result.push(HostKey::Utf16(js::read_units(env, value)?));
            }
            ObjectInspection::Keys(result)
        }
        ObjectQuery::BackendIdentity(id) => {
            ObjectInspection::BackendIdentity(crate::registry::identify(env, lookup(env, id)?)?)
        }
        ObjectQuery::SetValue {
            receiver,
            key: k,
            value,
        } => {
            let function = state.borrow().property_write.as_ref().ok_or(())?.value()?;
            ObjectInspection::Value(from_js(
                env,
                js::call(
                    env,
                    function,
                    js::undefined(env),
                    &[to_js(env, receiver)?, key(env, k)?, to_js(env, value)?],
                )?,
            )?)
        }
        ObjectQuery::Invoke {
            function,
            receiver,
            arguments,
        } => {
            let function = to_js(env, function)?;
            let receiver = to_js(env, receiver)?;
            let arguments = arguments
                .into_iter()
                .map(|value| to_js(env, value))
                .collect::<Result<Vec<_>, _>>()?;
            ObjectInspection::Value(from_js(
                env,
                js::call(env, function, receiver, &arguments)?,
            )?)
        }
        ObjectQuery::Binary {
            operation,
            left,
            right,
        } => {
            let function = state.borrow().binary.as_ref().ok_or(())?.value()?;
            let op = js::string(
                env,
                match operation {
                    BinaryOperation::Add => "add",
                    BinaryOperation::Subtract => "subtract",
                    BinaryOperation::LessThan => "less-than",
                    BinaryOperation::GreaterThan => "greater-than",
                },
            )?;
            ObjectInspection::Value(from_js(
                env,
                js::call(
                    env,
                    function,
                    js::undefined(env),
                    &[op, to_js(env, left)?, to_js(env, right)?],
                )?,
            )?)
        }
        ObjectQuery::InstanceOf { value, constructor } => {
            let function = state.borrow().instance_of.as_ref().ok_or(())?.value()?;
            ObjectInspection::Value(from_js(
                env,
                js::call(
                    env,
                    function,
                    js::undefined(env),
                    &[to_js(env, value)?, to_js(env, constructor)?],
                )?,
            )?)
        }
    })
}
pub(crate) fn error_value(env: sys::napi_env, error: HostError) -> js::JsResult<js::Value> {
    if let Some(value) = error.thrown {
        return to_js(env, *value);
    }
    let message = js::string(env, &error.message)?;
    let mut value = ptr::null_mut();
    js::check(unsafe { sys::napi_create_error(env, ptr::null_mut(), message, &mut value) })?;
    js::set(env, value, "name", js::string(env, &error.name)?)?;
    Ok(value)
}
pub(crate) fn inspection_error(env: sys::napi_env) -> HostError {
    let mut pending = false;
    if js::check(unsafe { sys::napi_is_exception_pending(env, &mut pending) }).is_ok() && pending {
        if let Ok(value) = js::exception(env).and_then(|value| from_js(env, value)) {
            return HostError::thrown(value);
        }
    }
    HostError::unsupported("JavaScript inspection failed")
}
pub(crate) fn settle(env: sys::napi_env, id: u64, result: HostResult) -> js::JsResult<()> {
    let deferred = state(env)?.borrow_mut().promises.remove(&id).ok_or(())?;
    match result {
        Ok(value) => match to_js(env, value) {
            Ok(value) => js::check(unsafe { sys::napi_resolve_deferred(env, deferred, value) }),
            Err(()) => {
                let value = error_value(
                    env,
                    HostError::unsupported("Cannot resolve Rust host value"),
                )?;
                js::check(unsafe { sys::napi_reject_deferred(env, deferred, value) })
            }
        },
        Err(error) => {
            let value = error_value(env, error)?;
            js::check(unsafe { sys::napi_reject_deferred(env, deferred, value) })
        }
    }
}
unsafe extern "C" fn callback(env: sys::napi_env, info: sys::napi_callback_info) -> js::Value {
    js::guarded(env, || {
        let (args, index) = js::callback_args(env, info)?;
        let state = state(env)?;
        let binding = state
            .borrow()
            .bindings
            .get(&(index as u64))
            .cloned()
            .ok_or(())?;
        if args.len() != 3 {
            return Err(());
        }
        let mut length = 0;
        js::check(unsafe { sys::napi_get_array_length(env, args[2], &mut length) })?;
        let mut arguments = Vec::with_capacity(length as usize);
        for index in 0..length {
            let mut value = ptr::null_mut();
            js::check(unsafe { sys::napi_get_element(env, args[2], index, &mut value) })?;
            arguments.push(from_js(env, value)?);
        }
        let call = HostCall {
            binding: binding.clone(),
            receiver: from_js(env, args[0])?,
            new_target: from_js(env, args[1])?,
            arguments,
        };
        let id = ipc::next_call(env)?;
        match binding.delivery {
            Delivery::VoidAsync => {
                ipc::prefix_call(env, id, call)?;
                Ok(js::undefined(env))
            }
            Delivery::Promise => {
                let mut deferred = ptr::null_mut();
                let mut promise = ptr::null_mut();
                js::check(unsafe { sys::napi_create_promise(env, &mut deferred, &mut promise) })?;
                state.borrow_mut().promises.insert(id, deferred);
                ipc::prefix_call(env, id, call)?;
                Ok(promise)
            }
            Delivery::Sync => match ipc::sync_call(env, id, call)? {
                Ok(value) => match to_js(env, value) {
                    Ok(value) => Ok(value),
                    Err(()) => {
                        let error = error_value(
                            env,
                            HostError::unsupported("Cannot resolve Rust host value"),
                        )?;
                        js::check(unsafe { sys::napi_throw(env, error) })?;
                        Err(())
                    }
                },
                Err(error) => {
                    let error = error_value(env, error)?;
                    js::check(unsafe { sys::napi_throw(env, error) })?;
                    Err(())
                }
            },
        }
    })
}
pub(crate) fn bind(env: sys::napi_env, name: Vec<u16>, binding: HostBinding) -> js::JsResult<()> {
    let function = create_function(env, binding, &name, 0, None)?;
    let name = js::text(env, &name)?;
    js::check(unsafe { sys::napi_set_property(env, js::global(env)?, name, function) })
}
pub(crate) fn create_function(
    env: sys::napi_env,
    binding: HostBinding,
    name: &[u16],
    length: u32,
    shape: Option<&HostFunctionShape>,
) -> js::JsResult<js::Value> {
    let state = state(env)?;
    let index = {
        let mut state = state.borrow_mut();
        let index = state.next;
        state.next += 1;
        state.bindings.insert(index, binding);
        index
    };
    let dispatch = js::function(
        env,
        &String::from_utf16_lossy(name),
        Some(callback),
        index as usize,
    )?;
    let name = js::text(env, name)?;
    let factory = state.borrow().factory.as_ref().ok_or(())?.value()?;
    let mut constructible = ptr::null_mut();
    js::check(unsafe {
        sys::napi_get_boolean(
            env,
            shape.is_some_and(|s| s.constructible),
            &mut constructible,
        )
    })?;
    let function = js::call(
        env,
        factory,
        js::undefined(env),
        &[dispatch, name, constructible],
    )?;
    if let Some(shape) = shape {
        crate::registry::function_shape(env, function, name, length, shape)?;
    }
    Ok(function)
}

/// Generic internal callback shaping; no product body or fixed receiver.
pub(crate) fn forward_internal(
    env: sys::napi_env,
    dispatch: js::Value,
    name: &str,
    length: u32,
    ordinary: bool,
) -> js::JsResult<js::Value> {
    let state = state(env)?;
    let factory = state.borrow().factory.as_ref().ok_or(())?.value()?;
    let name = js::string(env, name)?;
    let mut constructible = ptr::null_mut();
    js::check(unsafe { sys::napi_get_boolean(env, ordinary, &mut constructible) })?;
    let function = js::call(
        env,
        factory,
        js::undefined(env),
        &[dispatch, name, constructible],
    )?;
    let mut shape = HostFunctionShape::strict_function();
    if !ordinary {
        shape.constructible = false;
        shape.prototype = None;
    }
    crate::registry::function_shape(env, function, name, length, &shape)?;
    Ok(function)
}
pub(crate) fn read_value(
    env: sys::napi_env,
    value: js::Value,
    key: &str,
) -> js::JsResult<js::Value> {
    let function = state(env)?
        .borrow()
        .property_read
        .as_ref()
        .ok_or(())?
        .value()?;
    js::call(
        env,
        function,
        js::undefined(env),
        &[value, js::string(env, key)?],
    )
}
pub(crate) fn instance_of_value(
    env: sys::napi_env,
    value: js::Value,
    constructor: js::Value,
) -> js::JsResult<bool> {
    let function = state(env)?
        .borrow()
        .instance_of
        .as_ref()
        .ok_or(())?
        .value()?;
    let value = js::call(env, function, js::undefined(env), &[value, constructor])?;
    boolean(env, value)
}
