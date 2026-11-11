//! Real JS constructors/prototypes and one backend-object wrapper per ID.
//! Metadata defines descriptors; Rust backend bindings define behavior.
use crate::{
    evaluate as js,
    host::{self, Root},
};
use mnemosyne_runtime_protocol::*;
use napi::sys;
use std::{cell::RefCell, collections::HashMap, ptr, rc::Rc};
struct Class {
    constructor: Root,
}
struct Object {
    class: Vec<u16>,
    value: Root,
}
struct Registry {
    classes: HashMap<Vec<u16>, Class>,
    objects: HashMap<HostObjectId, Object>,
    set_prototype: Root,
    create: Root,
}
thread_local! {static REGISTRIES:RefCell<HashMap<usize,Rc<RefCell<Registry>>>>=RefCell::new(HashMap::new());}
fn registry(env: sys::napi_env) -> js::JsResult<Rc<RefCell<Registry>>> {
    if let Some(registry) = REGISTRIES.with(|r| r.borrow().get(&(env as usize)).cloned()) {
        return Ok(registry);
    }
    let object = js::get(env, js::global(env)?, "Object")?;
    let registry = Rc::new(RefCell::new(Registry {
        classes: HashMap::new(),
        objects: HashMap::new(),
        set_prototype: Root::new(env, js::get(env, object, "setPrototypeOf")?)?,
        create: Root::new(env, js::get(env, object, "create")?)?,
    }));
    js::check(unsafe { sys::napi_add_env_cleanup_hook(env, Some(cleanup), env.cast()) })?;
    REGISTRIES.with(|r| r.borrow_mut().insert(env as usize, registry.clone()));
    Ok(registry)
}
unsafe extern "C" fn cleanup(data: *mut std::ffi::c_void) {
    REGISTRIES.with(|r| r.borrow_mut().remove(&(data as usize)));
}
fn attributes(flags: PropertyFlags) -> sys::napi_property_attributes {
    (if flags.writable {
        sys::PropertyAttributes::writable
    } else {
        0
    }) | (if flags.enumerable {
        sys::PropertyAttributes::enumerable
    } else {
        0
    }) | (if flags.configurable {
        sys::PropertyAttributes::configurable
    } else {
        0
    })
}
pub(crate) fn data(
    env: sys::napi_env,
    object: js::Value,
    key: js::Value,
    value: js::Value,
    flags: PropertyFlags,
) -> js::JsResult<()> {
    let descriptor = sys::napi_property_descriptor {
        utf8name: ptr::null(),
        name: key,
        value,
        method: None,
        getter: None,
        setter: None,
        attributes: attributes(flags),
        data: ptr::null_mut(),
    };
    js::check(unsafe { sys::napi_define_properties(env, object, 1, &descriptor) })
}
pub(crate) fn function_shape(
    env: sys::napi_env,
    function: js::Value,
    name: js::Value,
    length: u32,
    shape: &HostFunctionShape,
) -> js::JsResult<()> {
    data(
        env,
        function,
        js::string(env, "name")?,
        name,
        shape.name_flags,
    )?;
    let mut value = ptr::null_mut();
    js::check(unsafe { sys::napi_create_uint32(env, length, &mut value) })?;
    data(
        env,
        function,
        js::string(env, "length")?,
        value,
        shape.length_flags,
    )?;
    if let Some(prototype) = &shape.prototype {
        if !shape.constructible {
            return Err(());
        }
        let object = js::get(env, function, "prototype")?;
        data(
            env,
            function,
            js::string(env, "prototype")?,
            object,
            prototype.flags,
        )?;
        data(
            env,
            object,
            js::string(env, "constructor")?,
            function,
            prototype.constructor_flags,
        )?;
    } else if shape.constructible {
        return Err(());
    }
    Ok(())
}
pub(crate) fn class(env: sys::napi_env, key: &[u16]) -> js::JsResult<js::Value> {
    registry(env)?
        .borrow()
        .classes
        .get(key)
        .ok_or(())?
        .constructor
        .value()
}
pub(crate) fn prototype(env: sys::napi_env, key: &[u16]) -> js::JsResult<js::Value> {
    js::get(env, class(env, key)?, "prototype")
}
fn key(env: sys::napi_env, key: HostKey) -> js::JsResult<js::Value> {
    match key {
        HostKey::Utf16(s) => js::text(env, &s),
        HostKey::Symbol(id) => host::lookup(env, id),
    }
}
pub(crate) fn define(
    env: sys::napi_env,
    object: js::Value,
    properties: Vec<HostDescriptor>,
) -> js::JsResult<()> {
    for property in properties {
        let key = key(env, property.key)?;
        match property.kind {
            HostDescriptorKind::Data { value, writable } => data(
                env,
                object,
                key,
                host::to_js(env, value)?,
                PropertyFlags {
                    writable,
                    enumerable: property.enumerable,
                    configurable: property.configurable,
                },
            )?,
            HostDescriptorKind::Method {
                binding,
                name,
                length,
                writable,
                shape,
            } => {
                let function = host::create_function(env, binding, &name, length, Some(&shape))?;
                data(
                    env,
                    object,
                    key,
                    function,
                    PropertyFlags {
                        writable,
                        enumerable: property.enumerable,
                        configurable: property.configurable,
                    },
                )?;
            }
            // Actual accessor functions, rather than a generic value getter
            // replacing writable SDK data fields.
            HostDescriptorKind::Accessor { get, set } => {
                let mut getter = js::undefined(env);
                let mut setter = js::undefined(env);
                if let Some(binding) = get {
                    getter = host::create_function(env, binding, &[], 0, None)?;
                }
                if let Some(binding) = set {
                    setter = host::create_function(env, binding, &[], 1, None)?;
                }
                let descriptor = js::call(
                    env,
                    js::get(env, js::global(env)?, "Object")?,
                    js::undefined(env),
                    &[],
                )?;
                js::set(env, descriptor, "get", getter)?;
                js::set(env, descriptor, "set", setter)?;
                let mut b = ptr::null_mut();
                js::check(unsafe { sys::napi_get_boolean(env, property.enumerable, &mut b) })?;
                js::set(env, descriptor, "enumerable", b)?;
                js::check(unsafe { sys::napi_get_boolean(env, property.configurable, &mut b) })?;
                js::set(env, descriptor, "configurable", b)?;
                let object_ctor = js::get(env, js::global(env)?, "Object")?;
                js::call(
                    env,
                    js::get(env, object_ctor, "defineProperty")?,
                    object_ctor,
                    &[object, key, descriptor],
                )?;
            }
        }
    }
    Ok(())
}
pub(crate) fn register_class(env: sys::napi_env, spec: HostClassSpec) -> js::JsResult<()> {
    let registry = registry(env)?;
    if registry.borrow().classes.contains_key(&spec.name) {
        return Err(());
    }
    let constructor = host::create_function(
        env,
        spec.constructor_binding,
        &spec.constructor_name,
        spec.constructor_length,
        Some(&spec.constructor_shape),
    )?;
    let proto = js::get(env, constructor, "prototype")?;
    if let Some(parent) = spec.parent {
        let parent_ctor = class(env, &parent)?;
        let parent_proto = prototype(env, &parent)?;
        let set = registry.borrow().set_prototype.value()?;
        js::call(env, set, js::undefined(env), &[constructor, parent_ctor])?;
        js::call(env, set, js::undefined(env), &[proto, parent_proto])?;
    }
    registry.borrow_mut().classes.insert(
        spec.name.clone(),
        Class {
            constructor: Root::new(env, constructor)?,
        },
    );
    if let Err(()) = define(env, proto, spec.prototype_members)
        .and_then(|_| define(env, constructor, spec.static_members))
    {
        registry.borrow_mut().classes.remove(&spec.name);
        return Err(());
    }
    Ok(())
}
pub(crate) fn object(
    env: sys::napi_env,
    id: HostObjectId,
    class: Vec<u16>,
) -> js::JsResult<js::Value> {
    let registry = registry(env)?;
    if let Some(object) = registry.borrow().objects.get(&id) {
        if object.class != class {
            return Err(());
        }
        return object.value.value();
    }
    let mut value = ptr::null_mut();
    if class == "Object".encode_utf16().collect::<Vec<_>>() {
        js::check(unsafe { sys::napi_create_object(env, &mut value) })?;
    } else if class == "Array".encode_utf16().collect::<Vec<_>>() {
        js::check(unsafe { sys::napi_create_array(env, &mut value) })?;
    } else {
        let proto = prototype(env, &class)?;
        if js::kind(env, proto)? == sys::ValueType::napi_object
            || js::kind(env, proto)? == sys::ValueType::napi_function
        {
            let create = registry.borrow().create.value()?;
            value = js::call(env, create, js::undefined(env), &[proto])?;
        } else {
            js::check(unsafe { sys::napi_create_object(env, &mut value) })?;
        }
    }
    registry.borrow_mut().objects.insert(
        id,
        Object {
            class,
            value: Root::new(env, value)?,
        },
    );
    Ok(value)
}
pub(crate) fn register_instance(
    env: sys::napi_env,
    id: HostObjectId,
    class: Vec<u16>,
    reference: Option<JsValueId>,
    properties: Vec<HostDescriptor>,
) -> js::JsResult<()> {
    let value = if let Some(reference) = reference {
        let value = host::lookup(env, reference)?;
        if js::kind(env, value)? != sys::ValueType::napi_object {
            return Err(());
        }
        let registry = registry(env)?;
        let existing = {
            let borrowed = registry.borrow();
            borrowed
                .objects
                .get(&id)
                .map(|existing| {
                    existing
                        .value
                        .value()
                        .map(|value| (existing.class.clone(), value))
                })
                .transpose()?
        };
        if let Some((existing_class, existing_value)) = existing {
            let mut equal = false;
            js::check(unsafe { sys::napi_strict_equals(env, value, existing_value, &mut equal) })?;
            if !equal || existing_class != class {
                return Err(());
            }
        } else {
            registry.borrow_mut().objects.insert(
                id,
                Object {
                    class,
                    value: Root::new(env, value)?,
                },
            );
        }
        value
    } else {
        object(env, id, class)?
    };
    define(env, value, properties)
}
pub(crate) fn update_instance(
    env: sys::napi_env,
    id: HostObjectId,
    properties: Vec<HostDescriptor>,
) -> js::JsResult<()> {
    let value = registry(env)?
        .borrow()
        .objects
        .get(&id)
        .ok_or(())?
        .value
        .value()?;
    define(env, value, properties)
}
pub(crate) fn identify(env: sys::napi_env, value: js::Value) -> js::JsResult<Option<HostObjectId>> {
    let registry = registry(env)?;
    for (id, object) in &registry.borrow().objects {
        let mut equal = false;
        js::check(unsafe {
            sys::napi_strict_equals(env, value, object.value.value()?, &mut equal)
        })?;
        if equal {
            return Ok(Some(*id));
        }
    }
    Ok(None)
}
