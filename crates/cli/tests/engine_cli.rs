#![cfg(unix)]
use mnemosyne_core::CONFIG_DIR;
use mnemosyne_engine::{server::UnixEngineServer, Engine};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = PathBuf::from("/tmp").join(format!(
            "mn-cli-{}-{}-{stamp}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        Self(fs::canonicalize(root).unwrap())
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct OwnedChild(std::process::Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn serve_handles_termination_signals_and_preserves_the_session() {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;
    use std::time::Instant;

    // These exercise the real process signal handler and shutdown sequence;
    // library ServerControl tests cannot establish either behavior.
    for signal in ["TERM", "INT", "HUP"] {
        let scratch = Scratch::new();
        let root = scratch.0.join("vault");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("A.md"), "saved A\n").unwrap();
        let socket = scratch.0.join("app.sock");
        let mut child = OwnedChild(
            Command::new(env!("CARGO_BIN_EXE_mnemosyne"))
                .arg("serve")
                .arg(format!("socket={}", socket.display()))
                .env("MNEMOSYNE_VAULT", &root)
                .env_remove("MNEMOSYNE_SOCKET")
                .env_remove("MNEMOSYNE_PROFILE")
                .current_dir(&scratch.0)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let stderr = child.0.stderr.take().unwrap();
        let (send, receive) = mpsc::channel();
        let reader = thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let _ = send.send(line);
        });
        assert_eq!(
            receive.recv_timeout(Duration::from_secs(5)).unwrap(),
            format!("Engine listening: {}\n", socket.display()),
            "{signal} startup"
        );
        reader.join().unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_mnemosyne"))
            .args(["open", "path=A.md"])
            .env("MNEMOSYNE_SOCKET", &socket)
            .current_dir(&scratch.0)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"Opened: A.md\n");
        assert!(output.stderr.is_empty());
        assert!(Command::new("/bin/kill")
            .arg(format!("-{signal}"))
            .arg(child.0.id().to_string())
            .status()
            .unwrap()
            .success());
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "{signal} shutdown timed out");
            thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(status.code(), Some(0), "{signal} graceful exit");
        assert!(!socket.exists(), "{signal} owned socket cleanup");
        let restored = Engine::open(&root).unwrap();
        assert_eq!(restored.active_file(), Some("A.md"), "{signal} persistence");
        assert_eq!(fs::read_to_string(root.join("A.md")).unwrap(), "saved A\n");
    }
}

#[test]
fn native_style_cli_socket_replays_captured_session_sequence() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../engine/tests/fixtures/reference_session.json"
    ))
    .unwrap();
    let scratch = Scratch::new();
    let root = scratch.0.join("vault");
    fs::create_dir(&root).unwrap();
    for (path, text) in fixture["initial_files"].as_object().unwrap() {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text.as_str().unwrap()).unwrap();
    }
    let mut engine = Engine::open(&root).unwrap();
    let mut initial = engine.session().clone();
    initial.last_open_files = serde_json::from_value(fixture["initial_recents"].clone()).unwrap();
    engine.restore(initial).unwrap();
    let socket = scratch.0.join("app.sock");
    let mut server = UnixEngineServer::bind(&socket, engine).unwrap();
    let control = server.control();
    let (send, receive) = mpsc::channel();
    let worker = thread::spawn(move || {
        server.run_engine(|engine,request| {
            if request.argv.first().map(String::as_str)==Some("__fixture:editor") {
                engine.editor_mut().unwrap().set_value(request.argv[1].clone());
                return b"fixture editor changed\n".to_vec();
            }
            if request.argv.first().map(String::as_str)==Some("__fixture:observe-edit") {
                let path = engine.active_file();
                let ready = engine.session().leaves.iter().filter(|leaf| leaf.file.as_deref() == path)
                    .all(|leaf| engine.editor_for_leaf(&leaf.id).is_some_and(|buffer| buffer.text == request.argv[1]));
                return serde_json::to_vec(&serde_json::json!({"ready":ready})).unwrap();
            }
            let output = mnemosyne::app::execute(engine,&request.argv);
            let leaves:Vec<_>=engine.session().leaves.iter().map(|leaf| serde_json::json!({
                "type":leaf.view_type,"file":leaf.file,
                "text":engine.editor_for_leaf(&leaf.id).map(|b|b.text.as_str())
            })).collect();
            send.send(serde_json::json!({"active":engine.active_file(),"recents":engine.recents(),"leaves":leaves})).unwrap();
            output.into_bytes()
        }).unwrap();
    });
    let outcome = std::panic::catch_unwind(|| {
        for (number, step) in fixture["steps"].as_array().unwrap().iter().enumerate() {
            let args: Vec<String> = if let Some(text) = step["editor_set_value"].as_str() {
                vec!["__fixture:editor".into(), text.into()]
            } else {
                serde_json::from_value(step["args"].clone()).unwrap()
            };
            let output = Command::new(env!("CARGO_BIN_EXE_mnemosyne"))
                .args(&args)
                .env("MNEMOSYNE_SOCKET", &socket)
                .current_dir(&scratch.0)
                .output()
                .unwrap();
            if step.get("args").is_none() {
                // The native fixture captures a separate eval/process turn
                // after editing. Observe the genuine 10ms peer propagation
                // through the normal idle actor, before its 2s disk autosave.
                let deadline = std::time::Instant::now() + Duration::from_millis(500);
                loop {
                    let observed = Command::new(env!("CARGO_BIN_EXE_mnemosyne"))
                        .args([
                            "__fixture:observe-edit",
                            step["editor_set_value"].as_str().unwrap(),
                        ])
                        .env("MNEMOSYNE_SOCKET", &socket)
                        .current_dir(&scratch.0)
                        .output()
                        .unwrap();
                    assert!(observed.status.success());
                    let value: serde_json::Value =
                        serde_json::from_slice(&observed.stdout).unwrap();
                    if value["ready"] == true {
                        break;
                    }
                    assert!(
                        std::time::Instant::now() < deadline,
                        "step {number}: peer editor did not become ready"
                    );
                    std::thread::sleep(Duration::from_millis(2));
                }
                continue;
            }
            assert_eq!(
                output.status.code(),
                Some(step["expected_process"]["exit_code"].as_i64().unwrap() as i32),
                "step {number} exit"
            );
            assert_eq!(
                output.stdout,
                step["expected_process"]["stdout"]
                    .as_str()
                    .unwrap()
                    .as_bytes(),
                "step {number} output"
            );
            assert_eq!(
                output.stderr,
                step["expected_process"]["stderr"]
                    .as_str()
                    .unwrap()
                    .as_bytes(),
                "step {number} stderr"
            );
            assert_eq!(
                receive.recv_timeout(Duration::from_secs(2)).unwrap(),
                step["expected_state"],
                "step {number} state"
            );
        }
        assert_eq!(
            fs::read_to_string(root.join("A.md")).unwrap(),
            "saved A\n",
            "unsaved editor must not overwrite saved reads"
        );
    });
    control.stop();
    worker.join().unwrap();
    assert!(!socket.exists(), "owned socket removed on normal shutdown");
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

#[test]
fn ordinary_search_observes_current_disk_text_without_reindexing() {
    let scratch = Scratch::new();
    let root = scratch.0.join("vault");
    fs::create_dir(&root).unwrap();
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("../../core/tests/fixtures/search.json")).unwrap();
    let mut expected_files = std::collections::BTreeMap::new();
    for step in fixture["freshness"].as_array().unwrap() {
        for (path, text) in step["files"].as_object().unwrap() {
            let bytes = text.as_str().unwrap().as_bytes();
            fs::write(root.join(path), bytes).unwrap();
            expected_files.insert(path.clone(), bytes.to_vec());
        }
        let args: Vec<String> = serde_json::from_value(step["args"].clone()).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_mnemosyne"))
            .args(args)
            .env("MNEMOSYNE_VAULT", &root)
            .env_remove("MNEMOSYNE_SOCKET")
            .current_dir(&scratch.0)
            .output()
            .unwrap();
        assert_eq!(
            output.stdout,
            step["expected"]["stdout"].as_str().unwrap().as_bytes()
        );
        assert_eq!(
            output.stderr,
            step["expected"]["stderr"].as_str().unwrap().as_bytes()
        );
        assert_eq!(
            output.status.code(),
            Some(step["expected"]["exit_code"].as_i64().unwrap() as i32)
        );
    }
    let mut expected_root = expected_files.keys().cloned().collect::<Vec<_>>();
    expected_root.push(".mnemosyne".into());
    expected_root.sort();
    let mut actual_root = fs::read_dir(&root)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect::<Vec<_>>();
    actual_root.sort();
    assert_eq!(
        actual_root, expected_root,
        "ordinary search preserves the user tree and adds only its private cache directory"
    );
    for (path, expected) in expected_files {
        assert_eq!(
            fs::read(root.join(&path)).unwrap(),
            expected,
            "ordinary search preserves final user bytes: {path}"
        );
    }
    let private = root.join(".mnemosyne");
    assert!(fs::symlink_metadata(&private).unwrap().file_type().is_dir());
    let private_entries = fs::read_dir(&private)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        private_entries,
        ["metadata-cache.json"],
        "no unrelated private files or temporary writes remain"
    );
    assert!(fs::symlink_metadata(private.join("metadata-cache.json"))
        .unwrap()
        .file_type()
        .is_file());
}

#[test]
fn saved_reads_match_reference_bom_crlf_empty_and_output_framing() {
    let scratch = Scratch::new();
    let root = scratch.0.join("vault");
    fs::create_dir(&root).unwrap();
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("../../core/tests/fixtures/search.json")).unwrap();
    for (path, text) in fixture["phases"]["4"]["files"].as_object().unwrap() {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text.as_str().unwrap()).unwrap();
    }
    for case in fixture["reads"].as_array().unwrap() {
        let args: Vec<String> = serde_json::from_value(case["args"].clone()).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_mnemosyne"))
            .args(&args)
            .env("MNEMOSYNE_VAULT", &root)
            .env_remove("MNEMOSYNE_SOCKET")
            .current_dir(&scratch.0)
            .output()
            .unwrap();
        assert_eq!(
            output.stdout,
            case["expected"]["stdout"].as_str().unwrap().as_bytes(),
            "{args:?}"
        );
        assert_eq!(
            output.stderr,
            case["expected"]["stderr"].as_str().unwrap().as_bytes()
        );
        assert_eq!(
            output.status.code(),
            Some(case["expected"]["exit_code"].as_i64().unwrap() as i32)
        );
        let mut engine = Engine::open(&root).unwrap();
        assert_eq!(
            mnemosyne::app::execute(&mut engine, &args),
            case["expected"]["stdout"].as_str().unwrap(),
            "retained engine framing"
        );
    }
}

#[test]
fn saved_reads_decode_invalid_utf8_like_the_native_adapter() {
    let scratch = Scratch::new();
    let root = scratch.0.join("vault");
    fs::create_dir(&root).unwrap();
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../engine/tests/fixtures/reference_utf8.json"
    ))
    .unwrap();
    for case in fixture["cases"].as_array().unwrap() {
        let bytes: Vec<u8> = serde_json::from_value(case["bytes"].clone()).unwrap();
        fs::write(root.join(case["path"].as_str().unwrap()), bytes).unwrap();
    }
    let mut engine = Engine::open(&root).unwrap();
    assert_eq!(
        engine.vault.notes.len(),
        fixture["cases"].as_array().unwrap().len()
    );
    for case in fixture["cases"].as_array().unwrap() {
        let args = vec![
            "read".to_owned(),
            format!("path={}", case["path"].as_str().unwrap()),
        ];
        assert_eq!(
            mnemosyne::app::execute(&mut engine, &args),
            case["cli"]["stdout"],
            "{} retained read",
            case["id"]
        );
        let output = Command::new(env!("CARGO_BIN_EXE_mnemosyne"))
            .args(&args)
            .env("MNEMOSYNE_VAULT", &root)
            .env_remove("MNEMOSYNE_SOCKET")
            .current_dir(&scratch.0)
            .output()
            .unwrap();
        assert_eq!(
            output.stdout,
            case["cli"]["stdout"].as_str().unwrap().as_bytes(),
            "{} process read",
            case["id"]
        );
        assert_eq!(
            output.stderr,
            case["cli"]["stderr"].as_str().unwrap().as_bytes()
        );
        assert_eq!(output.status.code(), Some(0));
    }
}

// Process integration of independently captured routing and command contracts.
// This is an authored check, not a whole-host capture.
#[test]
fn profile_server_routes_and_persists_independent_vault_sessions() {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;
    use std::time::Instant;
    let scratch = Scratch::new();
    let a = scratch.0.join("Alpha");
    let b = scratch.0.join("Beta");
    for (root, text) in [(&a, "saved A\n"), (&b, "saved B\n")] {
        fs::create_dir_all(root.join(CONFIG_DIR)).unwrap();
        fs::write(root.join(CONFIG_DIR).join("core-plugins.json"), "{}").unwrap();
        fs::write(root.join("Note.md"), text).unwrap();
    }
    let profile = scratch.0.join("registry.json");
    let document = serde_json::json!({"cli":true,"future":{"preserved":7},"vaults":{
        "a-id":{"path":a,"open":true,"ts":1},
        "b-id":{"path":b,"open":true,"ts":2}
    }});
    fs::write(&profile, serde_json::to_vec(&document).unwrap()).unwrap();
    let socket = scratch.0.join("host.sock");
    let start = || {
        let mut child = OwnedChild(
            Command::new(env!("CARGO_BIN_EXE_mnemosyne"))
                .arg("serve")
                .arg(format!("profile={}", profile.display()))
                .arg(format!("socket={}", socket.display()))
                .env_remove("MNEMOSYNE_SOCKET")
                .env_remove("MNEMOSYNE_PROFILE")
                .current_dir(&scratch.0)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let stderr = child.0.stderr.take().unwrap();
        let (send, receive) = mpsc::channel();
        let reader = thread::spawn(move || {
            let mut line = String::new();
            BufReader::new(stderr).read_line(&mut line).unwrap();
            let _ = send.send(line);
        });
        assert_eq!(
            receive.recv_timeout(Duration::from_secs(5)).unwrap(),
            format!("Engine listening: {}\n", socket.display())
        );
        reader.join().unwrap();
        child
    };
    let request = |cwd: &std::path::Path, args: &[&str], expected: &str| {
        let output = Command::new(env!("CARGO_BIN_EXE_mnemosyne"))
            .args(args)
            .env("MNEMOSYNE_SOCKET", &socket)
            .env_remove("MNEMOSYNE_PROFILE")
            .current_dir(cwd)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(0), "{args:?}");
        assert!(output.stderr.is_empty(), "{args:?}: {:?}", output.stderr);
        assert_eq!(output.stdout, expected.as_bytes(), "{args:?}");
    };
    let stop = |child: &mut OwnedChild| {
        assert!(Command::new("/bin/kill")
            .arg("-TERM")
            .arg(child.0.id().to_string())
            .status()
            .unwrap()
            .success());
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                assert_eq!(status.code(), Some(0));
                break;
            }
            assert!(
                Instant::now() < deadline,
                "profile server shutdown timed out"
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!socket.exists());
    };
    let mut child = start();
    request(
        &scratch.0,
        &["vault=a-id", "open", "path=Note.md"],
        "Opened: Note.md\n",
    );
    request(
        &scratch.0,
        &["vault=beta", "open", "path=Note.md"],
        "Opened: Note.md\n",
    );
    request(&scratch.0, &["vault=a-id", "read"], "saved A\n");
    request(&scratch.0, &["vault=b-id", "read"], "saved B\n");
    request(&a, &["read", "vault=b-id"], "saved A\n");
    request(&b, &["read"], "saved B\n");
    request(&scratch.0, &["vault=unknown", "read"], "Vault not found.\n");
    request(
        &scratch.0,
        &["vault=Alpha", "vault", "info=path"],
        &format!("{}\n", a.display()),
    );
    stop(&mut child);
    let saved: serde_json::Value = serde_json::from_slice(&fs::read(&profile).unwrap()).unwrap();
    assert_eq!(
        saved, document,
        "shutdown must retain open flags and unknown profile fields"
    );
    for root in [&a, &b] {
        assert_eq!(Engine::open(root).unwrap().active_file(), Some("Note.md"));
    }
    // A second process restores both active files from separate saved sessions.
    let mut child = start();
    request(&scratch.0, &["vault=a-id", "read"], "saved A\n");
    request(&scratch.0, &["vault=b-id", "read"], "saved B\n");
    stop(&mut child);
}

#[test]
fn explicit_profile_standalone_preserves_native_disabled_and_unknown_framing() {
    let scratch = Scratch::new();
    let profile = scratch.0.join("registry.json");
    let invoke = |document: serde_json::Value, args: &[&str], expected: &str| {
        fs::write(&profile, serde_json::to_vec(&document).unwrap()).unwrap();
        let result = Command::new(env!("CARGO_BIN_EXE_mnemosyne"))
            .args(args)
            .env("MNEMOSYNE_PROFILE", &profile)
            .env_remove("MNEMOSYNE_SOCKET")
            .current_dir(&scratch.0)
            .output()
            .unwrap();
        assert_eq!(result.status.code(), Some(0));
        assert!(result.stderr.is_empty());
        assert_eq!(result.stdout, expected.as_bytes());
    };
    invoke(
        serde_json::json!({"vaults":{}}),
        &["vault=missing", "read"],
        &format!("{}\n", mnemosyne_engine::vault_registry::CLI_DISABLED),
    );
    invoke(
        serde_json::json!({"cli":true,"vaults":{}}),
        &["vault=missing", "read"],
        "Vault not found.\n",
    );
}
