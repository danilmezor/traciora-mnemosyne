use super::*;
use deno_core::futures::{channel::oneshot, executor::block_on};
use std::task::Context;

fn from_js(scope: &mut v8::PinScope, value: v8::Local<v8::Value>, shared: &State) -> HostValue {
    if value.is_undefined() {
        return HostValue::Undefined;
    }
    if value.is_null() {
        return HostValue::Null;
    }
    if value.is_boolean() {
        return HostValue::Boolean(value.boolean_value(scope));
    }
    if value.is_number() {
        return HostValue::Number(value.number_value(scope).unwrap());
    }
    if value.is_string() {
        let value = v8::Local::<v8::String>::try_from(value).unwrap();
        let mut text = vec![0; value.length()];
        value.write_v2(scope, 0, &mut text, v8::WriteFlags::empty());
        return HostValue::Utf16(text);
    }
    if value.is_big_int() {
        let value = v8::Local::<v8::BigInt>::try_from(value).unwrap();
        let mut words = vec![0; value.word_count()];
        let (negative, used) = value.to_words_array(&mut words);
        let count = used.len();
        words.truncate(count);
        return HostValue::BigInt { negative, words };
    }
    let kind = if value.is_symbol() {
        ReferenceKind::Symbol
    } else if value.is_function() {
        ReferenceKind::Function
    } else {
        ReferenceKind::Object
    };
    let mut state = shared.borrow_mut();
    let index = state.references.iter().position(|root| {
        root.as_ref()
            .is_some_and(|root| v8::Local::new(scope, root).strict_equals(value))
    });
    let id = index.unwrap_or_else(|| {
        state.references.push(Some(v8::Global::new(scope, value)));
        state.references.len() - 1
    });
    HostValue::Reference(JsReference {
        id: JsValueId {
            context: state.context_id,
            index: id as u64,
        },
        kind,
    })
}
fn to_js<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    value: HostValue,
    shared: &State,
) -> Option<v8::Local<'s, v8::Value>> {
    Some(match value {
        HostValue::Undefined => v8::undefined(scope).into(),
        HostValue::Null => v8::null(scope).into(),
        HostValue::Boolean(value) => v8::Boolean::new(scope, value).into(),
        HostValue::Number(value) => v8::Number::new(scope, value).into(),
        HostValue::Utf16(value) => {
            v8::String::new_from_two_byte(scope, &value, v8::NewStringType::Normal)?.into()
        }
        HostValue::BigInt { negative, words } => {
            v8::BigInt::new_from_words(scope, negative, &words)?.into()
        }
        HostValue::Reference(reference) => {
            let state = shared.borrow();
            if state.context_id != reference.id.context {
                return None;
            }
            v8::Local::new(
                scope,
                state
                    .references
                    .get(usize::try_from(reference.id.index).ok()?)?
                    .as_ref()?,
            )
        }
        // No SDK prototype registry is installed in the reference backend.
        // Reject this value until real class wrappers are attached.
        HostValue::HostObject { .. }
        | HostValue::HostClass { .. }
        | HostValue::HostPrototype { .. } => return None,
        HostValue::Array(values) => {
            let array = v8::Array::new(scope, i32::try_from(values.len()).ok()?);
            for (index, value) in values.into_iter().enumerate() {
                let value = to_js(scope, value, shared)?;
                if array.set_index(scope, index as u32, value) != Some(true) {
                    return None;
                }
            }
            array.into()
        }
        HostValue::Record(values) => {
            let object = v8::Object::new(scope);
            for (name, value) in values {
                let name = v8::String::new_from_two_byte(scope, &name, v8::NewStringType::Normal)?;
                let value = to_js(scope, value, shared)?;
                // Define an own property; '__proto__' must not invoke the
                // legacy inherited setter for Rust-authored records.
                if object.create_data_property(scope, name.into(), value) != Some(true) {
                    return None;
                }
            }
            object.into()
        }
    })
}
fn exception<'s>(scope: &mut v8::PinScope<'s, '_>, error: HostError) -> v8::Local<'s, v8::Value> {
    if let Some(value) = error.thrown {
        let shared = state(scope);
        if let Some(value) = to_js(scope, *value, &shared) {
            return value;
        }
    }
    let message = key(scope, &error.message);
    let value = v8::Exception::error(scope, message);
    let object = value.to_object(scope).unwrap();
    let name = key(scope, &error.name);
    set(scope, object, "name", name.into());
    value
}
pub(super) fn callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    mut output: v8::ReturnValue,
) {
    let shared = state(scope);
    let index = args.data().uint32_value(scope).unwrap() as usize;
    let (binding, host) = {
        let state = shared.borrow();
        (state.bindings[index].clone(), state.host.clone())
    };
    let arguments = (0..args.length())
        .map(|index| from_js(scope, args.get(index), &shared))
        .collect();
    let delivery = binding.delivery;
    let (completion, result) = oneshot::channel();
    let request = HostRequest {
        call: HostCall {
            binding,
            receiver: from_js(scope, args.this().into(), &shared),
            new_target: from_js(scope, args.new_target(), &shared),
            arguments,
        },
        completion: if delivery == Delivery::VoidAsync {
            None
        } else {
            Some(completion)
        },
    };
    let pending = if delivery == Delivery::Promise {
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        output.set(resolver.get_promise(scope).into());
        Some(v8::Global::new(scope, resolver))
    } else {
        None
    };
    // A Rust panic never crosses the V8 callback ABI. Dropping a request's
    // completion translates it to a concrete rejected/throwing host error.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| host.call(request)));
    match delivery {
        Delivery::VoidAsync => {}
        Delivery::Promise => shared.borrow_mut().host_promises.push(PendingHostPromise {
            resolver: pending.unwrap(),
            result,
        }),
        Delivery::Sync => match block_on(result)
            .unwrap_or_else(|_| Err(HostError::unsupported("Rust backend request was dropped")))
        {
            Ok(value) => {
                if let Some(value) = to_js(scope, value, &shared) {
                    output.set(value);
                } else {
                    let value = exception(
                        scope,
                        HostError::unsupported("Invalid Rust backend value/reference"),
                    );
                    scope.throw_exception(value);
                }
            }
            Err(error) => {
                let value = exception(scope, error);
                scope.throw_exception(value);
            }
        },
    }
}
pub(super) fn poll(js: &mut JsRuntime, shared: &State, cx: &mut Context<'_>) {
    let mut ready = Vec::new();
    {
        let mut state = shared.borrow_mut();
        let mut index = 0;
        while index < state.host_promises.len() {
            match Pin::new(&mut state.host_promises[index].result).poll(cx) {
                Poll::Ready(result) => {
                    let pending = state.host_promises.remove(index);
                    ready.push((
                        pending.resolver,
                        result.unwrap_or_else(|_| {
                            Err(HostError::unsupported("Rust backend request was dropped"))
                        }),
                    ));
                }
                Poll::Pending => index += 1,
            }
        }
    }
    if ready.is_empty() {
        return;
    }
    deno_core::scope!(scope, js);
    for (resolver, result) in ready {
        let resolver = v8::Local::new(scope, resolver);
        match result {
            Ok(value) => {
                if let Some(value) = to_js(scope, value, shared) {
                    resolver.resolve(scope, value);
                } else {
                    let error = exception(
                        scope,
                        HostError::unsupported("Invalid Rust backend value/reference"),
                    );
                    resolver.reject(scope, error);
                }
            }
            Err(error) => {
                let error = exception(scope, error);
                resolver.reject(scope, error);
            }
        }
    }
}
