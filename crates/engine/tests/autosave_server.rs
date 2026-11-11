//! Integration checks for the real server clock. The 2-second debounce is
//! supported by reference_markdown_timers.json; these are Rust transport tests.
use mnemosyne_engine::{
    host::VaultHost,
    server::{CliRequest, ServerControl, UnixEngineServer, UnixStateServer},
    vault_registry::VaultRegistry,
    Engine, FileTarget, OpenOptions,
};
use serde_json::json;
use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "mnemo-auto-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("A.md"), "saved\n").unwrap();
        Self(root)
    }
    fn socket(&self) -> PathBuf {
        self.0.join("s.sock")
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
struct Running {
    control: ServerControl,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Drop for Running {
    fn drop(&mut self) {
        self.control.stop();
        self.thread.take().unwrap().join().unwrap();
    }
}
fn send(stream: &mut UnixStream, argv: Vec<String>) {
    let mut bytes = serde_json::to_vec(&CliRequest {
        argv,
        tty: false,
        cwd: String::new(),
    })
    .unwrap();
    bytes.push(b'\n');
    stream.write_all(&bytes).unwrap();
}
fn request(socket: &Path, argv: Vec<String>) {
    let mut stream = UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    send(&mut stream, argv);
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"edited");
}
fn wait_saved(path: &Path, expected: &str) {
    let started = Instant::now();
    while std::fs::read_to_string(path).unwrap() != expected {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "autosave did not run without another request"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn start(fixture: &Fixture) -> Running {
    let mut server =
        UnixEngineServer::bind(fixture.socket(), Engine::open(&fixture.0).unwrap()).unwrap();
    let control = server.control();
    let thread = std::thread::spawn(move || {
        server
            .run_engine(|engine, request| {
                engine
                    .open_file(FileTarget::path("A.md"), OpenOptions::default())
                    .unwrap();
                engine.editor_mut().unwrap().set_value("edited\n".into());
                if request.argv.first().map(String::as_str) == Some("large") {
                    vec![b'x'; 16 * 1024 * 1024]
                } else {
                    b"edited".to_vec()
                }
            })
            .unwrap()
    });
    Running {
        control,
        thread: Some(thread),
    }
}
#[test]
fn idle_loop_saves_without_a_followup_request() {
    let fixture = Fixture::new();
    let running = start(&fixture);
    request(&fixture.socket(), vec![]);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(
        std::fs::read_to_string(fixture.0.join("A.md")).unwrap(),
        "saved\n"
    );
    wait_saved(&fixture.0.join("A.md"), "edited\n");
    drop(running);
    assert!(!fixture.socket().exists());
}
#[test]
fn incomplete_client_does_not_delay_editor_autosave() {
    let fixture = Fixture::new();
    let running = start(&fixture);
    request(&fixture.socket(), vec![]);
    let mut held = UnixStream::connect(fixture.socket()).unwrap();
    held.write_all(b"{").unwrap();
    wait_saved(&fixture.0.join("A.md"), "edited\n");
    drop(running);
    assert!(!fixture.socket().exists());
}
#[test]
fn stalled_output_does_not_delay_editor_autosave() {
    let fixture = Fixture::new();
    let running = start(&fixture);
    let mut held = UnixStream::connect(fixture.socket()).unwrap();
    send(&mut held, vec!["large".into()]);
    wait_saved(&fixture.0.join("A.md"), "edited\n");
    drop(running);
    assert!(!fixture.socket().exists());
}
#[test]
fn stopping_before_deadline_leaves_no_background_save() {
    let fixture = Fixture::new();
    let running = start(&fixture);
    request(&fixture.socket(), vec![]);
    drop(running);
    std::thread::sleep(Duration::from_millis(2100));
    assert_eq!(
        std::fs::read_to_string(fixture.0.join("A.md")).unwrap(),
        "saved\n"
    );
    assert!(!fixture.socket().exists());
}
#[test]
fn host_idle_loop_saves_independent_loaded_vaults() {
    let fixture = Fixture::new();
    let other = fixture.0.join("other");
    std::fs::create_dir(&other).unwrap();
    std::fs::write(other.join("A.md"), "other saved\n").unwrap();
    let registry = VaultRegistry::from_document(
        json!({"vaults":{"a":{"path":fixture.0},"b":{"path":other}}}),
        &fixture.0,
        None,
    )
    .unwrap();
    let mut host = VaultHost::from_registry(registry);
    host.open_vault("a").unwrap();
    host.open_vault("b").unwrap();
    let mut server = UnixStateServer::bind(fixture.socket(), host).unwrap();
    let control = server.control();
    let thread = std::thread::spawn(move || {
        server
            .run_host(|host, request| {
                let id = &request.argv[0];
                let engine = host.engine_mut(id).unwrap();
                engine
                    .open_file(FileTarget::path("A.md"), OpenOptions::default())
                    .unwrap();
                engine
                    .editor_mut()
                    .unwrap()
                    .set_value(format!("vault {id}\n"));
                b"edited".to_vec()
            })
            .unwrap()
    });
    let running = Running {
        control,
        thread: Some(thread),
    };
    request(&fixture.socket(), vec!["a".into()]);
    request(&fixture.socket(), vec!["b".into()]);
    wait_saved(&fixture.0.join("A.md"), "vault a\n");
    wait_saved(&other.join("A.md"), "vault b\n");
    drop(running);
}
