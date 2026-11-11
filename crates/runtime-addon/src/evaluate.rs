//! Native eval orchestration is Rust. Every opaque Node-API value/reference
//! below is confined to its creating JS thread; no unsafe Send is implemented.
//! Private FFI helpers accept only handles received/created in that environment.
use napi::{
    Env,
    bindgen_prelude::{FromNapiValue, Unknown, Utf16String},
    sys,
};
use std::{
    cell::RefCell,
    collections::HashMap,
    ffi::{CString, c_void},
    ptr,
    rc::Rc,
};

pub(crate) type Value = sys::napi_value;
pub(crate) type JsResult<T> = Result<T, ()>;
pub(crate) fn check(status: sys::napi_status) -> JsResult<()> {
    if status == sys::Status::napi_ok {
        Ok(())
    } else {
        Err(())
    }
}
pub(crate) fn global(env: sys::napi_env) -> JsResult<Value> {
    let mut value = ptr::null_mut();
    check(unsafe { sys::napi_get_global(env, &mut value) })?;
    Ok(value)
}
pub(crate) fn get(env: sys::napi_env, object: Value, name: &str) -> JsResult<Value> {
    let name = CString::new(name).unwrap();
    let mut value = ptr::null_mut();
    check(unsafe { sys::napi_get_named_property(env, object, name.as_ptr(), &mut value) })?;
    Ok(value)
}
pub(crate) fn set(env: sys::napi_env, object: Value, name: &str, value: Value) -> JsResult<()> {
    let name = CString::new(name).unwrap();
    check(unsafe { sys::napi_set_named_property(env, object, name.as_ptr(), value) })
}
pub(crate) fn kind(env: sys::napi_env, value: Value) -> JsResult<sys::napi_valuetype> {
    let mut result = sys::ValueType::napi_undefined;
    check(unsafe { sys::napi_typeof(env, value, &mut result) })?;
    Ok(result)
}
pub(crate) fn undefined(env: sys::napi_env) -> Value {
    let mut value = ptr::null_mut();
    unsafe { sys::napi_get_undefined(env, &mut value) };
    value
}
pub(crate) fn text(env: sys::napi_env, value: &[u16]) -> JsResult<Value> {
    let mut result = ptr::null_mut();
    check(unsafe {
        sys::napi_create_string_utf16(env, value.as_ptr(), value.len() as isize, &mut result)
    })?;
    Ok(result)
}
pub(crate) fn string(env: sys::napi_env, value: &str) -> JsResult<Value> {
    text(env, &value.encode_utf16().collect::<Vec<_>>())
}
pub(crate) fn read_text(env: sys::napi_env, value: Value) -> JsResult<String> {
    Ok(String::from_utf16_lossy(&read_units(env, value)?))
}
pub(crate) fn read_units(env: sys::napi_env, value: Value) -> JsResult<Vec<u16>> {
    let mut len = 0;
    check(unsafe { sys::napi_get_value_string_utf16(env, value, ptr::null_mut(), 0, &mut len) })?;
    let mut units = vec![0; len + 1];
    let mut written = 0;
    check(unsafe {
        sys::napi_get_value_string_utf16(env, value, units.as_mut_ptr(), units.len(), &mut written)
    })?;
    units.truncate(written);
    Ok(units)
}
pub(crate) fn call(
    env: sys::napi_env,
    function: Value,
    receiver: Value,
    args: &[Value],
) -> JsResult<Value> {
    let mut value = ptr::null_mut();
    check(unsafe {
        sys::napi_call_function(
            env,
            receiver,
            function,
            args.len(),
            args.as_ptr(),
            &mut value,
        )
    })?;
    Ok(value)
}
pub(crate) fn implicit(env: sys::napi_env, value: Value) -> JsResult<String> {
    let mut result = ptr::null_mut();
    check(unsafe { sys::napi_coerce_to_string(env, value, &mut result) })?;
    read_text(env, result)
}
pub(crate) fn explicit(env: sys::napi_env, value: Value) -> JsResult<String> {
    let global = global(env)?;
    let function = get(env, global, "String")?;
    implicit(env, call(env, function, undefined(env), &[value])?)
}
fn result_prefix(env: sys::napi_env, value: Value) -> JsResult<String> {
    let operator = state(env)?.borrow().add.clone();
    read_text(
        env,
        call(
            env,
            operator.value()?,
            undefined(env),
            &[string(env, "=> ")?, value],
        )?,
    )
}
fn scalar_prefix(env: sys::napi_env, value: Value) -> JsResult<String> {
    let function = get(env, global(env)?, "String")?;
    result_prefix(env, call(env, function, undefined(env), &[value])?)
}
pub(crate) fn json(env: sys::napi_env, value: Value, pretty: bool) -> JsResult<Value> {
    let object = get(env, global(env)?, "JSON")?;
    let function = get(env, object, "stringify")?;
    if pretty {
        let mut null = ptr::null_mut();
        unsafe { sys::napi_get_null(env, &mut null) };
        let mut gap = ptr::null_mut();
        unsafe { sys::napi_create_int32(env, 2, &mut gap) };
        call(env, function, object, &[value, null, gap])
    } else {
        call(env, function, object, &[value])
    }
}
pub(crate) fn exception(env: sys::napi_env) -> JsResult<Value> {
    let mut value = ptr::null_mut();
    check(unsafe { sys::napi_get_and_clear_last_exception(env, &mut value) })?;
    Ok(value)
}

struct Root {
    env: sys::napi_env,
    reference: sys::napi_ref,
}
impl Root {
    fn new(env: sys::napi_env, value: Value) -> JsResult<Rc<Self>> {
        let mut reference = ptr::null_mut();
        check(unsafe { sys::napi_create_reference(env, value, 1, &mut reference) })?;
        Ok(Rc::new(Self { env, reference }))
    }
    fn value(&self) -> JsResult<Value> {
        let mut value = ptr::null_mut();
        check(unsafe { sys::napi_get_reference_value(self.env, self.reference, &mut value) })?;
        Ok(value)
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        unsafe { sys::napi_delete_reference(self.env, self.reference) };
    }
}
struct Wrapper {
    capture: usize,
    frame: Option<u64>,
    original: Rc<Root>,
    console: Rc<Root>,
    level: usize,
}
struct Capture {
    logs: Vec<String>,
    console: Rc<Root>,
    originals: Vec<Rc<Root>>,
    resolve: Rc<crate::host::Root>,
    reject: Rc<crate::host::Root>,
    frame: Option<u64>,
}
struct State {
    add: Rc<Root>,
    then: Rc<Root>,
    captures: HashMap<usize, Capture>,
    next_capture: usize,
    wrappers: HashMap<usize, Rc<Wrapper>>,
    next_wrapper: usize,
}
type Shared = Rc<RefCell<State>>;
thread_local! {static STATES:RefCell<HashMap<usize,Shared>>=RefCell::new(HashMap::new());}
fn state(env: sys::napi_env) -> JsResult<Shared> {
    if let Some(state) = STATES.with(|states| states.borrow().get(&(env as usize)).cloned()) {
        return Ok(state);
    }
    let global = global(env)?;
    let promise = get(env, global, "Promise")?;
    let state = Rc::new(RefCell::new(State {
        add: {
            // Generic VM operator, independent of mutable global eval and of
            // any SDK/product body. Default ToPrimitive must be performed by VM.
            let source = string(env, "(function(left,right){return left+right})")?;
            let mut operator = ptr::null_mut();
            check(unsafe { sys::napi_run_script(env, source, &mut operator) })?;
            Root::new(env, operator)?
        },
        then: Root::new(env, get(env, get(env, promise, "prototype")?, "then")?)?,
        captures: HashMap::new(),
        next_capture: 1,
        wrappers: HashMap::new(),
        next_wrapper: 1,
    }));
    check(unsafe { sys::napi_add_env_cleanup_hook(env, Some(cleanup), env.cast()) })?;
    STATES.with(|states| states.borrow_mut().insert(env as usize, state.clone()));
    Ok(state)
}
unsafe extern "C" fn cleanup(data: *mut c_void) {
    STATES.with(|states| {
        states.borrow_mut().remove(&(data as usize));
    });
}
pub(crate) fn callback_args(
    env: sys::napi_env,
    info: sys::napi_callback_info,
) -> JsResult<(Vec<Value>, usize)> {
    let mut len = 0;
    let mut data = ptr::null_mut();
    check(unsafe {
        sys::napi_get_cb_info(
            env,
            info,
            &mut len,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut data,
        )
    })?;
    let mut values = vec![ptr::null_mut(); len];
    check(unsafe {
        sys::napi_get_cb_info(
            env,
            info,
            &mut len,
            values.as_mut_ptr(),
            ptr::null_mut(),
            &mut data,
        )
    })?;
    Ok((values, data as usize))
}
pub(crate) fn function(
    env: sys::napi_env,
    name: &str,
    callback: sys::napi_callback,
    data: usize,
) -> JsResult<Value> {
    let mut value = ptr::null_mut();
    check(unsafe {
        sys::napi_create_function(
            env,
            name.as_ptr().cast(),
            name.len() as isize,
            callback,
            data as *mut c_void,
            &mut value,
        )
    })?;
    Ok(value)
}
pub(crate) fn guarded(env: sys::napi_env, work: impl FnOnce() -> JsResult<Value>) -> Value {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)) {
        Ok(Ok(value)) => value,
        Ok(Err(())) => ptr::null_mut(),
        Err(_) => {
            unsafe {
                sys::napi_throw_error(env, ptr::null(), c"Rust runtime callback panicked".as_ptr())
            };
            ptr::null_mut()
        }
    }
}
fn console_argument(env: sys::napi_env, value: Value) -> JsResult<String> {
    match kind(env, value)? {
        sys::ValueType::napi_undefined => Ok("undefined".into()),
        sys::ValueType::napi_null => Ok("null".into()),
        sys::ValueType::napi_object => match json(env, value, false) {
            Ok(value) => {
                if kind(env, value)? == sys::ValueType::napi_undefined {
                    Ok(String::new())
                } else {
                    implicit(env, value)
                }
            }
            Err(()) => {
                exception(env)?;
                explicit(env, value)
            }
        },
        _ => explicit(env, value),
    }
}
unsafe extern "C" fn console(env: sys::napi_env, info: sys::napi_callback_info) -> Value {
    guarded(env, || {
        let (args, id) = callback_args(env, info)?;
        let shared = state(env)?;
        let wrapper = shared.borrow().wrappers.get(&id).cloned().ok_or(())?;
        let parts = args
            .iter()
            .map(|value| console_argument(env, *value))
            .collect::<JsResult<Vec<_>>>()?;
        let message = parts.join(" ");
        let line = match wrapper.level {
            1 => format!("[warn] {message}"),
            2 => format!("[error] {message}"),
            _ => message.clone(),
        };
        if let Some(capture) = shared.borrow_mut().captures.get_mut(&wrapper.capture) {
            capture.logs.push(line);
        }
        #[cfg(unix)]
        crate::ipc::console_event_for(env, wrapper.frame, wrapper.level, &message)?;
        call(
            env,
            wrapper.original.value()?,
            get(env, global(env)?, "console")?,
            &args,
        )?;
        Ok(undefined(env))
    })
}
unsafe extern "C" fn finalize_wrapper(env: sys::napi_env, data: *mut c_void, _: *mut c_void) {
    if let Some(shared) = STATES.with(|states| states.borrow().get(&(env as usize)).cloned()) {
        shared.borrow_mut().wrappers.remove(&(data as usize));
    }
}
fn error_body(env: sys::napi_env, error: Value, mut logs: Vec<String>) -> JsResult<String> {
    // The original exception stays a genuine realm value. Property access and
    // concat may themselves throw; that exits the catch and still runs finally.
    let message = crate::host::read_value(env, error, "message")?;
    let mut truthy = ptr::null_mut();
    check(unsafe { sys::napi_coerce_to_bool(env, message, &mut truthy) })?;
    let mut selected = false;
    check(unsafe { sys::napi_get_value_bool(env, truthy, &mut selected) })?;
    let prefix = string(env, "Error: ")?;
    let concat = crate::host::read_value(env, prefix, "concat")?;
    logs.push(read_text(
        env,
        call(
            env,
            concat,
            prefix,
            &[if selected { message } else { error }],
        )?,
    )?);
    Ok(logs.join("\n"))
}
fn restore(env: sys::napi_env, shared: &Shared, id: usize) -> JsResult<()> {
    let (console, originals) = {
        let state = shared.borrow();
        let capture = state.captures.get(&id).ok_or(())?;
        (capture.console.clone(), capture.originals.clone())
    };
    for (name, original) in ["log", "warn", "error"].into_iter().zip(originals) {
        let current = get(env, global(env)?, "console")?;
        set(env, current, name, original.value()?)?;
    }
    Ok(())
}
fn complete(
    env: sys::napi_env,
    shared: &Shared,
    id: usize,
    value: Value,
    rejected: bool,
) -> JsResult<Value> {
    let logs = shared.borrow().captures.get(&id).ok_or(())?.logs.clone();
    let body = if rejected {
        error_body(env, value, logs)
    } else {
        let result = (|| {
            Ok(match kind(env, value)? {
                sys::ValueType::napi_undefined => None,
                sys::ValueType::napi_null => Some("=> null".into()),
                sys::ValueType::napi_object => Some(result_prefix(env, json(env, value, true)?)?),
                _ => Some(scalar_prefix(env, value)?),
            })
        })();
        match result {
            Ok(result) => {
                let mut lines = logs;
                if let Some(result) = result {
                    lines.push(result);
                }
                if lines.is_empty() {
                    Ok("(no output)".into())
                } else {
                    Ok(lines.join("\n"))
                }
            }
            Err(()) => {
                let error = exception(env)?;
                let logs = shared.borrow().captures.get(&id).ok_or(())?.logs.clone();
                error_body(env, error, logs)
            }
        }
    };
    let failure = if body.is_err() {
        Some(Rc::new(crate::host::Root::new(env, exception(env)?)?))
    } else {
        None
    };
    restore(env, shared, id)?;
    if let Some(error) = failure {
        check(unsafe { sys::napi_throw(env, error.value()?) })?;
        return Err(());
    }
    let body = body?;
    let resolve = shared.borrow().captures.get(&id).ok_or(())?.resolve.clone();
    let body = string(env, &body)?;
    call(env, resolve.value()?, undefined(env), &[body])?;
    finish_handler(env, id)?;
    Ok(undefined(env))
}
pub(crate) fn initialize(env: sys::napi_env) -> JsResult<()> {
    state(env)?;
    Ok(())
}
fn finish_handler(env: sys::napi_env, id: usize) -> JsResult<()> {
    if let Some(capture) = state(env)?.borrow_mut().captures.remove(&id) {
        crate::ipc::handler_finished(env, capture.frame, id)?;
    }
    Ok(())
}
pub(crate) fn resume_handler(
    env: sys::napi_env,
    id: usize,
    value: Value,
    rejected: bool,
) -> JsResult<()> {
    let shared = state(env)?;
    let frame = shared.borrow().captures.get(&id).ok_or(())?.frame;
    crate::ipc::with_eval(env, frame, || {
        if complete(env, &shared, id, value, rejected).is_err() {
            let error = exception(env)?;
            let reject = shared.borrow().captures.get(&id).ok_or(())?.reject.clone();
            let result = call(env, reject.value()?, undefined(env), &[error]);
            finish_handler(env, id)?;
            result?;
        }
        Ok(())
    })
}
pub(crate) fn begin_handler(
    env: sys::napi_env,
    code: Option<Vec<u16>>,
    constructor: Rc<crate::host::Root>,
    resolve: Rc<crate::host::Root>,
    reject: Rc<crate::host::Root>,
) -> JsResult<()> {
    let Some(code) = code.filter(|code| !code.is_empty()) else {
        check(unsafe {
            sys::napi_throw(
                env,
                string(
                    env,
                    "Missing required parameter: code\nUsage: eval code=<javascript>",
                )?,
            )
        })?;
        return Err(());
    };
    let shared = state(env)?;
    let console = Root::new(env, get(env, global(env)?, "console")?)?;
    let originals = ["log", "warn", "error"]
        .into_iter()
        .map(|name| Root::new(env, get(env, console.value()?, name)?))
        .collect::<JsResult<Vec<_>>>()?;
    let frame = crate::ipc::active_eval(env)?;
    let capture = {
        let mut state = shared.borrow_mut();
        let id = state.next_capture;
        state.next_capture += 1;
        state.captures.insert(
            id,
            Capture {
                logs: Vec::new(),
                console: console.clone(),
                originals: originals.clone(),
                resolve,
                reject,
                frame,
            },
        );
        id
    };
    crate::ipc::handler_started(env, frame, capture)?;
    for (level, (name, original)) in ["log", "warn", "error"]
        .into_iter()
        .zip(originals)
        .enumerate()
    {
        let index = {
            let mut state = shared.borrow_mut();
            let id = state.next_wrapper;
            state.next_wrapper += 1;
            state.wrappers.insert(
                id,
                Rc::new(Wrapper {
                    capture,
                    frame,
                    original,
                    console: console.clone(),
                    level,
                }),
            );
            id
        };
        let wrapper = function(env, name, Some(self::console), index)?;
        check(unsafe {
            sys::napi_add_finalizer(
                env,
                wrapper,
                index as *mut _,
                Some(finalize_wrapper),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        })?;
        set(env, console.value()?, name, wrapper)?;
    }
    let evaluated = (|| {
        let receiver = global(env)?;
        let eval = get(env, receiver, "eval")?;
        if kind(env, eval)? != sys::ValueType::napi_function {
            return crate::promise_flow::type_error(env, "window.eval is not a function")
                .and(Err(()));
        }
        call(env, eval, receiver, &[text(env, &code)?])
    })();
    match evaluated {
        Ok(value) => {
            if crate::promise_flow::await_eval(env, constructor.value()?, value, capture).is_err() {
                // Awaiter adoption errors do not enter the generator's catch
                // or finally. Preserve physical console state and original throw.
                let error = exception(env)?;
                finish_handler(env, capture)?;
                check(unsafe { sys::napi_throw(env, error) })?;
                return Err(());
            }
        }
        Err(()) => {
            let error = exception(env)?;
            if complete(env, &shared, capture, error, true).is_err() {
                let error = exception(env)?;
                finish_handler(env, capture)?;
                check(unsafe { sys::napi_throw(env, error) })?;
                return Err(());
            }
        }
    }
    Ok(())
}
pub fn evaluate<'env>(env: &'env Env, code: Option<Utf16String>) -> napi::Result<Unknown<'env>> {
    let value = crate::promise_flow::current(
        env.raw(),
        crate::promise_flow::Work::Handler(code.map(|value| value.to_vec())),
    );
    match value {
        Ok(value) => unsafe { Unknown::from_napi_value(env.raw(), value) },
        Err(()) => Err(napi::Error::new(
            napi::Status::PendingException,
            "Native async handler failed",
        )),
    }
}
pub(crate) fn evaluate_cli<'env>(
    env: &'env Env,
    code: Option<Utf16String>,
) -> napi::Result<Unknown<'env>> {
    let value = crate::promise_flow::current(
        env.raw(),
        crate::promise_flow::Work::Queue(code.map(|value| value.to_vec())),
    );
    match value {
        Ok(value) => unsafe { Unknown::from_napi_value(env.raw(), value) },
        Err(()) => Err(napi::Error::new(
            napi::Status::PendingException,
            "Native CLI boundary failed",
        )),
    }
}

pub(crate) fn react(
    env: sys::napi_env,
    promise: Value,
    fulfilled: Value,
    rejected: Value,
) -> JsResult<Value> {
    let shared = state(env)?;
    let then = shared.borrow().then.clone();
    call(env, then.value()?, promise, &[fulfilled, rejected])
}
