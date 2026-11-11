#![cfg(unix)]
//! Authored regressions for real idle core-config timers, not native event goldens.
use mnemosyne_core::CONFIG_DIR;
use mnemosyne_engine::{
    core_plugins::catalog,
    host::VaultHost,
    server::{ServerControl, UnixEngineServer, UnixStateServer},
    vault_registry::VaultRegistry,
    Engine,
};
use serde_json::{json, Value};
use std::{
    io::Write,
    os::unix::net::UnixStream,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "mn-config-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join(CONFIG_DIR)).unwrap();
        let config: serde_json::Map<String, Value> = catalog()
            .iter()
            .map(|p| (p.id.clone(), json!(false)))
            .collect();
        std::fs::write(
            root.join(CONFIG_DIR).join("core-plugins.json"),
            serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
        Self(root)
    }
    fn socket(&self) -> PathBuf {
        self.0.join("socket")
    }
    fn enabled(&self) -> bool {
        let value: Value = serde_json::from_slice(
            &std::fs::read(self.0.join(CONFIG_DIR).join("core-plugins.json")).unwrap(),
        )
        .unwrap();
        value["templates"] == true
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
struct Running(ServerControl, Option<std::thread::JoinHandle<()>>);
impl Drop for Running {
    fn drop(&mut self) {
        self.0.stop();
        self.1.take().unwrap().join().unwrap();
    }
}
fn assert_idle_save(fixture: &Fixture) {
    let mut incomplete = UnixStream::connect(fixture.socket()).unwrap();
    incomplete.write_all(b"{").unwrap();
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        !fixture.enabled(),
        "source 500ms save was incorrectly immediate"
    );
    let started = Instant::now();
    while !fixture.enabled() {
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "save required another complete CLI request or stalled behind header"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
#[test]
fn engine_idle_core_config_save_runs_while_client_header_is_incomplete() {
    let fixture = Fixture::new();
    let mut engine = Engine::open(&fixture.0).unwrap();
    assert!(engine.enable_core_plugin("templates"));
    let mut server = UnixEngineServer::bind(fixture.socket(), engine).unwrap();
    let control = server.control();
    let running = Running(
        control,
        Some(std::thread::spawn(move || {
            server.run_engine(|_, _| vec![]).unwrap()
        })),
    );
    assert_idle_save(&fixture);
    drop(running);
    assert!(!fixture.socket().exists());
}
#[test]
fn host_idle_core_config_save_runs_without_another_request() {
    let fixture = Fixture::new();
    let registry =
        VaultRegistry::from_document(json!({"vaults":{"a":{"path":fixture.0}}}), &fixture.0, None)
            .unwrap();
    let mut host = VaultHost::from_registry(registry);
    host.open_vault("a").unwrap();
    assert!(host
        .engine_mut("a")
        .unwrap()
        .enable_core_plugin("templates"));
    let mut server = UnixStateServer::bind(fixture.socket(), host).unwrap();
    let control = server.control();
    let running = Running(
        control,
        Some(std::thread::spawn(move || {
            server.run_host(|_, _| vec![]).unwrap()
        })),
    );
    assert_idle_save(&fixture);
    drop(running);
    assert!(!fixture.socket().exists());
}
