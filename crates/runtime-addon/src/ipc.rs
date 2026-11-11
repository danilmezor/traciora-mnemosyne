//! Rust-only transport reader and Node-API thread-safe dispatch. Background
//! threads carry owned protocol bytes, never Env, napi_value or napi_ref.
use crate::evaluate as js;
use mnemosyne_runtime_protocol::{
    ChildMessage, HostCall, HostError, HostResult, PROTOCOL_VERSION, ParentMessage, read_frame,
    write_frame,
};
use napi::{
    Env, JsValue,
    bindgen_prelude::{Buffer, Utf16String},
    sys,
    threadsafe_function::ThreadsafeFunctionCallMode,
};
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, HashMap},
    os::unix::net::UnixStream,
    ptr,
    rc::Rc,
    sync::{Arc, Mutex, mpsc},
};

struct Context {
    writer: Arc<Mutex<UnixStream>>,
    context: Cell<u64>,
    current_eval: Cell<Option<u64>>,
    next_call: Cell<u64>,
    sync_wait: Arc<Mutex<Vec<Waiter>>>,
}
impl Drop for Context {
    fn drop(&mut self) {
        if let Ok(writer) = self.writer.lock() {
            let _ = writer.shutdown(std::net::Shutdown::Both);
        }
    }
}
unsafe extern "C" fn cleanup(data: *mut std::ffi::c_void) {
    CONTEXTS.with(|contexts| {
        contexts.borrow_mut().remove(&(data as usize));
    });
}
thread_local! {static CONTEXTS:RefCell<HashMap<usize,Rc<Context>>>=RefCell::new(HashMap::new());}
fn context(env: sys::napi_env) -> js::JsResult<Rc<Context>> {
    CONTEXTS
        .with(|contexts| contexts.borrow().get(&(env as usize)).cloned())
        .ok_or(())
}
fn send(env: sys::napi_env, message: ChildMessage) -> js::JsResult<()> {
    let ctx = context(env)?;
    write_frame(&mut *ctx.writer.lock().map_err(|_| ())?, &message).map_err(|_| ())
}
pub(crate) fn identity(env: sys::napi_env) -> js::JsResult<u64> {
    Ok(context(env)?.context.get())
}
pub(crate) fn next_call(env: sys::napi_env) -> js::JsResult<u64> {
    let context = context(env)?;
    let id = context.next_call.get();
    context.next_call.set(id + 1);
    Ok(id)
}
pub(crate) fn host_call(env: sys::napi_env, id: u64, call: HostCall) -> js::JsResult<()> {
    send(
        env,
        ChildMessage::HostCall {
            eval_id: context(env)?.current_eval.get().unwrap_or(0),
            id,
            call,
        },
    )
}
pub(crate) fn console_event_for(
    env: sys::napi_env,
    frame: Option<u64>,
    level: usize,
    message: &str,
) -> js::JsResult<()> {
    if let Ok(context) = context(env) {
        if let Some(id) = frame {
            send(
                env,
                ChildMessage::Console {
                    id,
                    level: match level {
                        1 => mnemosyne_runtime_protocol::ConsoleLevel::Warn,
                        2 => mnemosyne_runtime_protocol::ConsoleLevel::Error,
                        _ => mnemosyne_runtime_protocol::ConsoleLevel::Log,
                    },
                    message: message.encode_utf16().collect(),
                },
            )?;
        }
    }
    Ok(())
}
pub(crate) fn active_eval(env: sys::napi_env) -> js::JsResult<Option<u64>> {
    Ok(context(env)?.current_eval.get())
}
pub(crate) fn with_eval<T>(
    env: sys::napi_env,
    id: Option<u64>,
    work: impl FnOnce() -> js::JsResult<T>,
) -> js::JsResult<T> {
    let context = context(env)?;
    let previous = context.current_eval.replace(id);
    struct Reset(Rc<Context>, Option<u64>);
    impl Drop for Reset {
        fn drop(&mut self) {
            self.0.current_eval.set(self.1);
        }
    }
    let _reset = Reset(context, previous);
    work()
}
pub(crate) fn handler_started(
    env: sys::napi_env,
    frame: Option<u64>,
    capture: usize,
) -> js::JsResult<()> {
    if let Some(id) = frame {
        send(
            env,
            ChildMessage::HandlerStarted {
                id,
                capture: capture as u64,
            },
        )?;
    }
    Ok(())
}
pub(crate) fn handler_finished(
    env: sys::napi_env,
    frame: Option<u64>,
    capture: usize,
) -> js::JsResult<()> {
    if let Some(id) = frame {
        send(
            env,
            ChildMessage::HandlerFinished {
                id,
                capture: capture as u64,
            },
        )?;
    }
    Ok(())
}
#[derive(Clone, Copy, PartialEq)]
enum WaitKind {
    Reply,
    Prefix,
    Poll,
}
#[derive(Clone)]
struct Waiter {
    id: u64,
    kind: WaitKind,
    sender: mpsc::Sender<ParentMessage>,
}
fn is_terminal(kind: WaitKind, id: u64, message: &ParentMessage) -> bool {
    match message {
        ParentMessage::HostReply { id: reply, .. } => kind == WaitKind::Reply && *reply == id,
        ParentMessage::PrefixReady { id: reply } => kind == WaitKind::Prefix && *reply == id,
        ParentMessage::PollFinished { id: reply } => kind == WaitKind::Poll && *reply == id,
        _ => false,
    }
}
fn wait_for(
    env: sys::napi_env,
    id: u64,
    kind: WaitKind,
    call: Option<HostCall>,
) -> js::JsResult<Option<HostResult>> {
    let context = context(env)?;
    let (sender, receiver) = mpsc::channel();
    context
        .sync_wait
        .lock()
        .map_err(|_| ())?
        .push(Waiter { id, kind, sender });
    struct Reset {
        wait: Arc<Mutex<Vec<Waiter>>>,
        id: u64,
        kind: WaitKind,
    }
    impl Drop for Reset {
        fn drop(&mut self) {
            if let Ok(mut wait) = self.wait.lock() {
                if let Some(index) = wait
                    .iter()
                    .position(|entry| entry.id == self.id && entry.kind == self.kind)
                {
                    wait.remove(index);
                }
            }
        }
    }
    let _reset = Reset {
        wait: context.sync_wait.clone(),
        id,
        kind,
    };
    if let Some(call) = call {
        host_call(env, id, call)?;
    } else {
        send(env, ChildMessage::HostPoll { id })?;
    }
    loop {
        let message = receiver.recv().map_err(|_| ())?;
        if is_terminal(kind, id, &message) {
            return Ok(match message {
                ParentMessage::HostReply { result, .. } => Some(result),
                _ => None,
            });
        }
        process(env, message)?;
    }
}
pub(crate) fn sync_call(env: sys::napi_env, id: u64, call: HostCall) -> js::JsResult<HostResult> {
    wait_for(env, id, WaitKind::Reply, Some(call))?.ok_or(())
}
pub(crate) fn prefix_call(env: sys::napi_env, id: u64, call: HostCall) -> js::JsResult<()> {
    wait_for(env, id, WaitKind::Prefix, Some(call))?;
    Ok(())
}

fn failure(env: sys::napi_env, id: Option<u64>, message: impl Into<String>) {
    let _ = send(
        env,
        ChildMessage::Failed {
            id,
            error: HostError::unsupported(message),
        },
    );
}
fn versions(env: sys::napi_env) -> js::JsResult<BTreeMap<String, String>> {
    let process = js::get(env, js::global(env)?, "process")?;
    let versions = js::get(env, process, "versions")?;
    let mut keys = ptr::null_mut();
    js::check(unsafe { sys::napi_get_property_names(env, versions, &mut keys) })?;
    let mut length = 0;
    js::check(unsafe { sys::napi_get_array_length(env, keys, &mut length) })?;
    let mut result = BTreeMap::new();
    for index in 0..length {
        let mut key = ptr::null_mut();
        js::check(unsafe { sys::napi_get_element(env, keys, index, &mut key) })?;
        let name = js::read_text(env, key)?;
        let value = js::read_text(env, js::get(env, versions, &name)?)?;
        result.insert(name, value);
    }
    Ok(result)
}
unsafe extern "C" fn result_callback(
    env: sys::napi_env,
    info: sys::napi_callback_info,
) -> sys::napi_value {
    js::guarded(env, || {
        let (args, id) = js::callback_args(env, info)?;
        let body = js::read_text(
            env,
            args.first().copied().unwrap_or_else(|| js::undefined(env)),
        )?;
        send(
            env,
            ChildMessage::Evaluated {
                id: id as u64,
                body: body.encode_utf16().collect(),
            },
        )?;
        Ok(js::undefined(env))
    })
}
unsafe extern "C" fn error_callback(
    env: sys::napi_env,
    info: sys::napi_callback_info,
) -> sys::napi_value {
    js::guarded(env, || {
        let (args, id) = js::callback_args(env, info)?;
        let error = args.first().copied().unwrap_or_else(|| js::undefined(env));
        let body = if js::kind(env, error)? == sys::ValueType::napi_string {
            format!("Error: {}", js::read_text(env, error)?)
        } else {
            js::implicit(env, error)?
        };
        send(
            env,
            ChildMessage::Evaluated {
                id: id as u64,
                body: body.encode_utf16().collect(),
            },
        )?;
        Ok(js::undefined(env))
    })
}
unsafe extern "C" fn dispatch(
    env: sys::napi_env,
    info: sys::napi_callback_info,
) -> sys::napi_value {
    js::guarded(env, || {
        let (args, _) = js::callback_args(env, info)?;
        let mut data = ptr::null_mut();
        let mut length = 0;
        js::check(unsafe { sys::napi_get_buffer_info(env, args[0], &mut data, &mut length) })?;
        // The Buffer is owned by this callback scope; deserialize before it can
        // be released. The background reader never sees its N-API handle.
        let bytes = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), length) };
        let message: ParentMessage = postcard::from_bytes(bytes).map_err(|_| ())?;
        process(env, message)?;
        Ok(js::undefined(env))
    })
}
fn process(env: sys::napi_env, message: ParentMessage) -> js::JsResult<()> {
    match message {
        ParentMessage::Initialize {
            version,
            context: identity,
        } => {
            if version != PROTOCOL_VERSION {
                failure(env, None, "Runtime protocol version mismatch");
            } else {
                context(env)?.context.set(identity);
                let versions = versions(env)?;
                send(
                    env,
                    ChildMessage::Ready {
                        version,
                        context: identity,
                        pid: std::process::id(),
                        versions,
                    },
                )?;
            }
        }
        ParentMessage::Evaluate { id, code } => {
            with_eval(env, Some(id), || {
                let env_wrapper = Env::from_raw(env);
                match js::evaluate_cli(&env_wrapper, code.map(Utf16String::from)) {
                    Ok(promise) => {
                        let fulfilled =
                            js::function(env, "rpcResult", Some(result_callback), id as usize)?;
                        let rejected =
                            js::function(env, "rpcError", Some(error_callback), id as usize)?;
                        js::react(env, promise.raw(), fulfilled, rejected)?;
                    }
                    Err(_) => {
                        let thrown = js::exception(env)?;
                        let body = if js::kind(env, thrown)? == sys::ValueType::napi_string {
                            format!("Error: {}", js::read_text(env, thrown)?)
                        } else {
                            js::implicit(env, thrown)?
                        };
                        send(
                            env,
                            ChildMessage::Evaluated {
                                id,
                                body: body.encode_utf16().collect(),
                            },
                        )?;
                    }
                }
                Ok(())
            })?;
        }
        ParentMessage::RequestPoll { id } => {
            wait_for(env, id, WaitKind::Poll, None)?;
        }
        ParentMessage::PrefixReady { .. } | ParentMessage::PollFinished { .. } => {
            return Err(());
        }
        ParentMessage::Shutdown => {
            let process = js::get(env, js::global(env)?, "process")?;
            let exit = js::get(env, process, "exit")?;
            let mut zero = ptr::null_mut();
            js::check(unsafe { sys::napi_create_int32(env, 0, &mut zero) })?;
            js::call(env, exit, process, &[zero])?;
        }
        ParentMessage::Bind { id, name, binding } => {
            let result = crate::host::bind(env, name, binding)
                .map_err(|_| HostError::unsupported("Cannot install Rust callback"));
            send(env, ChildMessage::Acknowledged { id, result })?;
        }
        ParentMessage::Inspect { id, query } => {
            let result =
                crate::host::inspect(env, query).map_err(|_| crate::host::inspection_error(env));
            send(env, ChildMessage::Inspected { id, result })?;
        }
        ParentMessage::Release { id, reference } => {
            let result = crate::host::release(env, reference)
                .map_err(|_| HostError::unsupported("Foreign or released JavaScript reference"));
            send(env, ChildMessage::Acknowledged { id, result })?;
        }
        ParentMessage::RegisterClass { id, class } => {
            let result = crate::registry::register_class(env, class)
                .map_err(|_| crate::host::inspection_error(env));
            send(env, ChildMessage::Acknowledged { id, result })?;
        }
        ParentMessage::RetainReference { id, reference } => {
            let result = crate::host::retain(env, reference).map_err(|_| {
                HostError::unsupported("Cannot retain foreign or released JavaScript reference")
            });
            send(env, ChildMessage::Acknowledged { id, result })?;
        }
        ParentMessage::ReleaseLease { id, reference } => {
            let result = crate::host::release_lease(env, reference).map_err(|_| {
                HostError::unsupported("Cannot release foreign or missing JavaScript root lease")
            });
            send(env, ChildMessage::Acknowledged { id, result })?;
        }
        ParentMessage::RegisterInstance {
            id,
            object,
            class,
            reference,
            properties,
        } => {
            let result =
                crate::registry::register_instance(env, object, class, reference, properties)
                    .map_err(|_| crate::host::inspection_error(env));
            send(env, ChildMessage::Acknowledged { id, result })?;
        }
        ParentMessage::UpdateInstance {
            id,
            object,
            properties,
        } => {
            let result = crate::registry::update_instance(env, object, properties)
                .map_err(|_| crate::host::inspection_error(env));
            send(env, ChildMessage::Acknowledged { id, result })?;
        }
        ParentMessage::HostReply { id, result } => {
            if crate::host::settle(env, id, result).is_err() {
                failure(env, Some(id), "No matching addon host request");
            }
        }
    }
    Ok(())
}
pub fn start(
    env: &Env,
    socket: &str,
    require: js::Value,
    module: js::Value,
    filename: &[u16],
    dirname: &[u16],
    factory: js::Value,
    property_read: js::Value,
    property_write: js::Value,
    binary: js::Value,
    instance_of: js::Value,
    install_commonjs: bool,
    property_delete: Option<js::Value>,
    for_in: Option<js::Value>,
) -> napi::Result<()> {
    let raw = env.raw();
    if CONTEXTS.with(|contexts| contexts.borrow().contains_key(&(raw as usize))) {
        return Err(napi::Error::from_reason("Addon transport already started"));
    }
    // These are the actual CommonJS bootstrap bindings supplied by Node,
    // rather than handwritten replacements for Node's module system.
    if install_commonjs {
        (|| -> js::JsResult<()> {
            let global = js::global(raw)?;
            js::set(raw, global, "require", require)?;
            js::set(raw, global, "module", module)?;
            js::set(raw, global, "exports", js::get(raw, module, "exports")?)?;
            js::set(raw, global, "__filename", js::text(raw, filename)?)?;
            js::set(raw, global, "__dirname", js::text(raw, dirname)?)
        })()
        .map_err(|_| napi::Error::from_reason("Cannot install actual CommonJS bindings"))?;
    }
    crate::host::install_factory(
        raw,
        factory,
        property_read,
        property_write,
        binary,
        instance_of,
        property_delete,
        for_in,
    )
    .map_err(|_| napi::Error::from_reason("Cannot install strict callback forwarding"))?;
    js::initialize(raw)
        .map_err(|_| napi::Error::from_reason("Cannot capture transport intrinsics"))?;
    let stream =
        UnixStream::connect(socket).map_err(|error| napi::Error::from_reason(error.to_string()))?;
    let reader = stream
        .try_clone()
        .map_err(|error| napi::Error::from_reason(error.to_string()))?;
    let sync_wait = Arc::new(Mutex::new(Vec::<Waiter>::new()));
    CONTEXTS.with(|contexts| {
        contexts.borrow_mut().insert(
            raw as usize,
            Rc::new(Context {
                writer: Arc::new(Mutex::new(stream)),
                context: Cell::new(0),
                current_eval: Cell::new(None),
                next_call: Cell::new(1),
                sync_wait: sync_wait.clone(),
            }),
        )
    });
    js::check(unsafe { sys::napi_add_env_cleanup_hook(raw, Some(cleanup), raw.cast()) })
        .map_err(|_| napi::Error::from_reason("Cannot install addon transport cleanup"))?;
    let function = env.create_function::<Buffer, ()>("rustDispatch", dispatch)?;
    let threadsafe = function
        .build_threadsafe_function::<Vec<u8>>()
        .callee_handled::<false>()
        .build_callback(|context| Ok(Buffer::from(context.value)))?;
    std::thread::Builder::new()
        .name("mnemosyne-node-reader".into())
        .spawn(move || {
            let mut reader = reader;
            loop {
                let message = match read_frame::<ParentMessage>(&mut reader) {
                    Ok(message) => message,
                    Err(_) => ParentMessage::Shutdown,
                };
                let shutdown = matches!(message, ParentMessage::Shutdown);
                // A poll may run only as a new JS task, never while servicing a
                // blocked source-synchronous prefix/getter. Terminal routing
                // removes its waiter before delivery, so later control frames
                // cannot become stranded on a receiver being unwound.
                let sync = if matches!(message, ParentMessage::RequestPoll { .. }) {
                    None
                } else {
                    sync_wait.lock().ok().and_then(|mut wait| {
                        let entry = wait.last().cloned()?;
                        if is_terminal(entry.kind, entry.id, &message) {
                            wait.pop();
                        }
                        Some(entry.sender)
                    })
                };
                let pending = if let Some(wait) = sync {
                    match wait.send(message) {
                        Ok(()) => None,
                        Err(error) => Some(error.0),
                    }
                } else {
                    Some(message)
                };
                if let Some(message) = pending {
                    if let Ok(bytes) = postcard::to_stdvec(&message) {
                        threadsafe.call(bytes, ThreadsafeFunctionCallMode::NonBlocking);
                    }
                }
                if shutdown {
                    break;
                }
            }
        })
        .map_err(|error| napi::Error::from_reason(error.to_string()))?;
    Ok(())
}
