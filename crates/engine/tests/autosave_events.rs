//! Native single-view autosave event replay. This does not certify external
//! watcher delivery, multiple views, or plugin event-listener scheduling.
use mnemosyne_engine::{Engine, FileTarget, OpenOptions};
use serde_json::{json, Value};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        loop {
            let path = std::env::temp_dir().join(format!(
                "mn-autosave-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create owned autosave fixture {}: {error}", path.display()),
            }
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn frame(engine: &Engine) -> Value {
    let state = engine.markdown_state().unwrap();
    json!({"mode":state.mode,"source":state.source,"dirty":engine.markdown_is_dirty(),"editor":engine.editor().unwrap().text,"viewData":engine.markdown_view_data(),"lastSavedData":state.last_saved_data()})
}
fn comparable_frame(mut value: Value) -> Value {
    value.as_object_mut().unwrap().shift_remove("label");
    value.as_object_mut().unwrap().shift_remove("elapsed_ms");
    value
}
fn projected_events(engine: &Engine, cursor: u64) -> Value {
    Value::Array(
        engine
            .event_journal_since(cursor)
            .unwrap()
            .iter()
            .map(|event| {
                let mut value = serde_json::to_value(event).unwrap();
                value.as_object_mut().unwrap().shift_remove("sequence");
                value
            })
            .collect(),
    )
}

#[test]
fn native_dirty_source_emits_modify_and_hidden_preview_unchanged_save_emits_none() {
    let native: Value =
        serde_json::from_str(include_str!("fixtures/autosave_events.json")).unwrap();
    assert_eq!(native["identity"]["reference_version"], "1.14.4");
    for case in native["cases"].as_array().unwrap() {
        let fixture = Fixture::new();
        let path = case["path"].as_str().unwrap();
        fs::write(fixture.0.join(path), case["initial_text"].as_str().unwrap()).unwrap();
        let mut engine = Engine::open(&fixture.0).unwrap();
        engine
            .open_file(FileTarget::path(path), OpenOptions::default())
            .unwrap();
        engine
            .set_markdown_state(&json!({"mode":case["mode"],"source":case["source"]}))
            .unwrap();
        let cursor = engine.start_event_journal();
        assert_eq!(
            frame(&engine),
            comparable_frame(case["result"]["frames"][0].clone())
        );
        engine
            .set_editor_value_at(case["editor_text"].as_str().unwrap().into(), 0)
            .unwrap();
        assert_eq!(
            frame(&engine),
            comparable_frame(case["result"]["frames"][1].clone())
        );
        assert!(engine.event_journal_since(cursor).unwrap().is_empty());
        assert!(engine.process_markdown_timers(1999).unwrap().is_empty());
        assert_eq!(
            fs::read_to_string(fixture.0.join(path)).unwrap(),
            case["initial_text"]
        );
        assert!(engine.event_journal_since(cursor).unwrap().is_empty());
        assert_eq!(engine.process_markdown_timers(2000).unwrap().len(), 1);
        assert_eq!(case["result"]["timed_out"], false);
        assert_eq!(
            frame(&engine),
            comparable_frame(case["result"]["frames"][3].clone())
        );
        let expected = case["result"]["events"]
            .as_array()
            .unwrap()
            .iter()
            .cloned()
            .map(|mut event| {
                event.as_object_mut().unwrap().shift_remove("elapsed_ms");
                event
            })
            .collect::<Vec<_>>();
        assert_eq!(
            projected_events(&engine, cursor),
            Value::Array(expected),
            "{} callback projection",
            case["id"]
        );
        assert_eq!(
            fs::read(fixture.0.join(path)).unwrap(),
            serde_json::from_value::<Vec<u8>>(case["post_eval_file_bytes"].clone()).unwrap()
        );
        assert_eq!(
            engine.read_saved(FileTarget::default()).unwrap(),
            case["result"]["saved"]
        );
        engine.save_active().unwrap();
        assert!(engine.process_markdown_timers(4000).unwrap().is_empty());
        assert_eq!(
            projected_events(&engine, cursor),
            {
                let mut expected = case["result"]["events"].clone();
                for event in expected.as_array_mut().unwrap() {
                    event.as_object_mut().unwrap().shift_remove("elapsed_ms");
                }
                expected
            },
            "unchanged repeated save must not emit another callback"
        );
    }
}

#[test]
fn failed_source_save_emits_no_successful_modify() {
    // Authored adapter failure injection, not a native IO-failure golden.
    let fixture = Fixture::new();
    fs::write(fixture.0.join("Note.md"), "old saved\n").unwrap();
    let mut engine = Engine::open(&fixture.0).unwrap();
    engine
        .open_file(FileTarget::path("Note.md"), OpenOptions::default())
        .unwrap();
    engine.set_editor_value_at("new dirty\n".into(), 0).unwrap();
    let cursor = engine.start_event_journal();
    fs::remove_file(fixture.0.join("Note.md")).unwrap();
    fs::create_dir(fixture.0.join("Note.md")).unwrap();
    assert!(engine.process_markdown_timers(2000).is_err());
    assert!(engine.event_journal_since(cursor).unwrap().is_empty());
    assert_eq!(
        engine.markdown_state().unwrap().last_saved_data(),
        "old saved\n"
    );
    assert_eq!(
        engine.markdown_state().unwrap().reading_data(),
        "new dirty\n",
        "source failure rolls back saved baseline, not the new view data"
    );
    assert_eq!(engine.editor().unwrap().saved_text, "old saved\n");
}

#[cfg(unix)]
#[test]
fn real_idle_server_emits_autosave_modify_without_a_followup_command() {
    use mnemosyne_engine::server::{CliRequest, ServerControl, UnixEngineServer};
    use std::{
        io::{Read, Write},
        os::unix::net::UnixStream,
        time::{Duration, Instant},
    };
    struct Running {
        control: ServerControl,
        thread: Option<std::thread::JoinHandle<Value>>,
    }
    impl Drop for Running {
        fn drop(&mut self) {
            self.control.stop();
            if let Some(thread) = self.thread.take() {
                thread.join().unwrap();
            }
        }
    }
    let fixture = Fixture::new();
    fs::write(fixture.0.join("Note.md"), "old saved\n").unwrap();
    let socket = fixture.0.join("engine.sock");
    let mut engine = Engine::open(&fixture.0).unwrap();
    let cursor = engine.start_event_journal();
    let mut server = UnixEngineServer::bind(&socket, engine).unwrap();
    let control = server.control();
    let thread = std::thread::spawn(move || {
        server
            .run_engine(|engine, _| {
                engine
                    .open_file(FileTarget::path("Note.md"), OpenOptions::default())
                    .unwrap();
                engine.editor_mut().unwrap().set_value("new saved\n".into());
                b"edited".to_vec()
            })
            .unwrap();
        projected_events(server.state(), cursor)
    });
    let mut running = Running {
        control,
        thread: Some(thread),
    };
    let mut client = UnixStream::connect(&socket).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut request = serde_json::to_vec(&CliRequest {
        argv: vec![],
        tty: false,
        cwd: String::new(),
    })
    .unwrap();
    request.push(b'\n');
    client.write_all(&request).unwrap();
    let mut bytes = vec![];
    client.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"edited");
    // Disk is observed without making another request that could drive timers.
    let deadline = Instant::now() + Duration::from_secs(5);
    while fs::read_to_string(fixture.0.join("Note.md")).unwrap() != "new saved\n" {
        assert!(Instant::now() < deadline, "idle autosave did not run");
        std::thread::sleep(Duration::from_millis(10));
    }
    running.control.stop();
    let events = running.thread.take().unwrap().join().unwrap();
    assert_eq!(
        events,
        json!([{"name":"modify","path":"Note.md","kind":"file"}])
    );
    assert!(!socket.exists());
}
