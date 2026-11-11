#![cfg(unix)]
//! Real binary, Unix relay and owned upstream renderer. Opt-in external runtime.
use mnemosyne_core::CONFIG_DIR;
use serde_json::{json, Value};
use std::{
    fs,
    io::{BufRead, BufReader},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[ignore = "requires pinned upstream Electron43.7.7, official remote2.1.2 and built Rust addon"]
fn real_cli_server_and_relay_match_all_native_eval_cases() {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let proof = repo.join("target/rea/runtime-eval/electron-node");
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let report = repo.join(format!("target/rea/cli-script/run-{stamp}"));
    fs::create_dir_all(report.join("vault").join(CONFIG_DIR)).unwrap();
    let vault = report.join("vault");
    fs::write(vault.join("ParityReady.md"), "Isolated parity oracle.").unwrap();
    let addon = report.join("addon.node");
    fs::copy(
        repo.join("target/runtime-build/debug/libmnemosyne_runtime_addon.dylib"),
        &addon,
    )
    .unwrap();
    let manifest = report.join("runtime.json");
    // Relative profile/addon paths also exercise manifest-directory resolution.
    fs::write(&manifest, serde_json::to_vec_pretty(&json!({
        "executable":proof.join("dist/Electron.app/Contents/MacOS/Electron"),
        "framework":proof.join("dist/Electron.app/Contents/Frameworks/Electron Framework.framework/Electron Framework"),
        "addon":"addon.node", "profile":"profile",
        "remote_package":proof.join("remote-2.1.2/package")
    })).unwrap()).unwrap();
    let socket = PathBuf::from(format!(
        "/tmp/mn-script-{}-{stamp}.sock",
        std::process::id()
    ));
    let mut server = OwnedChild(
        Command::new(env!("CARGO_BIN_EXE_mnemosyne"))
            .args([
                "serve",
                &format!("socket={}", socket.display()),
                &format!("runtime={}", manifest.display()),
            ])
            .env("MNEMOSYNE_VAULT", &vault)
            .env_remove("MNEMOSYNE_SOCKET")
            .env_remove("MNEMOSYNE_PROFILE")
            .current_dir(&report)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let pid = server.0.id();
    let (sender, receiver) = mpsc::channel();
    let stderr = server.0.stderr.take().unwrap();
    let reader = thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let _ = sender.send(line);
        let mut tail = String::new();
        std::io::Read::read_to_string(&mut reader, &mut tail).unwrap();
        tail
    });
    let ready = receiver.recv_timeout(Duration::from_secs(30)).unwrap();
    let expected_ready = format!("Engine listening: {}\n", socket.display());
    let mut failures = Vec::new();
    let mut rows = Vec::new();
    if ready == expected_ready {
        let fixture: Value =
            serde_json::from_str(include_str!("../../engine/tests/fixtures/cli_eval.json"))
                .unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            let args = case["args"]
                .as_array()
                .unwrap()
                .iter()
                .map(|arg| arg.as_str().unwrap());
            let actual = Command::new(env!("CARGO_BIN_EXE_mnemosyne"))
                .args(args)
                .env("MNEMOSYNE_SOCKET", &socket)
                .env_remove("MNEMOSYNE_PROFILE")
                .current_dir(&vault)
                .output()
                .unwrap();
            let expected = &case["expected"];
            let matches = actual.stdout == expected["stdout"].as_str().unwrap().as_bytes()
                && actual.stderr == expected["stderr"].as_str().unwrap().as_bytes()
                && actual.status.code() == expected["exit_code"].as_i64().map(|code| code as i32);
            if !matches {
                failures.push(case["id"].clone());
            }
            rows.push(json!({"id":case["id"],"stdout":String::from_utf8_lossy(&actual.stdout),"stderr":String::from_utf8_lossy(&actual.stderr),"exit_code":actual.status.code(),"expected":expected,"matches":matches}));
        }
        // Retained eval globals coexist with normal engine CLI file operations.
        for (args, expected) in [
            (vec!["eval", "code=globalThis.cliRetained=41"], "=> 41\n"),
            (
                vec!["open", "path=ParityReady.md"],
                "Opened: ParityReady.md\n",
            ),
            (vec!["read"], "Isolated parity oracle.\n"),
            (vec!["eval", "code=cliRetained+1"], "=> 42\n"),
        ] {
            let actual = Command::new(env!("CARGO_BIN_EXE_mnemosyne"))
                .args(&args)
                .env("MNEMOSYNE_SOCKET", &socket)
                .env_remove("MNEMOSYNE_PROFILE")
                .current_dir(&vault)
                .output()
                .unwrap();
            let matches = actual.stdout == expected.as_bytes()
                && actual.stderr.is_empty()
                && actual.status.success();
            if !matches {
                failures.push(json!(args));
            }
            rows.push(json!({"integration_args":args,"stdout":String::from_utf8_lossy(&actual.stdout),"expected":expected,"matches":matches,"authority":"authored integration sequence"}));
        }
    } else {
        failures.push(json!({"startup":ready}));
    }
    let signal = Command::new("/bin/kill")
        .args(["-TERM", &pid.to_string()])
        .output()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = server.0.try_wait().unwrap() {
            break Some(status);
        }
        if Instant::now() >= deadline {
            let _ = server.0.kill();
            break server.0.wait().ok();
        }
        thread::sleep(Duration::from_millis(10));
    };
    let stderr_tail = reader.join().unwrap();
    let cleanup = fs::read(report.join("profile/stop-receipt.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    let restored = mnemosyne_engine::Engine::open(&vault).unwrap();
    let receipt = json!({"scope":"117 native eval CLI outputs/exit statuses through actual binary, explicit runtime server, common registry and Unix relay;4 additional authored retained-state/file-command steps. Full SDK/CLI service authority, async lifecycle, automatic startup, platforms and mutable Promise boundaries remain required",
        "rows":rows,"failures":failures,"server_pid":pid,"ready":ready,"server_exit_code":status.and_then(|status|status.code()),"signal_sent":signal.status.success(),"stderr_tail":stderr_tail,"runtime_cleanup":cleanup,"socket_absent":!socket.exists(),"restored_active_file":restored.active_file()});
    fs::write(
        report.join("report.json"),
        serde_json::to_vec_pretty(&receipt).unwrap(),
    )
    .unwrap();
    assert!(
        failures.is_empty(),
        "CLI mismatches: {failures:?}; report={}",
        report.display()
    );
    assert_eq!(receipt["server_exit_code"], 0);
    assert_eq!(receipt["runtime_cleanup"]["exit_code"], 0);
    assert_eq!(receipt["runtime_cleanup"]["reaped"], true);
    assert_eq!(receipt["runtime_cleanup"]["process_group_stopped"], true);
    assert!(stderr_tail.is_empty());
    assert!(!socket.exists());
    assert_eq!(restored.active_file(), Some("ParityReady.md"));
}
