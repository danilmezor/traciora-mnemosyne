use mnemosyne_core::CONFIG_DIR;
use mnemosyne_engine::{
    command_registry::CommandSpec,
    core_plugins,
    host::{BackendReadiness, CliQueueResult, CliReadiness, HostEvent, VaultHost, VaultPhase},
    vault_registry::{CliDispatchAction, VaultRegistry},
    Engine, FileTarget, OpenOptions,
};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "mnemosyne-host-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        Self(root)
    }
    fn seed(&self, path: &str, heading: &str) -> PathBuf {
        let root = self.0.join(path);
        std::fs::create_dir_all(root.join(CONFIG_DIR)).unwrap();
        let flags: serde_json::Map<String, Value> = core_plugins::catalog()
            .iter()
            .map(|p| (p.id.clone(), json!(false)))
            .collect();
        std::fs::write(
            root.join(CONFIG_DIR).join("core-plugins.json"),
            serde_json::to_vec(&flags).unwrap(),
        )
        .unwrap();
        std::fs::write(root.join("Note.md"), format!("# {heading}\n")).unwrap();
        root
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn open_note(engine: &mut Engine) {
    engine
        .open_file(FileTarget::path("Note.md"), OpenOptions::default())
        .unwrap();
}

#[test]
fn actual_multivault_routing_keeps_independent_buffers_cache_and_commands() {
    let fixture = Fixture::new();
    let source = include_str!("fixtures/reference_vault_registry.json")
        .replace("$RUN", fixture.0.to_str().unwrap());
    let receipt: Value = serde_json::from_str(&source).unwrap();
    fixture.seed("vault", "A");
    fixture.seed("vault/Inner", "Nested");
    fixture.seed("area/vault", "B");
    std::fs::write(fixture.0.join("vault/ParityReady.md"), "owned").unwrap();
    let registry =
        VaultRegistry::from_document(receipt["profile_after_open"].clone(), &fixture.0, None)
            .unwrap();
    let mut host = VaultHost::from_registry(registry);
    assert!(host
        .restore_open_vaults()
        .iter()
        .all(|(_, result)| result.is_ok()));
    for row in receipt["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row.get("selected").is_some())
    {
        let argv: Vec<String> = row["argv"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap().into())
            .collect();
        let plan = host.plan_cli_request(&argv, false, Path::new(row["cwd"].as_str().unwrap()));
        let CliDispatchAction::Execute {
            vault_id: Some(id), ..
        } = plan.action
        else {
            panic!("{}", row["id"]);
        };
        assert_eq!(id, row["selected"]["id"].as_str().unwrap());
        assert_eq!(
            host.open_vault(&id).unwrap().vault.root.to_str(),
            row["selected"]["path"].as_str()
        );
    }
    let base = "f1ea000000000001";
    let other = receipt["registrations"][1]["id"].as_str().unwrap();
    let engine = host.open_vault(base).unwrap();
    open_note(engine);
    engine.editor_mut().unwrap().set_value("unsaved A".into());
    engine
        .commands
        .register(
            "only-a",
            CommandSpec::new("only A", None),
            "a-handler".into(),
        )
        .unwrap();
    let engine = host.open_vault(other).unwrap();
    open_note(engine);
    engine.editor_mut().unwrap().set_value("unsaved B".into());
    assert!(engine.commands.get("only-a").is_none());
    assert_eq!(
        engine.file_cache("Note.md").unwrap().headings[0].heading,
        "B"
    );
    assert_eq!(
        host.engine(base).unwrap().editor().unwrap().text,
        "unsaved A"
    );
    assert_eq!(
        host.engine(other).unwrap().editor().unwrap().text,
        "unsaved B"
    );
    assert_eq!(
        host.engine(base)
            .unwrap()
            .file_cache("Note.md")
            .unwrap()
            .headings[0]
            .heading,
        "A"
    );
    assert_eq!(
        host.engine(base)
            .unwrap()
            .commands
            .get("only-a")
            .unwrap()
            .handler,
        "a-handler"
    );
    host.close_vault(base).unwrap();
    assert!(host.engine(base).is_none());
    assert_eq!(
        host.engine(other).unwrap().editor().unwrap().text,
        "unsaved B"
    );
    assert_eq!(
        host.open_vault(base).unwrap().editor().unwrap().text,
        "unsaved A"
    );
    let reopened = host.engine(base).unwrap();
    assert_eq!(
        std::fs::read_to_string(reopened.vault.root.join("Note.md")).unwrap(),
        "unsaved A"
    );
    assert!(!reopened.editor().unwrap().is_dirty());
    assert!(!host.set_cli_enabled(false));
    assert!(!host.registry().document().contains_key("cli"));
    assert_eq!(
        host.registry().document().get("updateDisabled"),
        receipt["profile_after_disable"].get("updateDisabled")
    );
    assert!(host.set_cli_enabled(true));
}

#[test]
fn source_readiness_fifo_cancellation_and_backend_pending_are_explicit() {
    let fixture = Fixture::new();
    let vault = fixture.seed("A", "A");
    std::fs::write(
        vault.join(CONFIG_DIR).join("core-plugins.json"),
        r#"{"daily-notes":true}"#,
    )
    .unwrap();
    let registry = VaultRegistry::from_document(
        json!({"cli":true,"vaults":{"a":{"path":vault}}}),
        &fixture.0,
        None,
    )
    .unwrap();
    let mut host = VaultHost::from_registry(registry);
    assert_eq!(
        host.enqueue_cli("a", vec!["one".into()]).unwrap(),
        CliQueueResult::Queued { ticket: 1 }
    );
    assert_eq!(
        host.enqueue_cli("a", vec!["two".into()]).unwrap(),
        CliQueueResult::Queued { ticket: 2 }
    );
    let readiness = host.readiness("a").unwrap();
    assert_eq!(readiness.phase, VaultPhase::EngineLoaded);
    assert_eq!(readiness.cli, CliReadiness::Pending);
    assert!(readiness.metadata_snapshot_loaded);
    assert_eq!(
        readiness.plugin_backends["daily-notes"],
        BackendReadiness::Pending
    );
    host.mark_handler_installed("a").unwrap();
    assert_eq!(
        host.readiness("a").unwrap().cli,
        CliReadiness::HandlerInstalled
    );
    assert!(!host.readiness("a").unwrap().layout_ready);
    assert!(
        matches!(host.enqueue_cli("a",vec!["after-install-before-layout".into()]).unwrap(),CliQueueResult::Dispatch(request) if request.ticket==3)
    );
    let queued = host.mark_dispatch_ready("a").unwrap();
    assert_eq!(queued.iter().map(|r| r.ticket).collect::<Vec<_>>(), [1, 2]);
    assert_eq!(queued[1].argv, ["two"]);
    assert!(host.mark_dispatch_ready("a").unwrap().is_empty());
    assert!(
        matches!(host.enqueue_cli("a",vec!["three".into()]).unwrap(),CliQueueResult::Dispatch(request) if request.ticket==4)
    );
    assert!(host.record_focus("a", 100));
    assert!(!host.record_focus("missing", 101));
    host.close_vault("a").unwrap();
    assert_eq!(host.readiness("a").unwrap().phase, VaultPhase::Closed);
    host.enqueue_cli("a", vec!["four".into()]).unwrap();
    host.close_vault("a").unwrap();
    let events = host.drain_events();
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, HostEvent::DispatchReady { .. }))
            .count(),
        1
    );
    assert!(events.contains(&HostEvent::CliCancelled {
        vault_id: "a".into(),
        ticket: 5
    }));
    assert!(host.drain_events().is_empty());
}

#[test]
fn source_compact_registry_persistence_preserves_unknowns_and_swallows_write_error() {
    let fixture = Fixture::new();
    let a = fixture.seed("A", "A");
    let b = fixture.seed("B", "B");
    let profile = fixture.0.join("registry.json");
    let document = json!({"cli":true,"unknown":{"future":[1,2]},"vaults":{"a":{"path":a,"ts":7,"extra":"a"},"b":{"path":b,"ts":8}}});
    std::fs::write(&profile, serde_json::to_vec(&document).unwrap()).unwrap();
    let mut host = VaultHost::from_profile(&profile, &fixture.0).unwrap();
    host.open_vault("a").unwrap();
    let bytes = std::fs::read(&profile).unwrap();
    assert!(!bytes.ends_with(b"\n"));
    let saved: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(saved["unknown"], document["unknown"]);
    assert_eq!(saved["vaults"]["a"]["ts"], 7);
    assert_eq!(saved["vaults"]["a"]["extra"], "a");
    assert_eq!(saved["vaults"]["a"]["open"], true);
    assert_eq!(
        bytes,
        serde_json::to_vec(host.registry().document()).unwrap()
    );
    assert!(host
        .persist_sessions()
        .into_iter()
        .all(|(_, result)| result.is_ok()));
    assert_eq!(
        std::fs::read(&profile).unwrap(),
        bytes,
        "shutdown export preserves startup-open flag and compact profile"
    );
    std::fs::remove_file(&profile).unwrap();
    std::fs::create_dir(&profile).unwrap();
    host.open_vault("b").unwrap();
    assert!(host.engine("b").is_some());
    assert!(host
        .registry()
        .diagnostics
        .iter()
        .any(|s| s.starts_with("Ignored registry write:")));
    assert_eq!(
        host.registry().registration("b").unwrap().metadata["open"],
        true
    );
}

#[test]
fn failed_vault_open_has_failed_state_without_a_ready_engine() {
    let fixture = Fixture::new();
    let file = fixture.0.join("File");
    std::fs::write(&file, b"not a directory").unwrap();
    let registry = VaultRegistry::from_document(
        json!({"cli":true,"vaults":{"bad":{"path":file}}}),
        &fixture.0,
        None,
    )
    .unwrap();
    let mut host = VaultHost::from_registry(registry);
    assert!(host.open_vault("bad").is_err());
    assert_eq!(host.readiness("bad").unwrap().phase, VaultPhase::Failed);
    assert!(host.engine("bad").is_none());
    assert!(host
        .drain_events()
        .iter()
        .any(|e| matches!(e,HostEvent::OpenFailed{vault_id,..} if vault_id=="bad")));
    assert_eq!(
        host.open_vault("missing").unwrap_err().to_string(),
        "Vault not found."
    );
}

#[cfg(unix)]
#[test]
fn generic_socket_keeps_two_vault_engines_across_native_request_frames() {
    use mnemosyne_engine::server::{CliRequest, ServerControl, UnixStateServer};
    use std::{
        io::{Read, Write},
        os::unix::net::UnixStream,
        time::Duration,
    };
    let fixture = Fixture::new();
    let a = fixture.seed("A", "A");
    let b = fixture.seed("B", "B");
    let registry = VaultRegistry::from_document(
        json!({"cli":true,"vaults":{"a":{"path":a},"b":{"path":b}}}),
        &fixture.0,
        None,
    )
    .unwrap();
    let path = fixture.0.join("host.sock");
    let mut server = UnixStateServer::bind(&path, VaultHost::from_registry(registry)).unwrap();
    struct Running {
        control: ServerControl,
        thread: Option<std::thread::JoinHandle<()>>,
    }
    impl Drop for Running {
        fn drop(&mut self) {
            self.control.stop();
            if let Some(thread) = self.thread.take() {
                thread.join().unwrap();
            }
        }
    }
    let running = Running {
        control: server.control(),
        thread: Some(std::thread::spawn(move || {
            server
                .run(|host, request| {
                    let plan =
                        host.plan_cli_request(&request.argv, request.tty, Path::new(&request.cwd));
                    let CliDispatchAction::Execute {
                        vault_id: Some(id),
                        argv,
                    } = plan.action
                    else {
                        panic!("unexpected adapter request");
                    };
                    host.open_vault(&id).unwrap();
                    host.mark_dispatch_ready(&id).unwrap();
                    let engine = host.engine_mut(&id).unwrap();
                    if argv[0] == "dirty" {
                        open_note(engine);
                        engine.editor_mut().unwrap().set_value(argv[1].clone());
                        b"ok\n".to_vec()
                    } else {
                        engine.editor().unwrap().text.as_bytes().to_vec()
                    }
                })
                .unwrap();
        })),
    };
    let request = |id: &str, args: Vec<&str>| {
        let mut stream = UnixStream::connect(&path).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let request = CliRequest {
            argv: std::iter::once(format!("vault={id}"))
                .chain(args.into_iter().map(str::to_owned))
                .collect(),
            tty: false,
            cwd: fixture.0.to_string_lossy().into_owned(),
        };
        let mut frame = serde_json::to_vec(&request).unwrap();
        frame.push(b'\n');
        stream.write_all(&frame).unwrap();
        let mut output = Vec::new();
        stream.read_to_end(&mut output).unwrap();
        output
    };
    assert_eq!(request("a", vec!["dirty", "one"]), b"ok\n");
    assert_eq!(request("b", vec!["dirty", "two"]), b"ok\n");
    assert_eq!(request("a", vec!["buffer"]), b"one");
    assert_eq!(request("b", vec!["buffer"]), b"two");
    drop(running);
    assert!(!path.exists());
}
