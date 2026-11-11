//! Unix transport for a long-lived headless engine.
//!
//! Native-client requests are newline-delimited JSON; output remains unframed.
//! Stream callbacks own nonvisual interactive behavior while connection I/O stays
//! nonblocking. All state callbacks are serialized; a synchronous callback cannot
//! be preempted. The native absolute header deadline is five seconds, with no
//! configured default size/output limit. Optional caller limits are authored policy.
use crate::Engine;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CliRequest {
    pub argv: Vec<String>,
    pub tty: bool,
    pub cwd: String,
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub max_request_bytes: usize,
    pub request_timeout: Duration,
    /// Optional authored output guard. Native Pt write/end has no output deadline.
    pub output_timeout: Option<Duration>,
}
impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            // Source Qe has an absolute five-second header timer, no size cap.
            max_request_bytes: usize::MAX,
            request_timeout: Duration::from_secs(5),
            output_timeout: None,
        }
    }
}

/// Identity belongs only to this explicitly bound server instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConnectionId(pub u64);
#[derive(Debug)]
pub enum StreamEvent {
    Request(CliRequest),
    /// Poll only an explicitly suspended command, never ordinary connections.
    Poll,
    Input(Vec<u8>),
    Eof,
    Closed,
}
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum InputPolicy {
    #[default]
    Preserve,
    /// Discard already buffered input after an execution frame.
    /// This primitive does not implement an asynchronous Promise scheduler.
    DiscardBuffered,
    /// Suspend command completion while leaving other connections serviceable.
    WaitForCompletion,
}
#[derive(Debug, Default)]
pub struct StreamAction {
    pub output: Vec<u8>,
    pub close_after_output: bool,
    pub input_policy: InputPolicy,
}
impl StreamAction {
    pub fn finish(output: Vec<u8>) -> Self {
        Self {
            output,
            close_after_output: true,
            input_policy: InputPolicy::Preserve,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("socket path already exists: {0}")]
    ExistingPath(PathBuf),
    #[error("invalid server configuration: {0}")]
    Configuration(&'static str),
    #[error("{0}: {1}")]
    Io(PathBuf, #[source] std::io::Error),
}

#[derive(Debug, Clone)]
pub struct ServerControl(Arc<AtomicBool>);
impl ServerControl {
    pub fn stop(&self) {
        self.0.store(true, Ordering::Release);
    }
    pub fn is_stopped(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// An explicitly bound socket. It never discovers or replaces other applications' sockets.
pub struct UnixStateServer<S> {
    listener: UnixListener,
    socket_path: PathBuf,
    socket_identity: (u64, u64),
    state: S,
    config: ServerConfig,
    control: ServerControl,
}
/// Compatibility name for the single-engine transport.
pub type UnixEngineServer = UnixStateServer<Engine>;
impl<S> UnixStateServer<S> {
    pub fn bind(path: impl AsRef<Path>, state: S) -> Result<Self, ServerError> {
        Self::bind_with_config(path, state, ServerConfig::default())
    }
    pub fn bind_with_config(
        path: impl AsRef<Path>,
        state: S,
        config: ServerConfig,
    ) -> Result<Self, ServerError> {
        if config.max_request_bytes == 0 {
            return Err(ServerError::Configuration(
                "max_request_bytes must be positive",
            ));
        }
        if config.request_timeout.is_zero() {
            return Err(ServerError::Configuration(
                "request_timeout must be positive",
            ));
        }
        if config
            .output_timeout
            .is_some_and(|timeout| timeout.is_zero())
        {
            return Err(ServerError::Configuration(
                "output_timeout must be positive when set",
            ));
        }
        let socket_path = path.as_ref().to_path_buf();
        match std::fs::symlink_metadata(&socket_path) {
            Ok(_) => return Err(ServerError::ExistingPath(socket_path)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(ServerError::Io(socket_path, error)),
        }
        let listener = UnixListener::bind(&socket_path)
            .map_err(|error| ServerError::Io(socket_path.clone(), error))?;
        let metadata = std::fs::symlink_metadata(&socket_path)
            .map_err(|error| ServerError::Io(socket_path.clone(), error))?;
        let server = Self {
            listener,
            socket_identity: (metadata.dev(), metadata.ino()),
            socket_path,
            state,
            config,
            control: ServerControl(Arc::new(AtomicBool::new(false))),
        };
        std::fs::set_permissions(&server.socket_path, std::fs::Permissions::from_mode(0o600))
            .map_err(|error| ServerError::Io(server.socket_path.clone(), error))?;
        server
            .listener
            .set_nonblocking(true)
            .map_err(|error| ServerError::Io(server.socket_path.clone(), error))?;
        Ok(server)
    }
    pub fn control(&self) -> ServerControl {
        self.control.clone()
    }
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }
    pub fn state(&self) -> &S {
        &self.state
    }
    pub fn state_mut(&mut self) -> &mut S {
        &mut self.state
    }
    /// Serialize command mutations through the same Engine. Invalid requests,
    /// timed-out clients and disconnected output readers affect one connection.
    /// The callback owns application error formatting; transport has no exit-code
    /// response field, matching the native client's raw relay contract.
    pub fn run(
        &mut self,
        handler: impl FnMut(&mut S, CliRequest) -> Vec<u8>,
    ) -> Result<(), ServerError> {
        self.run_with_event_loop(handler, |_| {})
    }
    /// Process state events while idle and while a client stalls transport I/O.
    /// Both callbacks run on this thread: synchronous command execution cannot
    /// be preempted. This is not an asynchronous JavaScript job/event scheduler.
    pub fn run_with_event_loop(
        &mut self,
        mut handler: impl FnMut(&mut S, CliRequest) -> Vec<u8>,
        tick: impl FnMut(&mut S),
    ) -> Result<(), ServerError> {
        self.run_streaming_with_event_loop(
            |state, _, event| match event {
                StreamEvent::Request(request) => StreamAction::finish(handler(state, request)),
                _ => StreamAction::default(),
            },
            tick,
        )
    }
    /// One serialized state owner with independent nonblocking connection I/O.
    /// The caller owns per-connection command/input state and visual rendering.
    /// Closed is delivered exactly once, including on stop; its returned output
    /// is ignored because the connection is no longer writable.
    pub fn run_streaming_with_event_loop(
        &mut self,
        mut handler: impl FnMut(&mut S, ConnectionId, StreamEvent) -> StreamAction,
        mut tick: impl FnMut(&mut S),
    ) -> Result<(), ServerError> {
        let mut connections = Vec::<Connection>::new();
        let mut next_id = 1u64;
        let result = (|| {
            while !self.control.is_stopped() {
                tick(&mut self.state);
                if self.control.is_stopped() {
                    break;
                }
                for _ in 0..32 {
                    match self.listener.accept() {
                        Ok((stream, _)) => {
                            if let Ok(connection) = Connection::new(stream, ConnectionId(next_id)) {
                                connections.push(connection);
                                next_id += 1;
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(error) => return Err(ServerError::Io(self.socket_path.clone(), error)),
                    }
                }
                for connection in &mut connections {
                    if self.control.is_stopped() {
                        break;
                    }
                    if connection.awaiting_completion && !connection.closing {
                        let action = handler(&mut self.state, connection.id, StreamEvent::Poll);
                        let discard = action.input_policy == InputPolicy::DiscardBuffered;
                        connection.apply(action);
                        if discard {
                            connection.discard_buffered_input();
                        }
                    }
                    let mut events = connection.poll_events(&self.config);
                    while let Some(event) = events.pop_front() {
                        if connection.closing || self.control.is_stopped() {
                            break;
                        }
                        let eof = matches!(event, StreamEvent::Eof);
                        let action = handler(&mut self.state, connection.id, event);
                        if self.control.is_stopped() {
                            break;
                        }
                        tick(&mut self.state);
                        let discard = action.input_policy == InputPolicy::DiscardBuffered;
                        connection.apply(action);
                        if discard {
                            events.retain(|event| !matches!(event, StreamEvent::Input(_)));
                            connection.discard_buffered_input();
                        }
                        // Node's default allowHalfOpen=false ends the writable
                        // side when readable EOF arrives, after queued output.
                        if eof {
                            connection.closing = true;
                        }
                    }
                    if self.control.is_stopped() {
                        break;
                    }
                    connection.poll_output(&self.config);
                }
                let mut index = 0;
                while index < connections.len() {
                    if connections[index].closed {
                        let connection = connections.remove(index);
                        let _ = handler(&mut self.state, connection.id, StreamEvent::Closed);
                    } else {
                        index += 1;
                    }
                }
                if !self.control.is_stopped() {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            Ok(())
        })();
        for connection in connections {
            let _ = handler(&mut self.state, connection.id, StreamEvent::Closed);
        }
        result
    }
}
impl UnixStateServer<Engine> {
    /// Run the engine's Markdown autosave callbacks without requiring another
    /// CLI request. Autosave errors are recorded by the engine and do not stop
    /// the service or prevent other due views from saving.
    pub fn run_engine(
        &mut self,
        handler: impl FnMut(&mut Engine, CliRequest) -> Vec<u8>,
    ) -> Result<(), ServerError> {
        self.state.ensure_startup_layout().map_err(|error| {
            ServerError::Io(
                self.socket_path.clone(),
                std::io::Error::other(error.to_string()),
            )
        })?;
        self.state
            .enable_metadata_lifecycle()
            .and_then(|_| self.state.enable_retained_watch())
            .map_err(|error| {
                ServerError::Io(
                    self.socket_path.clone(),
                    std::io::Error::other(error.to_string()),
                )
            })?;
        self.run_with_event_loop(handler, |engine| {
            let _ = engine.poll_external_changes();
            engine.poll_editor_autosave();
            let _ = engine.poll_metadata_lifecycle();
            engine.poll_file_operations();
            let _ = engine.poll_metadata_cache_gc();
            if let Err(error) = engine.poll_core_plugin_config() {
                eprintln!("Core plugin config save failed: {error}");
            }
        })
    }
    pub fn engine(&self) -> &Engine {
        &self.state
    }
    pub fn engine_mut(&mut self) -> &mut Engine {
        &mut self.state
    }
}
impl UnixStateServer<crate::host::VaultHost> {
    /// Each loaded vault keeps its own editor state and pending callbacks.
    pub fn run_host(
        &mut self,
        handler: impl FnMut(&mut crate::host::VaultHost, CliRequest) -> Vec<u8>,
    ) -> Result<(), ServerError> {
        self.state.enable_retained_services().map_err(|error| {
            ServerError::Io(
                self.socket_path.clone(),
                std::io::Error::other(error.to_string()),
            )
        })?;
        self.run_with_event_loop(handler, |host| {
            let ids: Vec<_> = host.loaded_vault_ids().map(str::to_owned).collect();
            for id in ids {
                if let Some(engine) = host.engine_mut(&id) {
                    let _ = engine.poll_external_changes();
                    engine.poll_editor_autosave();
                    let _ = engine.poll_metadata_lifecycle();
                    engine.poll_file_operations();
                    let _ = engine.poll_metadata_cache_gc();
                    if let Err(error) = engine.poll_core_plugin_config() {
                        eprintln!("Core plugin config save failed: {error}");
                    }
                }
            }
        })
    }
}
impl<S> Drop for UnixStateServer<S> {
    fn drop(&mut self) {
        // Do not remove a replacement path created by another process after
        // this server's socket was unlinked.
        if let Ok(metadata) = std::fs::symlink_metadata(&self.socket_path) {
            if metadata.file_type().is_socket()
                && (metadata.dev(), metadata.ino()) == self.socket_identity
            {
                let _ = std::fs::remove_file(&self.socket_path);
            }
        }
    }
}
/// Matches the pinned libuv 1.52.1 Unix allocation hint. Read/event boundaries
/// still depend on the OS; source Qe decodes each received chunk independently.
const READ_CHUNK: usize = 64 * 1024;
struct Connection {
    id: ConnectionId,
    stream: UnixStream,
    header: Option<(String, usize, Instant)>,
    output: VecDeque<Vec<u8>>,
    offset: usize,
    output_started: Option<Instant>,
    pending_eof: bool,
    awaiting_completion: bool,
    eof: bool,
    closing: bool,
    closed: bool,
}
impl Connection {
    fn new(stream: UnixStream, id: ConnectionId) -> std::io::Result<Self> {
        stream.set_nonblocking(true)?;
        Ok(Self {
            id,
            stream,
            header: Some((String::new(), 0, Instant::now())),
            output: VecDeque::new(),
            offset: 0,
            output_started: None,
            pending_eof: false,
            awaiting_completion: false,
            eof: false,
            closing: false,
            closed: false,
        })
    }
    fn poll_events(&mut self, config: &ServerConfig) -> VecDeque<StreamEvent> {
        let mut events = VecDeque::new();
        if self.closing || self.closed {
            return events;
        }
        if let Some((_, _, started)) = &self.header {
            if started.elapsed() >= config.request_timeout {
                self.closed = true;
                return events;
            }
        }
        if self.pending_eof {
            self.pending_eof = false;
            events.push_back(StreamEvent::Eof);
            return events;
        }
        if self.eof {
            return events;
        }
        let mut buffer = [0u8; READ_CHUNK];
        for _ in 0..8 {
            match self.stream.read(&mut buffer) {
                Ok(0) => {
                    self.eof = true;
                    if self.header.is_some() {
                        self.closed = true;
                    } else {
                        events.push_back(StreamEvent::Eof);
                    }
                    break;
                }
                Ok(count) => {
                    if let Some((text, raw_len, _)) = &mut self.header {
                        let raw = &buffer[..count];
                        let prefix_len =
                            raw.iter().position(|byte| *byte == b'\n').unwrap_or(count);
                        if raw_len
                            .checked_add(prefix_len)
                            .is_none_or(|n| n > config.max_request_bytes)
                        {
                            self.closed = true;
                            break;
                        }
                        *raw_len += prefix_len;
                        // Preserve native Buffer.toString per-chunk replacement,
                        // including a split UTF-8 sequence. Do not heal chunks.
                        let decoded = String::from_utf8_lossy(raw);
                        if let Some(end) = decoded.find('\n') {
                            text.push_str(&decoded[..end]);
                            let parsed = serde_json::from_str(text);
                            let trailing = decoded[end + 1..].as_bytes().to_vec();
                            self.header = None;
                            match parsed {
                                Ok(request) => {
                                    events.push_back(StreamEvent::Request(request));
                                    if !trailing.is_empty() {
                                        events.push_back(StreamEvent::Input(trailing));
                                    }
                                }
                                // Actual malformed JSON invokes a main-process
                                // error dialog. That UI error branch is deferred;
                                // this headless host closes only this connection.
                                Err(_) => {
                                    self.closed = true;
                                    break;
                                }
                            }
                        } else {
                            text.push_str(&decoded);
                        }
                    } else {
                        events.push_back(StreamEvent::Input(buffer[..count].to_vec()));
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    self.closed = true;
                    break;
                }
            }
        }
        events
    }
    fn apply(&mut self, action: StreamAction) {
        match action.input_policy {
            InputPolicy::WaitForCompletion => self.awaiting_completion = true,
            InputPolicy::DiscardBuffered => self.awaiting_completion = false,
            InputPolicy::Preserve => {}
        }
        if action.close_after_output {
            self.awaiting_completion = false;
        }
        if !action.output.is_empty() {
            if self.output.is_empty() {
                self.output_started = Some(Instant::now());
            }
            self.output.push_back(action.output);
        }
        self.closing |= action.close_after_output;
    }
    fn discard_buffered_input(&mut self) {
        let mut buffer = [0u8; READ_CHUNK];
        for _ in 0..32 {
            match self.stream.read(&mut buffer) {
                Ok(0) => {
                    self.eof = true;
                    self.pending_eof = true;
                    break;
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    self.closed = true;
                    break;
                }
            }
        }
    }
    fn poll_output(&mut self, config: &ServerConfig) {
        if self.closed {
            return;
        }
        if config
            .output_timeout
            .zip(self.output_started)
            .is_some_and(|(limit, started)| started.elapsed() >= limit)
        {
            self.closed = true;
            return;
        }
        for _ in 0..8 {
            let Some(bytes) = self.output.front() else {
                break;
            };
            match self.stream.write(&bytes[self.offset..]) {
                Ok(0) => {
                    self.closed = true;
                    return;
                }
                Ok(count) => {
                    self.offset += count;
                    if self.offset == bytes.len() {
                        self.output.pop_front();
                        self.offset = 0;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    self.closed = true;
                    return;
                }
            }
        }
        if self.output.is_empty() {
            self.output_started = None;
            self.closed |= self.closing;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root = PathBuf::from(format!(
                "/tmp/mnemo-s-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&root).unwrap();
            std::fs::write(root.join("A.md"), "saved 😀\n").unwrap();
            Self(root)
        }
        fn engine(&self) -> Engine {
            Engine::open(&self.0).unwrap()
        }
        fn socket(&self) -> PathBuf {
            self.0.join("cli.sock")
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    struct Running {
        control: ServerControl,
        thread: Option<std::thread::JoinHandle<Option<String>>>,
    }
    impl Running {
        fn start(
            mut server: UnixEngineServer,
            handler: impl FnMut(&mut Engine, CliRequest) -> Vec<u8> + Send + 'static,
        ) -> Self {
            let control = server.control();
            let thread = std::thread::spawn(move || {
                server.run(handler).unwrap();
                // Server drop releases its owned socket before the test returns.
                server.engine().active_file().map(str::to_owned)
            });
            Self {
                control,
                thread: Some(thread),
            }
        }
        fn start_streaming(
            mut server: UnixEngineServer,
            handler: impl FnMut(&mut Engine, ConnectionId, StreamEvent) -> StreamAction + Send + 'static,
        ) -> Self {
            let control = server.control();
            let thread = std::thread::spawn(move || {
                server
                    .run_streaming_with_event_loop(handler, |_| {})
                    .unwrap();
                server.engine().active_file().map(str::to_owned)
            });
            Self {
                control,
                thread: Some(thread),
            }
        }
        fn stop(mut self) -> Option<String> {
            self.control.stop();
            self.thread.take().unwrap().join().unwrap()
        }
    }
    impl Drop for Running {
        fn drop(&mut self) {
            self.control.stop();
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }
    fn request(path: &Path, request: CliRequest) -> Vec<u8> {
        let mut stream = UnixStream::connect(path).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut bytes = serde_json::to_vec(&request).unwrap();
        bytes.push(b'\n');
        // Fragment the header across writes to exercise stream framing.
        for fragment in bytes.chunks(3) {
            stream.write_all(fragment).unwrap();
        }
        let mut output = Vec::new();
        stream.read_to_end(&mut output).unwrap();
        output
    }
    #[test]
    fn native_requests_share_engine_state_and_preserve_argv_and_raw_output() {
        let fixture = Fixture::new();
        let path = fixture.socket();
        let server = UnixEngineServer::bind(&path, fixture.engine()).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let running = Running::start(server, |engine, request| {
            if request.argv[0] == "open" {
                engine
                    .open_file(
                        crate::FileTarget::path("A.md"),
                        crate::OpenOptions::default(),
                    )
                    .unwrap();
                b"Opened: A.md\n".to_vec()
            } else {
                assert_eq!(request.argv, ["read", "quote=\"\\\n\t😀"]);
                assert_eq!(request.cwd, "/a path/😀");
                assert!(!request.tty);
                engine
                    .read_saved(crate::FileTarget::default())
                    .unwrap()
                    .into_bytes()
            }
        });
        assert_eq!(
            request(
                &path,
                CliRequest {
                    argv: vec!["open".into()],
                    tty: false,
                    cwd: "".into()
                }
            ),
            b"Opened: A.md\n"
        );
        assert_eq!(
            request(
                &path,
                CliRequest {
                    argv: vec!["read".into(), "quote=\"\\\n\t😀".into()],
                    tty: false,
                    cwd: "/a path/😀".into()
                }
            ),
            "saved 😀\n".as_bytes()
        );
        let engine = running.stop();
        assert_eq!(engine.as_deref(), Some("A.md"));
        assert!(!path.exists());
    }
    #[test]
    fn malformed_oversize_incomplete_and_disconnected_clients_do_not_kill_service() {
        let fixture = Fixture::new();
        let path = fixture.socket();
        let server = UnixEngineServer::bind_with_config(
            &path,
            fixture.engine(),
            ServerConfig {
                max_request_bytes: 64,
                request_timeout: Duration::from_millis(100),
                output_timeout: None,
            },
        )
        .unwrap();
        let running = Running::start(server, |_, _| vec![0, 255, b'\n']);
        for bytes in [b"not json\n".to_vec(), vec![b'x'; 65], b"{".to_vec()] {
            let mut stream = UnixStream::connect(&path).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            stream.write_all(&bytes).unwrap();
            let mut output = Vec::new();
            let result = stream.read_to_end(&mut output);
            assert!(
                result.is_ok() || result.unwrap_err().kind() == std::io::ErrorKind::ConnectionReset
            );
            assert!(output.is_empty());
        }
        drop(UnixStream::connect(&path).unwrap());
        assert_eq!(
            request(
                &path,
                CliRequest {
                    argv: vec!["ok".into()],
                    tty: true,
                    cwd: "".into()
                }
            ),
            [0, 255, b'\n']
        );
        running.stop();
        assert!(!path.exists());
    }
    #[test]
    fn binding_refuses_existing_paths_and_drop_preserves_replacement() {
        let fixture = Fixture::new();
        let path = fixture.socket();
        std::fs::write(&path, "owned by another process").unwrap();
        assert!(matches!(
            UnixEngineServer::bind(&path, fixture.engine()),
            Err(ServerError::ExistingPath(_))
        ));
        assert_eq!(std::fs::read(&path).unwrap(), b"owned by another process");
        std::fs::remove_file(&path).unwrap();
        let server = UnixEngineServer::bind(&path, fixture.engine()).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, "replacement").unwrap();
        drop(server);
        assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
    }
    #[test]
    fn stop_interrupts_partial_request_and_removes_only_owned_socket() {
        let fixture = Fixture::new();
        let path = fixture.socket();
        let running = Running::start(
            UnixEngineServer::bind(&path, fixture.engine()).unwrap(),
            |_, _| panic!("partial request must not dispatch"),
        );
        let mut stream = UnixStream::connect(&path).unwrap();
        stream.write_all(b"{").unwrap();
        let started = Instant::now();
        running.stop();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(!path.exists());
    }
    #[test]
    fn simultaneous_clients_are_serialized_without_losing_responses() {
        let fixture = Fixture::new();
        let path = fixture.socket();
        let mut count = 0usize;
        let running = Running::start(
            UnixEngineServer::bind(&path, fixture.engine()).unwrap(),
            move |_, request| {
                assert_eq!(request.argv[0], "count");
                count += 1;
                count.to_string().into_bytes()
            },
        );
        let clients: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || {
                    request(
                        &path,
                        CliRequest {
                            argv: vec!["count".into()],
                            tty: false,
                            cwd: "".into(),
                        },
                    )
                })
            })
            .collect();
        let mut counts: Vec<usize> = clients
            .into_iter()
            .map(|client| {
                String::from_utf8(client.join().unwrap())
                    .unwrap()
                    .parse()
                    .unwrap()
            })
            .collect();
        counts.sort_unstable();
        assert_eq!(counts, (1..=8).collect::<Vec<_>>());
        running.stop();
    }
    #[test]
    fn default_native_limits_and_split_utf8_decoding_are_explicit() {
        let config = ServerConfig::default();
        assert_eq!(config.max_request_bytes, usize::MAX);
        assert_eq!(config.request_timeout, Duration::from_secs(5));
        assert_eq!(config.output_timeout, None);
        let (mut client, server) = UnixStream::pair().unwrap();
        let mut connection = Connection::new(server, ConnectionId(1)).unwrap();
        client
            .write_all(b"{\"argv\":[\"read\",\"path=Split-\xc3")
            .unwrap();
        assert!(connection.poll_events(&config).is_empty());
        client
            .write_all(b"\xa9.md\"],\"tty\":false,\"cwd\":\"\"}\n")
            .unwrap();
        let StreamEvent::Request(request) = connection.poll_events(&config).pop_front().unwrap()
        else {
            panic!();
        };
        // Actual native raw header fixture resolves the two replacement units
        // as a different filename; joining raw bytes would incorrectly heal é.
        assert_eq!(request.argv, ["read", "path=Split-��.md"]);
    }
    #[test]
    fn streamed_request_input_eof_and_stop_close_each_connection_once() {
        let fixture = Fixture::new();
        let path = fixture.socket();
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = events.clone();
        let running = Running::start_streaming(
            UnixEngineServer::bind(&path, fixture.engine()).unwrap(),
            move |_, id, event| {
                let name = match &event {
                    StreamEvent::Request(_) => "request",
                    StreamEvent::Input(_) => "input",
                    StreamEvent::Eof => "eof",
                    StreamEvent::Closed => "closed",
                    StreamEvent::Poll => panic!("ordinary connection must not poll completion"),
                };
                seen.lock().unwrap().push((id, name));
                StreamAction {
                    output: match event {
                        StreamEvent::Request(_) => b"ready".to_vec(),
                        StreamEvent::Input(data) => data,
                        StreamEvent::Eof => b"bye".to_vec(),
                        _ => vec![],
                    },
                    ..StreamAction::default()
                }
            },
        );
        let mut incomplete = UnixStream::connect(&path).unwrap();
        incomplete.write_all(b"{").unwrap();
        let mut stream = UnixStream::connect(&path).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream
            .write_all(b"{\"argv\":[],\"tty\":true,\"cwd\":\"\"}\nhello")
            .unwrap();
        let mut bytes = [0; 10];
        stream.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"readyhello");
        stream.shutdown(std::net::Shutdown::Write).unwrap();
        let mut tail = Vec::new();
        stream.read_to_end(&mut tail).unwrap();
        assert_eq!(tail, b"bye");
        running.stop();
        let events = events.lock().unwrap();
        assert_eq!(events.iter().filter(|(_, n)| *n == "request").count(), 1);
        assert_eq!(events.iter().filter(|(_, n)| *n == "input").count(), 1);
        assert_eq!(events.iter().filter(|(_, n)| *n == "eof").count(), 1);
        let mut counts = std::collections::BTreeMap::new();
        for (id, name) in events.iter() {
            if *name == "closed" {
                *counts.entry(*id).or_insert(0) += 1;
            }
        }
        assert_eq!(counts.len(), 2);
        assert!(counts.values().all(|count| *count == 1));
        assert!(!path.exists());
    }
    #[test]
    fn explicit_processing_policy_discards_buffered_input_but_keeps_later_input() {
        let fixture = Fixture::new();
        let path = fixture.socket();
        let (started_send, started_receive) = std::sync::mpsc::channel();
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = events.clone();
        let running = Running::start_streaming(
            UnixEngineServer::bind(&path, fixture.engine()).unwrap(),
            move |_, _, event| match event {
                StreamEvent::Request(_) => StreamAction {
                    output: b"ready".to_vec(),
                    ..Default::default()
                },
                StreamEvent::Input(data) => {
                    seen.lock().unwrap().push(data.clone());
                    if data == b"execute" {
                        started_send.send(()).unwrap();
                        std::thread::sleep(Duration::from_millis(100));
                        StreamAction {
                            output: b"done".to_vec(),
                            input_policy: InputPolicy::DiscardBuffered,
                            ..Default::default()
                        }
                    } else {
                        StreamAction {
                            output: data,
                            ..Default::default()
                        }
                    }
                }
                _ => StreamAction::default(),
            },
        );
        let mut stream = UnixStream::connect(&path).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream
            .write_all(b"{\"argv\":[],\"tty\":true,\"cwd\":\"\"}\n")
            .unwrap();
        let mut ready = [0; 5];
        stream.read_exact(&mut ready).unwrap();
        assert_eq!(&ready, b"ready");
        stream.write_all(b"execute").unwrap();
        started_receive
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        stream.write_all(b"discard while busy").unwrap();
        let mut done = [0; 4];
        stream.read_exact(&mut done).unwrap();
        assert_eq!(&done, b"done");
        stream.write_all(b"next").unwrap();
        stream.read_exact(&mut done).unwrap();
        assert_eq!(&done, b"next");
        running.stop();
        assert_eq!(
            *events.lock().unwrap(),
            vec![b"execute".to_vec(), b"next".to_vec()]
        );
    }
    #[test]
    fn stalled_header_does_not_delay_an_independent_complete_request() {
        let fixture = Fixture::new();
        let path = fixture.socket();
        let running = Running::start(
            UnixEngineServer::bind(&path, fixture.engine()).unwrap(),
            |_, request| {
                assert_eq!(request.argv, ["version"]);
                b"1.14.4 (installer 1.14.4)\n".to_vec()
            },
        );
        let mut held = UnixStream::connect(&path).unwrap();
        held.write_all(b"{").unwrap();
        let started = Instant::now();
        assert_eq!(
            request(
                &path,
                CliRequest {
                    argv: vec!["version".into()],
                    tty: false,
                    cwd: "".into()
                }
            ),
            b"1.14.4 (installer 1.14.4)\n"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "complete client waited for the held header timeout"
        );
        // The independent response must not destroy or time out the first
        // connection. Native held-header capture observes the same distinction.
        held.set_nonblocking(true).unwrap();
        assert_eq!(
            held.read(&mut [0u8; 1]).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        drop(held);
        running.stop();
        assert!(!path.exists());
    }
    #[test]
    fn backpressured_output_does_not_delay_an_independent_complete_request() {
        let fixture = Fixture::new();
        let path = fixture.socket();
        let (sent, received) = std::sync::mpsc::channel();
        let running = Running::start(
            UnixEngineServer::bind(&path, fixture.engine()).unwrap(),
            move |_, request| {
                if request.argv == ["large"] {
                    sent.send(()).unwrap();
                    // Exceeds a Unix socket's send/receive buffer. The first
                    // client never drains this output while the second runs.
                    vec![b'x'; 16 * 1024 * 1024]
                } else {
                    assert_eq!(request.argv, ["version"]);
                    b"1.14.4 (installer 1.14.4)\n".to_vec()
                }
            },
        );
        let mut held = UnixStream::connect(&path).unwrap();
        held.write_all(b"{\"argv\":[\"large\"],\"tty\":false,\"cwd\":\"\"}\n")
            .unwrap();
        received.recv_timeout(Duration::from_secs(1)).unwrap();
        let started = Instant::now();
        assert_eq!(
            request(
                &path,
                CliRequest {
                    argv: vec!["version".into()],
                    tty: false,
                    cwd: "".into()
                }
            ),
            b"1.14.4 (installer 1.14.4)\n"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "complete client waited for a blocked output writer"
        );
        // Actual output began; this is not a test of a request that was ignored.
        held.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let mut byte = [0u8; 1];
        held.read_exact(&mut byte).unwrap();
        assert_eq!(byte, [b'x']);
        drop(held);
        running.stop();
        assert!(!path.exists());
    }
    #[test]
    fn stop_interrupts_stalled_output_reader() {
        let fixture = Fixture::new();
        let path = fixture.socket();
        let (sent, received) = std::sync::mpsc::channel();
        let running = Running::start(
            UnixEngineServer::bind(&path, fixture.engine()).unwrap(),
            move |_, _| {
                sent.send(()).unwrap();
                vec![b'x'; 16 * 1024 * 1024]
            },
        );
        let mut stream = UnixStream::connect(&path).unwrap();
        stream
            .write_all(b"{\"argv\":[],\"tty\":false,\"cwd\":\"\"}\n")
            .unwrap();
        received.recv_timeout(Duration::from_secs(1)).unwrap();
        let started = Instant::now();
        running.stop();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(!path.exists());
    }
}
