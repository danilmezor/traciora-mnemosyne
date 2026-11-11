//! Rust-owned, persistent JavaScript contexts and the bounded native eval
//! contract. Node/Electron and SDK bindings remain separate required layers.
use crate::{ConsoleLevel, EvalOutcome, RuntimeError};
use deno_core::{JsRuntime, PollEventLoopOptions, RuntimeOptions, v8};
use std::{
    cell::RefCell,
    future::{Future, poll_fn},
    pin::Pin,
    rc::Rc,
    sync::atomic::{AtomicU64, Ordering},
    task::Poll,
    time::{Duration, Instant},
};

#[path = "thread.rs"]
mod thread;
pub use thread::RuntimeHandle;
#[path = "host.rs"]
mod host;
pub use host::*;
#[path = "host_v8.rs"]
mod host_v8;

/// Call on the application's main thread before creating runtime-owning
/// workers. V8 requires their common parent to initialize its platform.
pub fn initialize_platform() {
    JsRuntime::init_platform(None);
}

pub const TARGET_V8_VERSION: &str = "15.0.245.31-electron.0";
pub fn backend_version() -> &'static str {
    v8::V8::get_version()
}

/// Rust callbacks remain on the context's owning thread. No engine dependency,
/// Rust object serialization, raw pointers, or JS product implementation is
/// required. Future SDK ops can use the same owning application's Rust state.
pub trait HostCallbacks {
    fn console(&self, _level: ConsoleLevel, _message: &str) {}
    fn call(&self, request: HostRequest) {
        request.respond(Err(HostError::unsupported(
            "No Rust backend dispatcher is attached",
        )));
    }
}
struct NoHost;
impl HostCallbacks for NoHost {}

struct Timer {
    id: u32,
    deadline: Instant,
    callback: v8::Global<v8::Function>,
    arguments: Vec<v8::Global<v8::Value>>,
}
struct Capture {
    logs: Vec<String>,
    body: Option<String>,
    capturing: bool,
    console: v8::Global<v8::Object>,
    originals: Vec<v8::Global<v8::Value>>,
}
struct Shared {
    capture: Option<Capture>,
    timers: Vec<Timer>,
    next_timer: u32,
    host: Rc<dyn HostCallbacks>,
    bindings: Vec<HostBinding>,
    context_id: u64,
    references: Vec<Option<v8::Global<v8::Value>>>,
    host_promises: Vec<PendingHostPromise>,
}
struct PendingHostPromise {
    resolver: v8::Global<v8::PromiseResolver>,
    result: deno_core::futures::channel::oneshot::Receiver<HostResult>,
}
type State = Rc<RefCell<Shared>>;

/// A context is deliberately !Send/!Sync. The Rust application owns one per
/// retained vault session and must drive it on the creating thread.
pub struct Runtime {
    intrinsic_eval: v8::Global<v8::Function>,
    intrinsic_promise: v8::Global<v8::Function>,
    intrinsic_resolve: v8::Global<v8::Function>,
    state: State,
    js: JsRuntime,
    executor: tokio::runtime::Runtime,
}

fn state(scope: &v8::PinScope) -> State {
    scope.get_slot::<State>().unwrap().clone()
}
fn key<'s>(scope: &mut v8::PinScope<'s, '_>, name: &str) -> v8::Local<'s, v8::String> {
    v8::String::new(scope, name).unwrap()
}
fn get<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    object: v8::Local<'s, v8::Object>,
    name: &str,
) -> Option<v8::Local<'s, v8::Value>> {
    let name = key(scope, name);
    object.get(scope, name.into())
}
fn set(
    scope: &mut v8::PinScope,
    object: v8::Local<v8::Object>,
    name: &str,
    value: v8::Local<v8::Value>,
) -> bool {
    let name = key(scope, name);
    object.set(scope, name.into(), value) == Some(true)
}
fn explicit_string(scope: &mut v8::PinScope, value: v8::Local<v8::Value>) -> Option<String> {
    let global = scope.get_current_context().global(scope);
    let function = get(scope, global, "String")?;
    let function = v8::Local::<v8::Function>::try_from(function).ok()?;
    let receiver = v8::undefined(scope).into();
    let value = function.call(scope, receiver, &[value])?;
    Some(value.to_string(scope)?.to_rust_string_lossy(scope))
}
fn json<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    value: v8::Local<'s, v8::Value>,
    pretty: bool,
) -> Option<v8::Local<'s, v8::Value>> {
    let global = scope.get_current_context().global(scope);
    let object = get(scope, global, "JSON")?.to_object(scope)?;
    let function = get(scope, object, "stringify")?;
    let function = v8::Local::<v8::Function>::try_from(function).ok()?;
    let null = v8::null(scope).into();
    let gap = v8::Integer::new(scope, 2).into();
    if pretty {
        function.call(scope, object.into(), &[value, null, gap])
    } else {
        function.call(scope, object.into(), &[value])
    }
}
fn implicit_string(scope: &mut v8::PinScope, value: v8::Local<v8::Value>) -> Option<String> {
    Some(value.to_string(scope)?.to_rust_string_lossy(scope))
}
fn console_argument<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    value: v8::Local<'s, v8::Value>,
) -> Option<String> {
    if value.is_undefined() {
        return Some("undefined".into());
    }
    if value.is_null() {
        return Some("null".into());
    }
    if value.is_object() && !value.is_function() {
        v8::tc_scope!(let catcher,scope);
        if let Some(value) = json(catcher, value, false) {
            let result = if value.is_undefined() {
                Some(String::new())
            } else {
                implicit_string(catcher, value)
            };
            if result.is_none() {
                catcher.rethrow();
            }
            return result;
        }
        catcher.reset();
        let result = explicit_string(catcher, value);
        if result.is_none() {
            catcher.rethrow();
        }
        return result;
    }
    explicit_string(scope, value)
}
fn console_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    _: v8::ReturnValue,
) {
    let shared = state(scope);
    let level = match args.data().int32_value(scope).unwrap_or(0) {
        1 => ConsoleLevel::Warn,
        2 => ConsoleLevel::Error,
        _ => ConsoleLevel::Log,
    };
    v8::tc_scope!(let catcher,scope);
    let mut parts = Vec::new();
    for index in 0..args.length() {
        let Some(part) = console_argument(catcher, args.get(index)) else {
            catcher.rethrow();
            return;
        };
        parts.push(part);
    }
    let message = parts.join(" ");
    let line = match level {
        ConsoleLevel::Log => message.clone(),
        ConsoleLevel::Warn => format!("[warn] {message}"),
        ConsoleLevel::Error => format!("[error] {message}"),
    };
    let host = {
        let mut shared = shared.borrow_mut();
        if let Some(capture) = shared.capture.as_mut().filter(|capture| capture.capturing) {
            capture.logs.push(line);
        }
        shared.host.clone()
    };
    // Call the Rust observer after releasing the state borrow: it may inspect
    // application state without a RefCell borrow crossing the callback.
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        host.console(level, &message)
    }))
    .is_err()
    {
        let message = key(catcher, "Rust host console callback panicked");
        let error = v8::Exception::error(catcher, message);
        catcher.throw_exception(error);
        catcher.rethrow();
    }
}
fn error_body(
    scope: &mut v8::PinScope,
    error: v8::Local<v8::Value>,
    mut logs: Vec<String>,
) -> String {
    if error.is_null() {
        return "TypeError: Cannot read properties of null (reading 'message')".into();
    }
    if error.is_undefined() {
        return "TypeError: Cannot read properties of undefined (reading 'message')".into();
    }
    v8::tc_scope!(let catcher,scope);
    let result = (|| {
        let object = error.to_object(catcher)?;
        let message = get(catcher, object, "message")?;
        let selected = if message.boolean_value(catcher) {
            message
        } else {
            error
        };
        implicit_string(catcher, selected)
    })();
    if let Some(message) = result {
        logs.push(format!("Error: {message}"));
        return logs.join("\n");
    }
    let exception = catcher.exception();
    catcher.reset();
    exception
        .and_then(|value| implicit_string(catcher, value))
        .unwrap_or_else(|| "Error".into())
}
fn restore(scope: &mut v8::PinScope, shared: &State) {
    let restore = {
        let mut state = shared.borrow_mut();
        let Some(capture) = state.capture.as_mut() else {
            return;
        };
        capture.capturing = false;
        (capture.console.clone(), capture.originals.clone())
    };
    let object = v8::Local::new(scope, restore.0);
    for (name, original) in ["log", "warn", "error"].into_iter().zip(restore.1) {
        let original = v8::Local::new(scope, original);
        set(scope, object, name, original);
    }
}
fn settled<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    _: v8::ReturnValue,
) {
    let shared = state(scope);
    let rejection = args.data().boolean_value(scope);
    let logs = shared.borrow().capture.as_ref().unwrap().logs.clone();
    let value = args.get(0);
    let body = if rejection {
        error_body(scope, value, logs)
    } else {
        v8::tc_scope!(let catcher,scope);
        let result = if value.is_undefined() {
            Some(None)
        } else if value.is_null() {
            Some(Some("=> null".into()))
        } else if value.is_object() && !value.is_function() {
            json(catcher, value, true)
                .and_then(|value| implicit_string(catcher, value))
                .map(|s| Some(format!("=> {s}")))
        } else {
            explicit_string(catcher, value).map(|s| Some(format!("=> {s}")))
        };
        match result {
            Some(result) => {
                let mut lines = logs;
                if let Some(line) = result {
                    lines.push(line)
                }
                if lines.is_empty() {
                    "(no output)".into()
                } else {
                    lines.join("\n")
                }
            }
            None => {
                let exception = catcher.exception().unwrap();
                catcher.reset();
                let logs = shared.borrow().capture.as_ref().unwrap().logs.clone();
                error_body(catcher, exception, logs)
            }
        }
    };
    restore(scope, &shared);
    shared.borrow_mut().capture.as_mut().unwrap().body = Some(body);
}
fn queue_microtask(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _: v8::ReturnValue,
) {
    match v8::Local::<v8::Function>::try_from(args.get(0)) {
        Ok(callback) => scope.enqueue_microtask(callback),
        Err(_) => {
            let message = key(scope, "The provided callback is not a function");
            let error = v8::Exception::type_error(scope, message);
            scope.throw_exception(error);
        }
    }
}
fn set_timeout(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut output: v8::ReturnValue,
) {
    let Ok(callback) = v8::Local::<v8::Function>::try_from(args.get(0)) else {
        let message = key(scope, "Only function timer callbacks are implemented");
        let error = v8::Exception::type_error(scope, message);
        scope.throw_exception(error);
        return;
    };
    let delay = args.get(1).number_value(scope).unwrap_or(0.0);
    let delay = if delay.is_finite() && delay > 0.0 {
        delay.min(i32::MAX as f64).trunc() as u64
    } else {
        0
    };
    let callback = v8::Global::new(scope, callback);
    let arguments = (2..args.length())
        .map(|index| v8::Global::new(scope, args.get(index)))
        .collect();
    let shared = state(scope);
    let id = {
        let mut shared = shared.borrow_mut();
        shared.next_timer = shared.next_timer.wrapping_add(1).max(1);
        let id = shared.next_timer;
        shared.timers.push(Timer {
            id,
            deadline: Instant::now() + Duration::from_millis(delay),
            callback,
            arguments,
        });
        id
    };
    output.set(v8::Number::new(scope, id as f64).into());
}
fn clear_timeout(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _: v8::ReturnValue,
) {
    let id = args.get(0).uint32_value(scope).unwrap_or(0);
    state(scope)
        .borrow_mut()
        .timers
        .retain(|timer| timer.id != id);
}

impl Runtime {
    /// Release an opaque JS root on unload. IDs are never reused; stale or
    /// cross-context references cannot alias a later object.
    pub fn release_reference(&mut self, id: JsValueId) -> bool {
        let mut state = self.state.borrow_mut();
        if state.context_id != id.context {
            return false;
        }
        state
            .references
            .get_mut(id.index as usize)
            .is_some_and(|root| root.take().is_some())
    }
    /// Install a concrete Rust-dispatched function. SDK wrapper construction
    /// will use the same callback without exposing a placeholder app object.
    pub fn bind_function(&mut self, name: &str, binding: HostBinding) -> Result<(), RuntimeError> {
        let executor_handle = self.executor.handle().clone();
        let _entered = executor_handle.enter();
        let index = {
            let mut state = self.state.borrow_mut();
            let index = u32::try_from(state.bindings.len())
                .map_err(|_| RuntimeError::Initialization("Too many host bindings".into()))?;
            state.bindings.push(binding);
            index
        };
        deno_core::scope!(scope, &mut self.js);
        let data = v8::Integer::new_from_unsigned(scope, index).into();
        let function = v8::Function::builder(host_v8::callback)
            .data(data)
            .build(scope)
            .unwrap();
        let global = scope.get_current_context().global(scope);
        if !set(scope, global, name, function.into()) {
            return Err(RuntimeError::Initialization(
                "Unable to install host binding".into(),
            ));
        }
        Ok(())
    }
    pub fn new() -> Result<Self, RuntimeError> {
        Self::with_host(Rc::new(NoHost))
    }
    pub fn with_host(host: Rc<dyn HostCallbacks>) -> Result<Self, RuntimeError> {
        let executor = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .map_err(|error| RuntimeError::Initialization(error.to_string()))?;
        let executor_handle = executor.handle().clone();
        let _entered = executor_handle.enter();
        let mut js = JsRuntime::new(RuntimeOptions::default());
        static NEXT_CONTEXT: AtomicU64 = AtomicU64::new(1);
        let state = Rc::new(RefCell::new(Shared {
            capture: None,
            timers: Vec::new(),
            next_timer: 0,
            host,
            bindings: Vec::new(),
            context_id: NEXT_CONTEXT.fetch_add(1, Ordering::Relaxed),
            references: Vec::new(),
            host_promises: Vec::new(),
        }));
        js.v8_isolate().set_slot(state.clone());
        let (intrinsic_eval, intrinsic_promise, intrinsic_resolve) = {
            deno_core::scope!(scope, &mut js);
            let global = scope.get_current_context().global(scope);
            let eval =
                v8::Local::<v8::Function>::try_from(get(scope, global, "eval").unwrap()).unwrap();
            let promise =
                v8::Local::<v8::Function>::try_from(get(scope, global, "Promise").unwrap())
                    .unwrap();
            let resolve =
                v8::Local::<v8::Function>::try_from(get(scope, promise.into(), "resolve").unwrap())
                    .unwrap();
            let console = v8::Object::new(scope);
            for (level, name) in ["log", "warn", "error"].into_iter().enumerate() {
                let data = v8::Integer::new(scope, level as i32).into();
                let function = v8::Function::builder(console_callback)
                    .data(data)
                    .build(scope)
                    .unwrap();
                set(scope, console, name, function.into());
            }
            set(scope, global, "console", console.into());
            let function = v8::Function::new(scope, queue_microtask).unwrap();
            set(scope, global, "queueMicrotask", function.into());
            let function = v8::Function::new(scope, set_timeout).unwrap();
            set(scope, global, "setTimeout", function.into());
            let function = v8::Function::new(scope, clear_timeout).unwrap();
            set(scope, global, "clearTimeout", function.into());
            (
                v8::Global::new(scope, eval),
                v8::Global::new(scope, promise),
                v8::Global::new(scope, resolve),
            )
        };
        // Runtime bootstrap ops remain internal; do not present Deno APIs as
        // native host globals. No Node/SDK/DOM placeholder is installed.
        js.execute_script("mnemosyne:bootstrap", "delete globalThis.Deno;")
            .map_err(|error| RuntimeError::Initialization(error.to_string()))?;
        Ok(Self {
            intrinsic_eval,
            intrinsic_promise,
            intrinsic_resolve,
            state,
            js,
            executor,
        })
    }
    /// The native eval-command body. Argument validation remains distinguishable
    /// from executing JavaScript; a reusable context survives successive calls.
    pub fn eval_command(&mut self, code: Option<&str>) -> Result<EvalOutcome, RuntimeError> {
        let Some(code) = code else {
            return Ok(EvalOutcome{body:"Error: Missing required parameter: code=<javascript>\nUsage: eval code=<javascript>".into()});
        };
        if code.is_empty() {
            return Ok(EvalOutcome {
                body: "Error: Missing required parameter: code\nUsage: eval code=<javascript>"
                    .into(),
            });
        }
        self.eval(code)
    }
    pub fn eval(&mut self, code: &str) -> Result<EvalOutcome, RuntimeError> {
        let executor_handle = self.executor.handle().clone();
        let _entered = executor_handle.enter();
        // Deliver already due tasks before starting the next command capture.
        self.fire_timers()?;
        {
            deno_core::scope!(scope, &mut self.js);
            scope.perform_microtask_checkpoint();
        }
        {
            deno_core::scope!(scope, &mut self.js);
            let global = scope.get_current_context().global(scope);
            let console = get(scope, global, "console")
                .and_then(|value| value.to_object(scope))
                .ok_or_else(|| RuntimeError::EventLoop("console object unavailable".into()))?;
            let originals = ["log", "warn", "error"]
                .into_iter()
                .map(|name| {
                    let value = get(scope, console, name).unwrap();
                    v8::Global::new(scope, value)
                })
                .collect();
            self.state.borrow_mut().capture = Some(Capture {
                logs: Vec::new(),
                body: None,
                capturing: true,
                console: v8::Global::new(scope, console),
                originals,
            });
            for (level, name) in ["log", "warn", "error"].into_iter().enumerate() {
                let data = v8::Integer::new(scope, level as i32).into();
                let function = v8::Function::builder(console_callback)
                    .data(data)
                    .build(scope)
                    .unwrap();
                set(scope, console, name, function.into());
            }
            v8::tc_scope!(let catcher,scope);
            let eval = v8::Local::new(catcher, &self.intrinsic_eval);
            let receiver = v8::undefined(catcher).into();
            let code = key(catcher, code).into();
            let result = eval.call(catcher, receiver, &[code]);
            let promise = result.and_then(|result| {
                let constructor = v8::Local::new(catcher, &self.intrinsic_promise);
                let resolve = v8::Local::new(catcher, &self.intrinsic_resolve);
                resolve.call(catcher, constructor.into(), &[result])
            });
            if let Some(promise) = promise {
                let promise = v8::Local::<v8::Promise>::try_from(promise).unwrap();
                let no = v8::Boolean::new(catcher, false).into();
                let yes = v8::Boolean::new(catcher, true).into();
                let fulfilled = v8::Function::builder(settled)
                    .data(no)
                    .build(catcher)
                    .unwrap();
                let rejected = v8::Function::builder(settled)
                    .data(yes)
                    .build(catcher)
                    .unwrap();
                promise.then2(catcher, fulfilled, rejected);
            } else {
                let exception = catcher.exception().unwrap();
                catcher.reset();
                let logs = self.state.borrow().capture.as_ref().unwrap().logs.clone();
                let body = error_body(catcher, exception, logs);
                restore(catcher, &self.state);
                self.state.borrow_mut().capture.as_mut().unwrap().body = Some(body);
            }
        }
        // Await continuations run before timer tasks, including zero-delay
        // timers scheduled by the evaluated source.
        {
            deno_core::scope!(scope, &mut self.js);
            scope.perform_microtask_checkpoint();
        }
        let mut sleeper: Option<Pin<Box<tokio::time::Sleep>>> = None;
        loop {
            let body = self
                .state
                .borrow_mut()
                .capture
                .as_mut()
                .unwrap()
                .body
                .take();
            if let Some(body) = body {
                self.state.borrow_mut().capture = None;
                return Ok(EvalOutcome { body });
            }
            self.fire_timers()?;
            let state = self.state.clone();
            let js = &mut self.js;
            self.executor.block_on(poll_fn(|cx| {
                host_v8::poll(js, &state, cx);
                match js.poll_event_loop(cx, PollEventLoopOptions::default()) {
                    Poll::Ready(Err(error)) => {
                        return Poll::Ready(Err(RuntimeError::EventLoop(error.to_string())));
                    }
                    _ => {}
                }
                if state.borrow().capture.as_ref().unwrap().body.is_some() {
                    return Poll::Ready(Ok(()));
                }
                let deadline = state
                    .borrow()
                    .timers
                    .iter()
                    .map(|timer| timer.deadline)
                    .min();
                if let Some(deadline) = deadline {
                    if deadline <= Instant::now() {
                        return Poll::Ready(Ok(()));
                    }
                    if sleeper.as_ref().is_none_or(|sleep| {
                        sleep.deadline() != tokio::time::Instant::from_std(deadline)
                    }) {
                        sleeper = Some(Box::pin(tokio::time::sleep_until(
                            tokio::time::Instant::from_std(deadline),
                        )));
                    }
                    if sleeper.as_mut().unwrap().as_mut().poll(cx).is_ready() {
                        return Poll::Ready(Ok(()));
                    }
                }
                Poll::Pending
            }))?;
        }
    }
    fn fire_timers(&mut self) -> Result<(), RuntimeError> {
        loop {
            let timer = {
                let mut shared = self.state.borrow_mut();
                let index = shared
                    .timers
                    .iter()
                    .enumerate()
                    .filter(|(_, timer)| timer.deadline <= Instant::now())
                    .min_by_key(|(_, timer)| timer.deadline)
                    .map(|(index, _)| index);
                index.map(|index| shared.timers.remove(index))
            };
            let Some(timer) = timer else { return Ok(()) };
            deno_core::scope!(scope, &mut self.js);
            let callback = v8::Local::new(scope, timer.callback);
            let arguments: Vec<_> = timer
                .arguments
                .into_iter()
                .map(|argument| v8::Local::new(scope, argument))
                .collect();
            let receiver = scope.get_current_context().global(scope).into();
            v8::tc_scope!(let catcher,scope);
            callback.call(catcher, receiver, &arguments);
            if catcher.has_caught() {
                catcher.reset();
            }
        }
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        let mut state = self.state.borrow_mut();
        state.capture = None;
        state.timers.clear();
        state.host_promises.clear();
        state.references.clear();
    }
}
