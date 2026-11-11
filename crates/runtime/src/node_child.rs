//! Rust owns the child, IPC, identity checks and lifecycle. The only JavaScript
//! bootstrap loads the Rust addon and passes Node's actual CommonJS bindings.
use crate::{
    ChildMessage, ConsoleLevel, EvalOutcome, HostCall, HostError, HostResult, PROTOCOL_VERSION,
    ParentMessage, RuntimeError, read_frame, write_frame,
};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs::{self, File},
    io::Read,
    os::unix::{
        net::{UnixListener, UnixStream},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Debug)]
pub struct NodeIdentity {
    pub executable_sha256: String,
    pub framework_sha256: String,
    pub versions: BTreeMap<String, String>,
}
#[derive(Clone, Debug)]
pub struct NodeChildConfig {
    pub executable: PathBuf,
    pub framework: PathBuf,
    pub addon: PathBuf,
    /// Must be a new, exclusively owned directory. Existing profiles are refused.
    pub profile: PathBuf,
    pub vault: PathBuf,
    pub identity: NodeIdentity,
    pub context: u64,
    pub startup_timeout: Duration,
}
/// The independently pinned, official @electron/remote package. Every file is
/// verified before copying into the new owned application's module directory.
#[derive(Clone, Debug)]
pub struct RendererConfig {
    pub remote_package: PathBuf,
    pub remote_files_sha256: BTreeMap<String, String>,
}
#[derive(Clone, Debug)]
pub struct StopReceipt {
    pub pid: u32,
    pub exit_code: Option<i32>,
    pub reaped: bool,
    pub runtime_pid: u32,
    pub process_group_stopped: bool,
}
#[derive(Clone, Debug)]
pub struct RuntimeConsoleEvent {
    pub evaluation_id: u64,
    pub level: ConsoleLevel,
    pub message: Vec<u16>,
}
pub trait NodeHostDispatcher {
    fn dispatch(&mut self, runtime: &mut NodeChild, request: crate::HostRequest);
    /// Owner-thread continuations awaiting a genuine new JS task boundary.
    fn has_pending_jobs(&self) -> bool {
        false
    }
    /// Runs with no source-synchronous JS prefix/getter stack active.
    fn poll(&mut self, _runtime: &mut NodeChild) {}
}
impl<F: FnMut(&mut NodeChild, crate::HostRequest)> NodeHostDispatcher for F {
    fn dispatch(&mut self, runtime: &mut NodeChild, request: crate::HostRequest) {
        self(runtime, request);
    }
}
pub struct NodeChild {
    child: Child,
    stream: UnixStream,
    _socket_directory: tempfile::TempDir,
    next_id: u64,
    versions: BTreeMap<String, String>,
    profile: PathBuf,
    stopped: Option<StopReceipt>,
    events: Option<mpsc::Receiver<Result<ChildMessage, String>>>,
    reader: Option<thread::JoinHandle<()>>,
    completions: Vec<(u64, crate::HostCompletion)>,
    poll_inflight: Option<u64>,
    handlers: BTreeSet<(u64, u64)>,
    outcomes: BTreeMap<u64, Vec<u16>>,
    late_console: VecDeque<RuntimeConsoleEvent>,
    deferred_events: VecDeque<ChildMessage>,
    renderer: bool,
    runtime_pid: u32,
    lease_owner: Arc<crate::root_lease::LeaseOwner>,
    lease_sender: mpsc::Sender<crate::JsValueId>,
    lease_releases: mpsc::Receiver<crate::JsValueId>,
}
fn error(value: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::Worker(value.to_string())
}
fn hash(path: &Path) -> Result<String, RuntimeError> {
    let mut file = File::open(path).map_err(error)?;
    let mut digest = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let length = file.read(&mut buffer).map_err(error)?;
        if length == 0 {
            break;
        }
        digest.update(&buffer[..length]);
    }
    Ok(format!("{:x}", digest.finalize()))
}
fn guard(config: &NodeChildConfig) -> Result<(), RuntimeError> {
    if hash(&config.executable)? != config.identity.executable_sha256
        || hash(&config.framework)? != config.identity.framework_sha256
    {
        return Err(RuntimeError::Initialization(
            "Upstream Electron artifact digest mismatch; refusing to launch".into(),
        ));
    }
    let bytes = fs::read(&config.framework).map_err(error)?;
    let sentinel = b"dL7pKGdnNz796PbbjQWNKmHXBZaB9tsX";
    let offsets: Vec<_> = memchr::memmem::find_iter(&bytes, sentinel)
        .map(|i| i + sentinel.len())
        .collect();
    if offsets.len() != 1 {
        return Err(RuntimeError::Initialization(
            "Electron fuse sentinel is not unique".into(),
        ));
    }
    let wire = &bytes[offsets[0]..];
    if wire.len() < 3
        || wire[0] != 1
        || wire[1] == 0
        || wire[2] != b'1'
        || wire.len() < 2 + usize::from(wire[1])
    {
        return Err(RuntimeError::Initialization(
            "Electron RunAsNode fuse is not proved enabled; refusing to launch".into(),
        ));
    }
    Ok(())
}
fn js_path(path: &Path) -> Result<String, RuntimeError> {
    let text = path
        .to_str()
        .ok_or_else(|| error("Runtime path is not valid UTF8"))?;
    let mut value = String::from("'");
    for unit in text.encode_utf16() {
        value.push_str(&format!("\\u{unit:04x}"));
    }
    value.push('\'');
    Ok(value)
}
fn install_renderer(
    profile: &Path,
    addon: &Path,
    socket: &Path,
    renderer: &RendererConfig,
) -> Result<(), RuntimeError> {
    if !renderer.remote_files_sha256.contains_key("package.json")
        || !renderer.remote_files_sha256.contains_key("main/index.js")
    {
        return Err(error("Pinned remote package inventory is incomplete"));
    }
    let source = renderer.remote_package.canonicalize().map_err(error)?;
    let destination = profile.join("node_modules/@electron/remote");
    for (relative, expected) in &renderer.remote_files_sha256 {
        let relative = Path::new(relative);
        if relative
            .components()
            .any(|part| !matches!(part, std::path::Component::Normal(_)))
        {
            return Err(error("Remote inventory contains a non-relative file"));
        }
        let file = source.join(relative);
        if !file.canonicalize().map_err(error)?.starts_with(&source) || hash(&file)? != *expected {
            return Err(error("Official remote package file digest mismatch"));
        }
        let target = destination.join(relative);
        fs::create_dir_all(target.parent().unwrap()).map_err(error)?;
        fs::copy(file, target).map_err(error)?;
    }
    fs::write(
        profile.join("bootstrap.cjs"),
        include_str!("renderer_main.cjs"),
    )
    .map_err(error)?;
    let bootstrap = include_str!("renderer_bootstrap.js")
        .replace("ADDON_PATH", &js_path(addon)?)
        .replace("SOCKET_PATH", &js_path(socket)?);
    fs::write(profile.join("renderer-bootstrap.js"), bootstrap).map_err(error)?;
    fs::write(
        profile.join("runtime.html"),
        "<!doctype html><meta charset='utf-8'><title>Owned runtime context</title>",
    )
    .map_err(error)?;
    Ok(())
}
fn terminate_group(pid: u32) {
    if let Ok(pid) = i32::try_from(pid) {
        if pid > 1 {
            unsafe {
                libc::kill(-pid, libc::SIGTERM);
            }
        }
    }
}
fn stop_group(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    if pid <= 1 {
        return false;
    }
    // This group was created exclusively for the owned renderer supervisor.
    terminate_group(pid as u32);
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        if unsafe { libc::kill(-pid, 0) } != 0 {
            return std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
        }
        if Instant::now() >= deadline {
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
            let deadline = Instant::now() + Duration::from_secs(1);
            while Instant::now() < deadline {
                if unsafe { libc::kill(-pid, 0) } != 0 {
                    return std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
                }
                thread::sleep(Duration::from_millis(10));
            }
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
}
impl NodeChild {
    pub fn launch(config: NodeChildConfig) -> Result<Self, RuntimeError> {
        Self::launch_mode(config, None)
    }
    pub fn launch_renderer(
        config: NodeChildConfig,
        renderer: RendererConfig,
    ) -> Result<Self, RuntimeError> {
        Self::launch_mode(config, Some(renderer))
    }
    fn launch_mode(
        config: NodeChildConfig,
        renderer: Option<RendererConfig>,
    ) -> Result<Self, RuntimeError> {
        guard(&config)?;
        let addon = config.addon.canonicalize().map_err(error)?;
        let executable = config.executable.canonicalize().map_err(error)?;
        let vault = config.vault.canonicalize().map_err(error)?;
        fs::create_dir(&config.profile).map_err(error)?;
        let profile = config.profile.canonicalize().map_err(error)?;
        for name in ["home", "tmp", "config", "cache", "user-data"] {
            fs::create_dir(profile.join(name)).map_err(error)?;
        }
        let bootstrap = profile.join("bootstrap.cjs");
        // No eval implementation, SDK object, Node polyfill or product logic.
        fs::write(&bootstrap,"'use strict';\nrequire(process.argv[2]).start(process.argv[3], require, module, __filename, __dirname, function(dispatch, name, constructible) { if (constructible) return function(...args) { return dispatch(this, new.target, args); }; return ({[name](...args) { return dispatch(this, new.target, args); }})[name]; }, function(value, key) { return value[key]; }, function(value,key,next) { return value[key]=next; }, function(operation,left,right) { switch(operation) { case 'add': return left+right; case 'subtract': return left-right; case 'less-than': return left<right; case 'greater-than': return left>right; default: throw new TypeError('Unsupported binary operation'); } }, function(value,constructor) { return value instanceof constructor; }, true, function(value,key) { return delete value[key]; }, (function(){const begin=function*(value){for(const key in value)yield key};const next=Object.getPrototypeOf(begin({})).next;const apply=Reflect.apply;return [begin,function(iterator){return apply(next,iterator,[])}];})());\n").map_err(error)?;
        // A short, owned path is needed for macOS's sockaddr_un limit.
        let socket_directory = tempfile::Builder::new()
            .prefix("mnemosyne-runtime-")
            .tempdir()
            .map_err(error)?;
        let socket = socket_directory.path().join("runtime.sock");
        if let Some(renderer) = &renderer {
            install_renderer(&profile, &addon, &socket, renderer)?;
        }
        let listener = UnixListener::bind(&socket).map_err(error)?;
        listener.set_nonblocking(true).map_err(error)?;
        let stdout = File::create(profile.join("stdout.log")).map_err(error)?;
        let stderr = File::create(profile.join("stderr.log")).map_err(error)?;
        let mut command = Command::new(executable);
        command
            .arg(&bootstrap)
            .arg(&addon)
            .arg(&socket)
            .arg(format!(
                "--user-data-dir={}",
                profile.join("user-data").display()
            ))
            .arg("--use-mock-keychain")
            .current_dir(vault)
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr)
            .env("HOME", profile.join("home"))
            .env("TMPDIR", profile.join("tmp"))
            .env("XDG_CONFIG_HOME", profile.join("config"))
            .env("XDG_CACHE_HOME", profile.join("cache"));
        for name in [
            "NODE_OPTIONS",
            "NODE_REPL_EXTERNAL_MODULE",
            "NODE_PATH",
            "ELECTRON_EXTRA_LAUNCH_ARGS",
        ] {
            command.env_remove(name);
        }
        if renderer.is_some() {
            command.env_remove("ELECTRON_RUN_AS_NODE");
            // Only this child's descendants share its exclusively owned group.
            command.process_group(0);
        } else {
            command.env("ELECTRON_RUN_AS_NODE", "1");
        }
        let mut child = command.spawn().map_err(error)?;
        let deadline = Instant::now() + config.startup_timeout;
        let accepted = loop {
            match listener.accept() {
                Ok((stream, _)) => break Ok(stream),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => break Err(error(e)),
            }
            match child.try_wait() {
                Ok(Some(status)) => {
                    break Err(error(format!(
                        "Runtime exited before connecting: {status}; logs in {}",
                        profile.display()
                    )));
                }
                Ok(None) => {}
                Err(e) => break Err(error(e)),
            }
            if Instant::now() >= deadline {
                break Err(error("Runtime IPC startup timed out"));
            }
            thread::sleep(Duration::from_millis(10));
        };
        let stream = match accepted {
            Ok(s) => s,
            Err(e) => {
                if renderer.is_some() {
                    terminate_group(child.id());
                }
                let _ = child.kill();
                let _ = child.wait();
                if renderer.is_some() {
                    stop_group(child.id());
                }
                return Err(e);
            }
        };
        let (lease_sender, lease_releases) = mpsc::channel();
        let mut runtime = Self {
            child,
            stream,
            _socket_directory: socket_directory,
            next_id: 1,
            versions: BTreeMap::new(),
            profile,
            stopped: None,
            events: None,
            reader: None,
            completions: Vec::new(),
            poll_inflight: None,
            handlers: BTreeSet::new(),
            outcomes: BTreeMap::new(),
            late_console: VecDeque::new(),
            deferred_events: VecDeque::new(),
            renderer: renderer.is_some(),
            runtime_pid: 0,
            lease_owner: Arc::new(crate::root_lease::LeaseOwner {
                alive: AtomicBool::new(true),
            }),
            lease_sender,
            lease_releases,
        };
        runtime.stream.set_nonblocking(false).map_err(error)?;
        runtime
            .stream
            .set_read_timeout(Some(config.startup_timeout))
            .map_err(error)?;
        runtime.send(&ParentMessage::Initialize {
            version: PROTOCOL_VERSION,
            context: config.context,
        })?;
        match runtime.receive()? {
            ChildMessage::Ready {
                version,
                context,
                pid,
                versions,
            } if version == PROTOCOL_VERSION && context == config.context => {
                if runtime.renderer {
                    let receipt = fs::read_to_string(runtime.profile.join("renderer-ready.txt"))
                        .map_err(error)?;
                    let expected = format!("{} {pid}", runtime.child.id());
                    if receipt.trim() != expected || pid == runtime.child.id() || pid == 0 {
                        return Err(RuntimeError::Initialization(
                            "Renderer PID does not match owned browser WebContents receipt".into(),
                        ));
                    }
                } else if pid != runtime.child.id() {
                    return Err(RuntimeError::Initialization(
                        "Runtime PID does not match owned child".into(),
                    ));
                }
                runtime.runtime_pid = pid;
                for (name, expected) in config.identity.versions {
                    if versions.get(&name) != Some(&expected) {
                        return Err(RuntimeError::Initialization(format!(
                            "Actual runtime {name} does not match {expected}"
                        )));
                    }
                }
                runtime.versions = versions;
            }
            other => {
                return Err(error(format!(
                    "Unexpected runtime identity response: {other:?}"
                )));
            }
        }
        runtime.stream.set_read_timeout(None).map_err(error)?;
        let mut reader = runtime.stream.try_clone().map_err(error)?;
        let (sender, receiver) = mpsc::channel();
        runtime.reader = Some(
            thread::Builder::new()
                .name("mnemosyne-runtime-events".into())
                .spawn(move || {
                    loop {
                        let message =
                            read_frame::<ChildMessage>(&mut reader).map_err(|e| e.to_string());
                        let failed = message.is_err();
                        if sender.send(message).is_err() || failed {
                            break;
                        }
                    }
                })
                .map_err(error)?,
        );
        runtime.events = Some(receiver);
        Ok(runtime)
    }
    pub fn pid(&self) -> u32 {
        self.child.id()
    }
    pub fn runtime_pid(&self) -> u32 {
        self.runtime_pid
    }
    pub fn versions(&self) -> &BTreeMap<String, String> {
        &self.versions
    }
    pub fn profile(&self) -> &Path {
        &self.profile
    }
    fn id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
    fn send(&mut self, message: &ParentMessage) -> Result<(), RuntimeError> {
        write_frame(&mut self.stream, message).map_err(error)
    }
    fn receive(&mut self) -> Result<ChildMessage, RuntimeError> {
        if let Some(message) = self.deferred_events.pop_front() {
            return Ok(message);
        }
        self.receive_wire()
    }
    fn receive_wire(&mut self) -> Result<ChildMessage, RuntimeError> {
        if let Some(events) = &self.events {
            events.recv().map_err(error)?.map_err(error)
        } else {
            read_frame(&mut self.stream).map_err(error)
        }
    }
    pub fn evaluate(&mut self, code: Option<&str>) -> Result<EvalOutcome, RuntimeError> {
        self.evaluate_with(
            code.map(|s| s.encode_utf16().collect()),
            |_| Err(HostError::unsupported("No Rust host dispatcher supplied")),
            |_, _| {},
        )
    }
    /// Serial evaluation in the persistent per-vault context. The caller owns
    /// the Rust backend and runs callbacks on its own thread, never the JS thread.
    pub fn evaluate_with(
        &mut self,
        code: Option<Vec<u16>>,
        mut host: impl FnMut(HostCall) -> HostResult,
        mut console: impl FnMut(ConsoleLevel, &[u16]),
    ) -> Result<EvalOutcome, RuntimeError> {
        self.evaluate_with_context(
            code,
            |_, call| host(call),
            |level, message| console(level, message),
        )
    }
    /// The backend may inspect actual receiver/argument descriptors while a
    /// synchronous JS method is waiting; the JS owning thread services those
    /// messages in its sync wait loop rather than deadlocking on TSFN delivery.
    pub fn evaluate_with_context(
        &mut self,
        code: Option<Vec<u16>>,
        mut host: impl FnMut(&mut Self, HostCall) -> HostResult,
        mut console: impl FnMut(ConsoleLevel, &[u16]),
    ) -> Result<EvalOutcome, RuntimeError> {
        self.evaluate_with_requests(
            code,
            |runtime, request| {
                let result = host(runtime, request.call.clone());
                request.respond(result);
            },
            |level, message| console(level, message),
        )
    }
    /// A real Rust request may retain completion and resolve on another Rust
    /// thread. The caller continues pumping IPC; no JS handles leave the child.
    pub fn evaluate_with_requests(
        &mut self,
        code: Option<Vec<u16>>,
        mut host: impl FnMut(&mut Self, crate::HostRequest),
        mut console: impl FnMut(ConsoleLevel, &[u16]),
    ) -> Result<EvalOutcome, RuntimeError> {
        self.evaluate_with_dispatcher(code, &mut host, |level, message| console(level, message))
    }
    pub fn evaluate_with_dispatcher(
        &mut self,
        code: Option<Vec<u16>>,
        host: &mut dyn NodeHostDispatcher,
        mut console: impl FnMut(ConsoleLevel, &[u16]),
    ) -> Result<EvalOutcome, RuntimeError> {
        self.flush_root_releases()?;
        let id = self.id();
        self.send(&ParentMessage::Evaluate { id, code })?;
        loop {
            if let Some(body) = self.outcomes.remove(&id) {
                return Ok(EvalOutcome {
                    body: String::from_utf16_lossy(&body),
                });
            }
            self.poll_completions()?;
            self.request_poll(host)?;
            let message = if let Some(message) = self.deferred_events.pop_front() {
                message
            } else {
                match self
                    .events
                    .as_ref()
                    .ok_or_else(|| error("Runtime reader is not initialized"))?
                    .recv_timeout(Duration::from_millis(5))
                {
                    Ok(message) => message.map_err(error)?,
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(e) => return Err(error(e)),
                }
            };
            match message {
                ChildMessage::Evaluated { id: reply, body } if reply == id => {
                    return Ok(EvalOutcome {
                        body: String::from_utf16_lossy(&body),
                    });
                }
                ChildMessage::Console {
                    id: reply,
                    level,
                    message,
                } if reply == id => console(level, &message),
                ChildMessage::HostCall {
                    eval_id,
                    id: call,
                    call: request,
                } => {
                    let _ = eval_id;
                    self.dispatch_request(call, request, host)?;
                }
                ChildMessage::HostPoll { id: poll } => self.finish_poll(poll, host)?,
                ChildMessage::HandlerStarted { id, capture } => {
                    self.handlers.insert((id, capture));
                }
                ChildMessage::HandlerFinished { id, capture } => {
                    self.handlers.remove(&(id, capture));
                }
                ChildMessage::Console { id, level, message } => {
                    self.late_console.push_back(RuntimeConsoleEvent {
                        evaluation_id: id,
                        level,
                        message,
                    })
                }
                ChildMessage::Evaluated { id, body } => {
                    self.outcomes.insert(id, body);
                }
                ChildMessage::Failed {
                    id: reply,
                    error: e,
                } if reply == Some(id) => return Err(error(format!("{}: {}", e.name, e.message))),
                other => return Err(error(format!("Unexpected runtime response: {other:?}"))),
            }
        }
    }
    fn dispatch_request(
        &mut self,
        id: u64,
        call: HostCall,
        host: &mut dyn NodeHostDispatcher,
    ) -> Result<(), RuntimeError> {
        let asynchronous = call.binding.delivery != crate::Delivery::Sync;
        let (request, completion) = crate::HostRequest::channel(call);
        if let Some(completion) = completion {
            self.completions.push((id, completion));
        }
        host.dispatch(self, request);
        // Promise construction precedes this wait, but the callback cannot
        // return the Promise until the source-synchronous Rust prefix finishes.
        if asynchronous {
            self.send(&ParentMessage::PrefixReady { id })?;
        }
        Ok(())
    }
    fn request_poll(&mut self, host: &dyn NodeHostDispatcher) -> Result<(), RuntimeError> {
        if self.poll_inflight.is_none() && host.has_pending_jobs() {
            let id = self.id();
            self.send(&ParentMessage::RequestPoll { id })?;
            self.poll_inflight = Some(id);
        }
        Ok(())
    }
    fn finish_poll(
        &mut self,
        id: u64,
        host: &mut dyn NodeHostDispatcher,
    ) -> Result<(), RuntimeError> {
        if self.poll_inflight != Some(id) {
            return Err(error("Unexpected owner-thread poll identity"));
        }
        self.poll_inflight = None;
        host.poll(self);
        self.send(&ParentMessage::PollFinished { id })?;
        Ok(())
    }
    fn poll_completions(&mut self) -> Result<(), RuntimeError> {
        let mut index = 0;
        while index < self.completions.len() {
            let result = match self.completions[index].1.try_recv() {
                Ok(Some(result)) => Some(result),
                Ok(None) => None,
                Err(_) => Some(Err(HostError::unsupported(
                    "Rust backend request was dropped",
                ))),
            };
            if let Some(result) = result {
                let (id, _) = self.completions.remove(index);
                self.send(&ParentMessage::HostReply { id, result })?;
            } else {
                index += 1;
            }
        }
        Ok(())
    }
    fn ack(&mut self, id: u64) -> Result<(), RuntimeError> {
        loop {
            match self.receive_wire()? {
                ChildMessage::Acknowledged { id: reply, result } if reply == id => {
                    return result.map_err(|e| error(format!("{}: {}", e.name, e.message)));
                }
                ChildMessage::HandlerStarted { id, capture } => {
                    self.handlers.insert((id, capture));
                }
                ChildMessage::HandlerFinished { id, capture } => {
                    self.handlers.remove(&(id, capture));
                }
                message
                    if matches!(
                        message,
                        ChildMessage::Console { .. }
                            | ChildMessage::HostCall { .. }
                            | ChildMessage::Evaluated { .. }
                    ) =>
                {
                    self.deferred_events.push_back(message)
                }
                other => return Err(error(format!("Unexpected control reply: {other:?}"))),
            }
        }
    }
    pub fn bind_function(
        &mut self,
        name: &str,
        binding: crate::HostBinding,
    ) -> Result<(), RuntimeError> {
        let id = self.id();
        self.send(&ParentMessage::Bind {
            id,
            name: name.encode_utf16().collect(),
            binding,
        })?;
        self.ack(id)
    }
    pub fn register_class(&mut self, class: crate::HostClassSpec) -> Result<(), RuntimeError> {
        let id = self.id();
        self.send(&ParentMessage::RegisterClass { id, class })?;
        self.ack(id)
    }
    pub fn register_instance(
        &mut self,
        object: crate::HostObjectId,
        class: &str,
        reference: Option<crate::JsValueId>,
        properties: Vec<crate::HostDescriptor>,
    ) -> Result<(), RuntimeError> {
        let id = self.id();
        self.send(&ParentMessage::RegisterInstance {
            id,
            object,
            class: class.encode_utf16().collect(),
            reference,
            properties,
        })?;
        self.ack(id)
    }
    pub fn update_instance(
        &mut self,
        object: crate::HostObjectId,
        properties: Vec<crate::HostDescriptor>,
    ) -> Result<(), RuntimeError> {
        let id = self.id();
        self.send(&ParentMessage::UpdateInstance {
            id,
            object,
            properties,
        })?;
        self.ack(id)
    }
    pub fn release_reference(&mut self, reference: crate::JsValueId) -> Result<(), RuntimeError> {
        let id = self.id();
        self.send(&ParentMessage::Release { id, reference })?;
        self.ack(id)
    }
    /// Add a distinct ownership lease. Cloning it shares one retain. Existing
    /// borrowed-root ownership is separate and may be released explicitly.
    pub fn retain_reference(
        &mut self,
        reference: crate::JsReference,
    ) -> Result<crate::RootLease, HostError> {
        let id = self.id();
        self.send(&ParentMessage::RetainReference {
            id,
            reference: reference.id,
        })
        .map_err(|e| HostError::unsupported(e.to_string()))?;
        self.ack(id)
            .map_err(|e| HostError::unsupported(e.to_string()))?;
        Ok(crate::RootLease::new(
            reference,
            self.lease_owner.clone(),
            self.lease_sender.clone(),
        ))
    }
    pub fn validate_lease(&self, lease: &crate::RootLease) -> Result<(), HostError> {
        if lease.belongs_to(&self.lease_owner) {
            Ok(())
        } else {
            Err(HostError::unsupported(
                "Foreign or closed JavaScript root lease",
            ))
        }
    }
    pub fn retain_error(&mut self, error: HostError) -> Result<crate::RootedHostError, HostError> {
        fn references(value: &crate::HostValue, output: &mut Vec<crate::JsReference>) {
            match value {
                crate::HostValue::Reference(value) => {
                    if !output.contains(value) {
                        output.push(*value);
                    }
                }
                crate::HostValue::Array(values) => {
                    for value in values {
                        references(value, output);
                    }
                }
                crate::HostValue::Record(values) => {
                    for (_, value) in values {
                        references(value, output);
                    }
                }
                _ => {}
            }
        }
        let mut values = Vec::new();
        if let Some(thrown) = &error.thrown {
            references(thrown, &mut values);
        }
        let roots = values
            .into_iter()
            .map(|reference| self.retain_reference(reference))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(crate::RootedHostError { error, roots })
    }
    /// Service queued final lease drops on the runtime thread. An idle owner
    /// should call this too; stop releases all remaining environment roots.
    pub fn flush_root_releases(&mut self) -> Result<(), RuntimeError> {
        if self.stopped.is_some() {
            while self.lease_releases.try_recv().is_ok() {}
            return Ok(());
        }
        while let Ok(reference) = self.lease_releases.try_recv() {
            let id = self.id();
            self.send(&ParentMessage::ReleaseLease { id, reference })?;
            self.ack(id)?;
        }
        Ok(())
    }
    pub fn inspect(
        &mut self,
        query: crate::ObjectQuery,
    ) -> Result<crate::ObjectInspection, RuntimeError> {
        self.inspect_host(query)
            .map_err(|e| error(format!("{}: {} (thrown={:?})", e.name, e.message, e.thrown)))
    }
    pub fn inspect_host(
        &mut self,
        query: crate::ObjectQuery,
    ) -> Result<crate::ObjectInspection, HostError> {
        let mut unsupported = |_: &mut Self, request: crate::HostRequest| {
            request.respond(Err(HostError::unsupported(
                "Nested Rust host dispatcher not supplied",
            )))
        };
        self.inspect_with_dispatcher(query, &mut unsupported)
    }
    pub fn inspect_with_dispatcher(
        &mut self,
        query: crate::ObjectQuery,
        host: &mut dyn NodeHostDispatcher,
    ) -> Result<crate::ObjectInspection, HostError> {
        self.flush_root_releases()
            .map_err(|error| HostError::unsupported(error.to_string()))?;
        let id = self.id();
        self.send(&ParentMessage::Inspect { id, query })
            .map_err(|e| HostError::unsupported(e.to_string()))?;
        loop {
            self.poll_completions()
                .map_err(|e| HostError::unsupported(e.to_string()))?;
            self.request_poll(host)
                .map_err(|e| HostError::unsupported(e.to_string()))?;
            let message = match self
                .events
                .as_ref()
                .ok_or_else(|| HostError::unsupported("Runtime reader is not initialized"))?
                .recv_timeout(Duration::from_millis(5))
            {
                Ok(message) => message.map_err(HostError::unsupported)?,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(e) => return Err(HostError::unsupported(e.to_string())),
            };
            match message {
                ChildMessage::Inspected { id: reply, result } if reply == id => return result,
                ChildMessage::HostCall { id, call, .. } => {
                    self.dispatch_request(id, call, host)
                        .map_err(|e| HostError::unsupported(e.to_string()))?;
                }
                ChildMessage::HostPoll { id } => self
                    .finish_poll(id, host)
                    .map_err(|e| HostError::unsupported(e.to_string()))?,
                ChildMessage::HandlerStarted { id, capture } => {
                    self.handlers.insert((id, capture));
                }
                ChildMessage::HandlerFinished { id, capture } => {
                    self.handlers.remove(&(id, capture));
                }
                ChildMessage::Evaluated { id, body } => {
                    self.outcomes.insert(id, body);
                }
                message if matches!(message, ChildMessage::Console { .. }) => {
                    self.deferred_events.push_back(message)
                }
                other => {
                    return Err(HostError::unsupported(format!(
                        "Unexpected inspection reply: {other:?}"
                    )));
                }
            }
        }
    }
    /// Handlers can remain live after their outer native CLI Promise rejects.
    pub fn pending_handlers(&self) -> usize {
        self.handlers.len()
    }
    /// Console events from retired/other requests, in wire arrival order.
    pub fn take_console_events(&mut self) -> Vec<RuntimeConsoleEvent> {
        self.late_console.drain(..).collect()
    }
    /// Nonblocking owner-thread idle pump. Backend requests/completions and
    /// continuation turns must also be serviced between command evaluations.
    pub fn poll_events(
        &mut self,
        host: &mut dyn NodeHostDispatcher,
        mut console: impl FnMut(u64, ConsoleLevel, &[u16]),
    ) -> Result<usize, RuntimeError> {
        self.flush_root_releases()?;
        self.poll_completions()?;
        self.request_poll(host)?;
        let mut count = 0;
        loop {
            let message = if let Some(message) = self.deferred_events.pop_front() {
                message
            } else {
                match self
                    .events
                    .as_ref()
                    .ok_or_else(|| error("Runtime reader is not initialized"))?
                    .try_recv()
                {
                    Ok(message) => message.map_err(error)?,
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(e) => return Err(error(e)),
                }
            };
            count += 1;
            match message {
                ChildMessage::HostCall { id, call, .. } => self.dispatch_request(id, call, host)?,
                ChildMessage::HostPoll { id } => self.finish_poll(id, host)?,
                ChildMessage::HandlerStarted { id, capture } => {
                    self.handlers.insert((id, capture));
                }
                ChildMessage::HandlerFinished { id, capture } => {
                    self.handlers.remove(&(id, capture));
                }
                ChildMessage::Console { id, level, message } => console(id, level, &message),
                ChildMessage::Evaluated { id, body } => {
                    self.outcomes.insert(id, body);
                }
                ChildMessage::Failed { error: e, .. } => {
                    return Err(error(format!("{}: {}", e.name, e.message)));
                }
                other => return Err(error(format!("Unexpected idle runtime reply: {other:?}"))),
            }
            self.poll_completions()?;
            self.request_poll(host)?;
        }
        Ok(count)
    }
    pub fn stop(&mut self) -> Result<StopReceipt, RuntimeError> {
        self.lease_owner.alive.store(false, Ordering::Release);
        if let Some(receipt) = &self.stopped {
            return Ok(receipt.clone());
        }
        let pid = self.child.id();
        let _ = self.send(&ParentMessage::Shutdown);
        let deadline = Instant::now() + Duration::from_secs(if self.renderer { 3 } else { 1 });
        loop {
            if let Some(status) = self.child.try_wait().map_err(error)? {
                let process_group_stopped = !self.renderer || stop_group(pid);
                let receipt = StopReceipt {
                    pid,
                    exit_code: status.code(),
                    reaped: true,
                    runtime_pid: self.runtime_pid,
                    process_group_stopped,
                };
                self.stopped = Some(receipt.clone());
                if let Some(reader) = self.reader.take() {
                    let _ = reader.join();
                }
                return Ok(receipt);
            }
            if Instant::now() >= deadline {
                if self.renderer {
                    terminate_group(pid);
                }
                let _ = self.child.kill();
                let status = self.child.wait().map_err(error)?;
                let receipt = StopReceipt {
                    pid,
                    exit_code: status.code(),
                    reaped: true,
                    runtime_pid: self.runtime_pid,
                    process_group_stopped: !self.renderer || stop_group(pid),
                };
                self.stopped = Some(receipt.clone());
                if let Some(reader) = self.reader.take() {
                    let _ = reader.join();
                }
                return Ok(receipt);
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for NodeChild {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
