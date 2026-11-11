#![cfg(unix)]
//! Four search regressions through actual binary routing. Native captures
//! settle renderer file/source readiness; this does not certify watcher latency,
//! cold asynchronous chronology or the entire search language.
//! The retained route uses the shared development observer host with production
//! socket dispatch and idle hooks, so its barrier reads retained sources.
use mnemosyne_core::CONFIG_DIR;
#[allow(dead_code)] // The shared example main wrapper is unused in the child helper.
#[path = "../examples/observer_server.rs"]
mod observer_host;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
struct Scratch(PathBuf);
impl Scratch {
    fn new(files: &Value) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = PathBuf::from("/tmp").join(format!(
            "mn-search-{}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(path.join("vault").join(CONFIG_DIR)).unwrap();
        for (name, text) in files.as_object().unwrap() {
            let file = path.join("vault").join(name);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, text.as_str().unwrap()).unwrap();
        }
        fs::write(
            path.join("vault").join(CONFIG_DIR).join("app.json"),
            r#"{"trashOption":"local","alwaysUpdateLinks":true}"#,
        )
        .unwrap();
        fs::write(path.join("vault").join(CONFIG_DIR).join("core-plugins.json"),r#"{"global-search":true,"templates":false,"daily-notes":false,"canvas":false,"bases":false}"#).unwrap();
        Self(path)
    }
    fn vault(&self) -> PathBuf {
        self.0.join("vault")
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
struct OwnedServer {
    child: Child,
    socket: PathBuf,
    log: PathBuf,
}
impl OwnedServer {
    fn start(scratch: &Scratch) -> Self {
        let socket = scratch.0.join("app.sock");
        let log = scratch.0.join("observer.jsonl");
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "retained_observer_child", "--nocapture"])
            .env("MNEMOSYNE_P0_OBSERVER_VAULT", scratch.vault())
            .env("MNEMOSYNE_P0_OBSERVER_SOCKET", &socket)
            .env("MNEMOSYNE_P0_OBSERVER_LOG", &log)
            .env("MNEMOSYNE_OBSERVER_SAVED_SOURCES", "1")
            .env("MNEMOSYNE_VAULT", scratch.vault())
            .env_remove("MNEMOSYNE_SOCKET")
            .env_remove("MNEMOSYNE_PROFILE")
            .current_dir(&scratch.0)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stderr = child.stderr.take().unwrap();
        let (send, receive) = mpsc::channel();
        let reader = thread::spawn(move || {
            let mut line = String::new();
            let result = BufReader::new(stderr).read_line(&mut line).map(|_| line);
            let _ = send.send(result);
        });
        let mut server = Self { child, socket, log };
        let ready = receive.recv_timeout(Duration::from_secs(10));
        if ready.is_err() {
            let _ = server.child.kill();
            let _ = server.child.wait();
        }
        reader.join().unwrap();
        assert_eq!(
            ready.unwrap().unwrap(),
            format!("Engine listening: {}\n", server.socket.display())
        );
        server
    }
    fn wait_sources(&self, expected_tree: &Value, id: &str) {
        let expected: BTreeMap<String, String> = expected_tree.as_object().unwrap().iter()
            .map(|(path, bytes)| (path.clone(), String::from_utf8(STANDARD.decode(bytes.as_str().unwrap()).unwrap()).unwrap()))
            .collect();
        let expected_paths: std::collections::BTreeSet<_> = expected.keys().cloned().collect();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut last = Value::Null;
        loop {
            let log = fs::read_to_string(&self.log).unwrap();
            for line in log.lines() {
                // A concurrent append can leave the last line incomplete.
                if let Ok(record) = serde_json::from_str::<Value>(line) {
                    last = record;
                }
            }
            let metadata = &last["state"]["observation"]["metadata"];
            let paths: Option<std::collections::BTreeSet<String>> = serde_json::from_value(metadata["files"].clone()).ok();
            let sources: Option<BTreeMap<String, String>> = serde_json::from_value(metadata["saved_sources"].clone()).ok();
            if paths.as_ref() == Some(&expected_paths) && sources.as_ref() == Some(&expected) {
                return;
            }
            assert!(Instant::now() < deadline, "{id}: retained membership/sources did not converge: paths={paths:?}; sources={sources:?}");
            thread::sleep(Duration::from_millis(10));
        }
    }
    fn stop(&mut self) {
        assert!(Command::new("/bin/kill")
            .arg("-TERM")
            .arg(self.child.id().to_string())
            .status()
            .unwrap()
            .success());
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert_eq!(status.code(), Some(0));
                break;
            }
            assert!(Instant::now() < deadline, "owned server did not stop");
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!self.socket.exists(), "owned socket must be removed");
    }
}
impl Drop for OwnedServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn run(scratch: &Scratch, server: Option<&OwnedServer>, args: &Value) -> Output {
    let args: Vec<String> = serde_json::from_value(args.clone()).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_mnemosyne"));
    command
        .args(args)
        .env("MNEMOSYNE_VAULT", scratch.vault())
        .env_remove("MNEMOSYNE_PROFILE")
        .current_dir(&scratch.0);
    if let Some(server) = server {
        command.env("MNEMOSYNE_SOCKET", &server.socket);
    } else {
        command.env_remove("MNEMOSYNE_SOCKET");
    }
    command.output().unwrap()
}
fn check(output: Output, expected: &Value, id: &str, failures: &mut Vec<Value>) {
    let actual = json!({"stdout":String::from_utf8(output.stdout).unwrap(),"stderr":String::from_utf8(output.stderr).unwrap(),"exit_code":output.status.code()});
    if actual != *expected {
        failures.push(json!({"id":id,"actual":actual,"expected":expected}));
    }
}
fn snapshot(root: &Path) -> Value {
    fn walk(root: &Path, path: &Path, files: &mut BTreeMap<String, String>) {
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name();
            if name.to_string_lossy().starts_with('.') {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                walk(root, &path, files);
            } else {
                files.insert(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                    STANDARD.encode(fs::read(&path).unwrap()),
                );
            }
        }
    }
    let mut files = BTreeMap::new();
    walk(root, root, &mut files);
    json!(files)
}
fn external(root: &Path, action: &Value) {
    match action["kind"].as_str().unwrap() {
        "write" => {
            let p = root.join(action["path"].as_str().unwrap());
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, action["text"].as_str().unwrap()).unwrap();
        }
        "rename" => {
            let to = root.join(action["to"].as_str().unwrap());
            fs::create_dir_all(to.parent().unwrap()).unwrap();
            fs::rename(root.join(action["from"].as_str().unwrap()), to).unwrap();
        }
        "remove" => fs::remove_file(root.join(action["path"].as_str().unwrap())).unwrap(),
        other => panic!("unexpected captured external action {other}"),
    }
}
#[test]
fn retained_observer_child() {
    let Some(vault) = std::env::var_os("MNEMOSYNE_P0_OBSERVER_VAULT") else { return; };
    let args = vec![vault, std::env::var_os("MNEMOSYNE_P0_OBSERVER_SOCKET").unwrap(), std::env::var_os("MNEMOSYNE_P0_OBSERVER_LOG").unwrap()]
        .into_iter().map(|value| value.into_string().unwrap()).collect();
    observer_host::run(args).unwrap();
}
#[test]
fn search_refreshes_after_external_and_command_mutations_and_filters_before_limit() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/search_sequences.json")).unwrap();
    assert_eq!(fixture["identity"]["reference_version"], "1.14.4");
    assert_eq!(fixture["cleanup"]["stopped"], true);
    for persistent in [false, true] {
        let scratch = Scratch::new(&fixture["initial_files"]);
        let mut server = persistent.then(|| OwnedServer::start(&scratch));
        let mut failures = Vec::new();
        for step in fixture["steps"].as_array().unwrap() {
            let id = step["id"].as_str().unwrap();
            if let Some(action) = step.get("external") {
                external(&scratch.vault(), action);
            } else {
                assert!(step["args"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|v| v != "watch" && v != "index"));
                check(
                    run(&scratch, server.as_ref(), &step["args"]),
                    &step["expected"],
                    id,
                    &mut failures,
                );
            }
            if let Some(server) = &server {
                server.wait_sources(&step["tree"], id);
            }
            let actual = snapshot(&scratch.vault());
            if actual != step["tree"] {
                failures.push(json!({"id":id,"file_bytes_actual":actual,"expected":step["tree"]}));
            }
        }
        if let Some(server) = &mut server {
            server.stop();
        }
        assert!(
            failures.is_empty(),
            "persistent={persistent}: {}",
            serde_json::to_string_pretty(&failures).unwrap()
        );
    }
}
#[test]
fn query_case_context_and_absolute_frontmatter_layout_reuse_existing_native_captures() {
    let fixture: Value =
        serde_json::from_str(include_str!("../../core/tests/fixtures/search.json")).unwrap();
    // These captures exercise the same body/phrase/Boolean queries across normal,
    // case and context paths, both text/JSON layouts, YAML absolute lines and
    // CRLF. No native oracle is repeated; no traversal order is injected.
    let ids = [
        "search-000",
        "search-006",
        "search-007",
        "search-009",
        "search-013",
        "search-023",
        "search-034",
        "search-035",
        "search-036",
        "search-037",
        "search-038",
        "search-039",
        "search-045",
        "search-046",
        "search-047",
        "search-051",
        "search-052",
        "search-053",
        "search-054",
        "search-055",
        "search-056",
        "search-059",
        "search-060",
        "search-061",
        "search-062",
        "search-067",
    ];
    for persistent in [false, true] {
        let scratch = Scratch::new(&fixture["phases"]["1"]["files"]);
        let mut server = persistent.then(|| OwnedServer::start(&scratch));
        let mut failures = Vec::new();
        for id in ids {
            let case = fixture["cases"]
                .as_array()
                .unwrap()
                .iter()
                .find(|case| case["id"] == id)
                .unwrap();
            check(
                run(&scratch, server.as_ref(), &case["args"]),
                &case["expected"],
                id,
                &mut failures,
            );
        }
        if let Some(server) = &mut server {
            server.stop();
        }
        assert!(
            failures.is_empty(),
            "persistent={persistent}: {}",
            serde_json::to_string_pretty(&failures).unwrap()
        );
    }
}
