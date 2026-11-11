#![cfg(unix)]
//! Replay only the safe actual native headers; never send malformed GUI-alert packets.
use mnemosyne_core::CONFIG_DIR;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use mnemosyne_engine::{
    server::{ServerControl, UnixEngineServer},
    Engine,
};
use serde_json::Value;
use std::{
    fs,
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::Duration,
};
struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
struct Running {
    control: ServerControl,
    thread: Option<thread::JoinHandle<()>>,
}
impl Drop for Running {
    fn drop(&mut self) {
        self.control.stop();
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}
#[test]
fn safe_actual_native_header_output_matches_real_retained_cli_dispatch() {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let scratch = Scratch(PathBuf::from(format!(
        "/tmp/mn-head-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )));
    fs::create_dir_all(scratch.0.join(CONFIG_DIR)).unwrap();
    let flags: serde_json::Map<String, Value> = mnemosyne_engine::core_plugins::catalog()
        .iter()
        .map(|plugin| (plugin.id.clone(), Value::Bool(false)))
        .collect();
    fs::write(
        scratch.0.join(CONFIG_DIR).join("core-plugins.json"),
        serde_json::to_vec(&flags).unwrap(),
    )
    .unwrap();
    fs::write(scratch.0.join("Initial.md"), "Initial header fixture.\n").unwrap();
    fs::write(scratch.0.join("Split-é.md"), "Unicode target.\n").unwrap();
    let socket = scratch.0.join("app.sock");
    let mut server = UnixEngineServer::bind(&socket, Engine::open(&scratch.0).unwrap()).unwrap();
    let control = server.control();
    let running = Running {
        control,
        thread: Some(thread::spawn(move || {
            server
                .run(|engine, request| mnemosyne::app::execute(engine, &request.argv).into_bytes())
                .unwrap();
        })),
    };
    let fixture: Value = serde_json::from_str(include_str!(
        "../../engine/tests/fixtures/transport.json"
    ))
    .unwrap();
    for case in fixture["headers"].as_array().unwrap() {
        if case["id"] == "absolute-header-timeout" {
            continue;
        }
        let mut stream = UnixStream::connect(&socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let chunks = case["chunks_base64"].as_array().unwrap();
        for (index, chunk) in chunks.iter().enumerate() {
            stream
                .write_all(&STANDARD.decode(chunk.as_str().unwrap()).unwrap())
                .unwrap();
            if let Some(delay) = case["delays_seconds"].get(index).and_then(Value::as_f64) {
                thread::sleep(Duration::from_secs_f64(delay));
            }
        }
        let mut observed = Vec::new();
        stream.read_to_end(&mut observed).unwrap();
        assert_eq!(
            observed,
            STANDARD
                .decode(case["stdout_base64"].as_str().unwrap())
                .unwrap(),
            "{}",
            case["id"]
        );
    }
    // Independently authored constructor specified in the portable native fixture.
    let ignored = format!(
        "ignored={}{}",
        "é😀".repeat(2048),
        (1u8..32).map(char::from).collect::<String>()
    );
    let header =
        serde_json::json!({"argv":["read","path=Initial.md",ignored],"tty":false,"cwd":""});
    let mut frame = serde_json::to_vec(&header).unwrap();
    frame.push(b'\n');
    assert!(frame.len() > 4096);
    let mut stream = UnixStream::connect(&socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream.write_all(&frame).unwrap();
    let mut observed = Vec::new();
    stream.read_to_end(&mut observed).unwrap();
    assert_eq!(
        observed,
        STANDARD
            .decode(
                fixture["long_header"]["native_response_base64"]
                    .as_str()
                    .unwrap()
            )
            .unwrap()
    );
    drop(running);
    assert!(!socket.exists());
}
