//! Thread-safe ownership boundary. V8 handles never leave the worker.
use crate::{
    ConsoleLevel, EvalOutcome, HostBinding, HostCallbacks, HostError, HostRequest, JsValueId,
    Runtime, RuntimeError, initialize_platform,
};
use std::{
    cell::RefCell,
    rc::Rc,
    sync::{Arc, mpsc},
    thread::{self, JoinHandle},
};

enum Reply {
    Console(ConsoleLevel, String),
    Host(HostRequest),
    Complete(Result<EvalOutcome, RuntimeError>),
}
enum Command {
    Release {
        id: JsValueId,
        reply: mpsc::Sender<bool>,
    },
    Eval {
        code: Option<String>,
        replies: mpsc::Sender<Reply>,
    },
    Bind {
        name: String,
        binding: HostBinding,
        reply: mpsc::Sender<Result<(), RuntimeError>>,
    },
}
#[derive(Default)]
struct ConsoleBridge(RefCell<Option<mpsc::Sender<Reply>>>);
impl HostCallbacks for ConsoleBridge {
    fn console(&self, level: ConsoleLevel, message: &str) {
        if let Some(replies) = self.0.borrow().as_ref() {
            let _ = replies.send(Reply::Console(level, message.into()));
        }
    }
    fn call(&self, request: HostRequest) {
        let replies = self.0.borrow().clone();
        if let Some(replies) = replies {
            if let Err(error) = replies.send(Reply::Host(request)) {
                if let Reply::Host(request) = error.0 {
                    request.respond(Err(HostError::unsupported(
                        "Rust backend caller disconnected",
                    )));
                }
            }
        } else {
            request.respond(Err(HostError::unsupported("No active Rust backend caller")));
        }
    }
}
struct Worker {
    sender: Option<mpsc::Sender<Command>>,
    join: Option<JoinHandle<()>>,
}
impl Drop for Worker {
    fn drop(&mut self) {
        // Closing the channel lets an idle worker release the isolate on its
        // owning thread. Terminating non-cooperative JS remains future work.
        self.sender.take();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// A Send + Sync handle to one persistent, thread-confined JavaScript context.
/// Concurrent requests execute serially in channel arrival order. Each has its
/// own reply channel, so console events cannot cross request boundaries.
///
/// Typed host requests are pumped on the Rust backend's owning thread. No
/// Engine reference or V8 value is placed in a channel. SDK wrappers are a
/// separate required layer over this callback protocol.
#[derive(Clone)]
pub struct RuntimeHandle(Arc<Worker>);
impl RuntimeHandle {
    /// The application should first call initialize_platform on its main
    /// thread. This fallback supports callers that create a single worker.
    pub fn spawn() -> Result<Self, RuntimeError> {
        initialize_platform();
        let (sender, commands) = mpsc::channel::<Command>();
        let (started, ready) = mpsc::sync_channel(1);
        let join = thread::Builder::new()
            .name("mnemosyne-js".into())
            .spawn(move || {
                let bridge = Rc::new(ConsoleBridge::default());
                let mut runtime = match Runtime::with_host(bridge.clone()) {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = started.send(Err(error));
                        return;
                    }
                };
                if started.send(Ok(())).is_err() {
                    return;
                }
                for command in commands {
                    match command {
                        Command::Release { id, reply } => {
                            let _ = reply.send(runtime.release_reference(id));
                        }
                        Command::Eval { code, replies } => {
                            *bridge.0.borrow_mut() = Some(replies.clone());
                            let outcome = runtime.eval_command(code.as_deref());
                            bridge.0.borrow_mut().take();
                            let _ = replies.send(Reply::Complete(outcome));
                        }
                        Command::Bind {
                            name,
                            binding,
                            reply,
                        } => {
                            let _ = reply.send(runtime.bind_function(&name, binding));
                        }
                    }
                }
            })
            .map_err(|error| RuntimeError::Worker(error.to_string()))?;
        match ready.recv() {
            Ok(Ok(())) => Ok(Self(Arc::new(Worker {
                sender: Some(sender),
                join: Some(join),
            }))),
            outcome => {
                drop(sender);
                let _ = join.join();
                Err(match outcome {
                    Ok(Err(error)) => error,
                    _ => RuntimeError::Worker("worker stopped during initialization".into()),
                })
            }
        }
    }
    pub fn eval(&self, code: &str) -> Result<EvalOutcome, RuntimeError> {
        self.eval_command_with_console(Some(code), |_, _| {})
    }
    pub fn eval_command(&self, code: Option<&str>) -> Result<EvalOutcome, RuntimeError> {
        self.eval_command_with_console(code, |_, _| {})
    }
    pub fn release_reference(&self, id: JsValueId) -> Result<bool, RuntimeError> {
        let (reply, result) = mpsc::channel();
        self.0
            .sender
            .as_ref()
            .unwrap()
            .send(Command::Release { id, reply })
            .map_err(|_| RuntimeError::Worker("worker command channel closed".into()))?;
        result
            .recv()
            .map_err(|_| RuntimeError::Worker("worker reply channel closed".into()))
    }
    pub fn bind_function(&self, name: &str, binding: HostBinding) -> Result<(), RuntimeError> {
        let (reply, result) = mpsc::channel();
        self.0
            .sender
            .as_ref()
            .unwrap()
            .send(Command::Bind {
                name: name.into(),
                binding,
                reply,
            })
            .map_err(|_| RuntimeError::Worker("worker command channel closed".into()))?;
        result
            .recv()
            .map_err(|_| RuntimeError::Worker("worker reply channel closed".into()))?
    }
    /// Pump observer callbacks on the calling thread while the worker executes
    /// JavaScript. Do not synchronously submit another eval from this callback:
    /// the worker serializes commands and the first request has not completed.
    pub fn eval_command_with_console(
        &self,
        code: Option<&str>,
        console: impl FnMut(ConsoleLevel, &str),
    ) -> Result<EvalOutcome, RuntimeError> {
        self.eval_command_with_host(code, console, |request| {
            request.respond(Err(HostError::unsupported(
                "No Rust backend dispatcher is attached",
            )))
        })
    }
    /// The backend receives concrete operations while JS is running. It can
    /// respond synchronously, retain a Promise completion for later settlement,
    /// or queue a VoidAsync action without changing its JS return type.
    pub fn eval_command_with_host(
        &self,
        code: Option<&str>,
        mut console: impl FnMut(ConsoleLevel, &str),
        mut host: impl FnMut(HostRequest),
    ) -> Result<EvalOutcome, RuntimeError> {
        let (replies, events) = mpsc::channel();
        self.0
            .sender
            .as_ref()
            .unwrap()
            .send(Command::Eval {
                code: code.map(str::to_owned),
                replies,
            })
            .map_err(|_| RuntimeError::Worker("worker command channel closed".into()))?;
        for reply in events {
            match reply {
                Reply::Console(level, message) => console(level, &message),
                Reply::Host(request) => host(request),
                Reply::Complete(outcome) => return outcome,
            }
        }
        Err(RuntimeError::Worker("worker reply channel closed".into()))
    }
}
