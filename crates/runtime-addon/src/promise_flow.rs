//! Source-directed async control flow lives in Rust. Constructors, instanceof,
//! resolve functions and mutable then properties remain genuine realm values.
use crate::{evaluate as js, host};
use napi::sys;
use std::{cell::RefCell, collections::HashMap, ptr, rc::Rc};
type Owned = Rc<host::Root>;
#[derive(Clone)]
pub(crate) enum Work {
    Handler(Option<Vec<u16>>),
    Handle(Option<Vec<u16>>),
    Queue(Option<Vec<u16>>),
    Adopt(Owned),
}
enum Callback {
    Executor {
        work: Work,
        constructor: Owned,
    },
    Forward {
        resolve: Owned,
        reject: Owned,
        rejected: bool,
    },
    Resume {
        capture: usize,
        rejected: bool,
    },
}
struct Callbacks {
    next: usize,
    values: HashMap<usize, Rc<Callback>>,
}
thread_local! {static CALLBACKS:RefCell<HashMap<usize,Callbacks>>=RefCell::new(HashMap::new());}
fn own(env: sys::napi_env, value: js::Value) -> js::JsResult<Owned> {
    Ok(Rc::new(host::Root::new(env, value)?))
}
unsafe extern "C" fn cleanup(data: *mut std::ffi::c_void) {
    CALLBACKS.with(|all| {
        all.borrow_mut().remove(&(data as usize));
    });
}
unsafe extern "C" fn finalize(
    env: sys::napi_env,
    data: *mut std::ffi::c_void,
    _: *mut std::ffi::c_void,
) {
    CALLBACKS.with(|all| {
        if let Some(callbacks) = all.borrow_mut().get_mut(&(env as usize)) {
            callbacks.values.remove(&(data as usize));
        }
    });
}
fn callback(
    env: sys::napi_env,
    value: Callback,
    name: &str,
    length: u32,
    ordinary: bool,
) -> js::JsResult<js::Value> {
    let mut fresh = false;
    let id = CALLBACKS.with(|all| {
        let mut all = all.borrow_mut();
        let callbacks = all.entry(env as usize).or_insert_with(|| {
            fresh = true;
            Callbacks {
                next: 1,
                values: HashMap::new(),
            }
        });
        let id = callbacks.next;
        callbacks.next += 1;
        callbacks.values.insert(id, Rc::new(value));
        id
    });
    if fresh {
        js::check(unsafe { sys::napi_add_env_cleanup_hook(env, Some(cleanup), env.cast()) })?;
    }
    let raw = js::function(env, name, Some(dispatch), id)?;
    js::check(unsafe {
        sys::napi_add_finalizer(
            env,
            raw,
            id as *mut _,
            Some(finalize),
            ptr::null_mut(),
            ptr::null_mut(),
        )
    })?;
    host::forward_internal(env, raw, name, length, ordinary)
}
fn unpack(env: sys::napi_env, args: Vec<js::Value>) -> js::JsResult<Vec<js::Value>> {
    let array = *args.get(2).ok_or(())?;
    let mut count = 0;
    js::check(unsafe { sys::napi_get_array_length(env, array, &mut count) })?;
    (0..count)
        .map(|index| {
            let mut value = ptr::null_mut();
            js::check(unsafe { sys::napi_get_element(env, array, index, &mut value) })?;
            Ok(value)
        })
        .collect()
}
pub(crate) fn type_error(env: sys::napi_env, message: &str) -> js::JsResult<()> {
    let text = std::ffi::CString::new(message).map_err(|_| ())?;
    js::check(unsafe { sys::napi_throw_type_error(env, ptr::null(), text.as_ptr()) })?;
    Err(())
}
fn invoke(
    env: sys::napi_env,
    function: js::Value,
    receiver: js::Value,
    args: &[js::Value],
    label: &str,
) -> js::JsResult<js::Value> {
    if js::kind(env, function)? != sys::ValueType::napi_function {
        return type_error(env, &format!("{label} is not a function")).and(Err(()));
    }
    js::call(env, function, receiver, args)
}
pub(crate) fn construct(
    env: sys::napi_env,
    constructor: js::Value,
    work: Work,
) -> js::JsResult<js::Value> {
    let ordinary = !matches!(work, Work::Queue(_));
    let length = if matches!(work, Work::Adopt(_)) { 1 } else { 2 };
    let executor = callback(
        env,
        Callback::Executor {
            work,
            constructor: own(env, constructor)?,
        },
        "",
        length,
        ordinary,
    )?;
    let mut result = ptr::null_mut();
    js::check(unsafe { sys::napi_new_instance(env, constructor, 1, &executor, &mut result) })?;
    Ok(result)
}
pub(crate) fn current(env: sys::napi_env, work: Work) -> js::JsResult<js::Value> {
    construct(env, js::get(env, js::global(env)?, "Promise")?, work)
}
fn adopted(
    env: sys::napi_env,
    constructor: js::Value,
    value: js::Value,
) -> js::JsResult<js::Value> {
    if host::instance_of_value(env, value, constructor)? {
        Ok(value)
    } else {
        construct(env, constructor, Work::Adopt(own(env, value)?))
    }
}
pub(crate) fn await_eval(
    env: sys::napi_env,
    constructor: js::Value,
    value: js::Value,
    capture: usize,
) -> js::JsResult<()> {
    let value = adopted(env, constructor, value)?;
    let then = host::read_value(env, value, "then")?;
    let fulfilled = callback(
        env,
        Callback::Resume {
            capture,
            rejected: false,
        },
        "a",
        1,
        true,
    )?;
    let rejected = callback(
        env,
        Callback::Resume {
            capture,
            rejected: true,
        },
        "s",
        1,
        true,
    )?;
    invoke(
        env,
        then,
        value,
        &[fulfilled, rejected],
        "(t , (intermediate value)(intermediate value)(intermediate value)).then",
    )?;
    Ok(())
}
fn await_forward(
    env: sys::napi_env,
    constructor: js::Value,
    value: js::Value,
    resolve: Owned,
    reject: Owned,
) -> js::JsResult<()> {
    let value = adopted(env, constructor, value)?;
    let then = host::read_value(env, value, "then")?;
    let fulfilled = callback(
        env,
        Callback::Forward {
            resolve: resolve.clone(),
            reject: reject.clone(),
            rejected: false,
        },
        "a",
        1,
        true,
    )?;
    let rejected = callback(
        env,
        Callback::Forward {
            resolve,
            reject,
            rejected: true,
        },
        "s",
        1,
        true,
    )?;
    invoke(
        env,
        then,
        value,
        &[fulfilled, rejected],
        "(t , (intermediate value)(intermediate value)(intermediate value)).then",
    )?;
    Ok(())
}
unsafe extern "C" fn dispatch(env: sys::napi_env, info: sys::napi_callback_info) -> js::Value {
    js::guarded(env, || {
        let (args, id) = js::callback_args(env, info)?;
        let callback = CALLBACKS
            .with(|all| {
                all.borrow()
                    .get(&(env as usize))
                    .and_then(|c| c.values.get(&id))
                    .cloned()
            })
            .ok_or(())?;
        let args = unpack(env, args)?;
        match callback.as_ref() {
            Callback::Executor { work, constructor } => {
                let resolve = own(
                    env,
                    args.first().copied().unwrap_or_else(|| js::undefined(env)),
                )?;
                if let Work::Adopt(value) = work {
                    invoke(
                        env,
                        resolve.value()?,
                        js::undefined(env),
                        &[value.value()?],
                        "e",
                    )?;
                } else {
                    let reject = own(
                        env,
                        args.get(1).copied().unwrap_or_else(|| js::undefined(env)),
                    )?;
                    match work {
                        Work::Handler(code) => js::begin_handler(
                            env,
                            code.clone(),
                            constructor.clone(),
                            resolve,
                            reject,
                        )?,
                        Work::Handle(code) => {
                            let console = js::get(env, js::global(env)?, "console")?;
                            let log = js::get(env, console, "log")?;
                            let mut argv =
                                vec![mnemosyne_runtime_protocol::HostValue::from("eval")];
                            if let Some(code) = code {
                                let mut argument = "code=".encode_utf16().collect::<Vec<_>>();
                                argument.extend(code);
                                argv.push(mnemosyne_runtime_protocol::HostValue::Utf16(argument));
                            }
                            invoke(
                                env,
                                log,
                                console,
                                &[
                                    js::string(env, "Received CLI command")?,
                                    host::to_js(
                                        env,
                                        mnemosyne_runtime_protocol::HostValue::Array(argv),
                                    )?,
                                ],
                                "console.log",
                            )?;
                            if code.is_none() {
                                js::check(unsafe {
                                    sys::napi_throw(
                                        env,
                                        js::string(
                                            env,
                                            "Missing required parameter: code=<javascript>\nUsage: eval code=<javascript>",
                                        )?,
                                    )
                                })?;
                                return Err(());
                            }
                            let child = current(env, Work::Handler(code.clone()))?;
                            await_forward(env, constructor.value()?, child, resolve, reject)?;
                        }
                        Work::Queue(code) => {
                            // Call-expression evaluation retains the base and
                            // callee BEFORE executing the handler argument.
                            let base = js::get(env, js::global(env)?, "Promise")?;
                            let callee = host::read_value(env, base, "resolve")?;
                            let child = current(env, Work::Handle(code.clone()))?;
                            let awaited = invoke(env, callee, base, &[child], "Promise.resolve")?;
                            let then = host::read_value(env, awaited, "then")?;
                            invoke(
                                env,
                                then,
                                awaited,
                                &[resolve.value()?, reject.value()?],
                                "Promise.resolve(...).then",
                            )?;
                        }
                        Work::Adopt(_) => unreachable!(),
                    }
                }
            }
            Callback::Forward {
                resolve,
                reject,
                rejected,
            } => {
                let value = args.first().copied().unwrap_or_else(|| js::undefined(env));
                let function = if *rejected { reject } else { resolve };
                if invoke(
                    env,
                    function.value()?,
                    js::undefined(env),
                    &[value],
                    if *rejected { "o" } else { "r" },
                )
                .is_err()
                {
                    let error = js::exception(env)?;
                    invoke(env, reject.value()?, js::undefined(env), &[error], "o")?;
                }
            }
            Callback::Resume { capture, rejected } => {
                js::resume_handler(
                    env,
                    *capture,
                    args.first().copied().unwrap_or_else(|| js::undefined(env)),
                    *rejected,
                )?;
            }
        }
        Ok(js::undefined(env))
    })
}
