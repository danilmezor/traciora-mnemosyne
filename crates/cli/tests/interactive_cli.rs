#![cfg(unix)]
//! Actual binary/socket regressions for terminal ownership and normal routing.
//! Native ANSI drawing is excluded with UI; command body and file effects are
//! asserted independently of the source-level key model tests.
use mnemosyne_engine::vault_registry::{APP_DIR, REGISTRY_FILE};
use serde_json::json;
use std::{
    fs,
    io::{ErrorKind, Read, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = PathBuf::from("/tmp").join(format!(
            "mn-tui-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        Self(root)
    }
    fn fixture(&self) -> (PathBuf, PathBuf, PathBuf) {
        let alpha = self.0.join("Alpha");
        let beta = self.0.join("Beta Space");
        fs::create_dir(&alpha).unwrap();
        fs::create_dir(&beta).unwrap();
        fs::write(alpha.join("Initial.md"), "alpha\n").unwrap();
        fs::write(beta.join("Initial.md"), "beta\n").unwrap();
        let profile = self.0.join("profile.json");
        fs::write(
            &profile,
            serde_json::to_vec(&json!({"cli":true,"vaults":{
                "a":{"path":alpha,"open":true}, "b":{"path":beta,"open":false}
            }}))
            .unwrap(),
        )
        .unwrap();
        (alpha, beta, profile)
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_mnemosyne"));
        command
            .current_dir(&self.0)
            .env("HOME", &self.0)
            .env_remove("MNEMOSYNE_PROFILE")
            .env_remove("MNEMOSYNE_VAULT")
            .env_remove("MNEMOSYNE_SOCKET");
        command
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
struct OwnedChild(Option<Child>);
impl OwnedChild {
    fn stop(&mut self) {
        let child = self.0.as_mut().unwrap();
        assert!(Command::new("/bin/kill")
            .arg("-INT")
            .arg(child.id().to_string())
            .status()
            .unwrap()
            .success());
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            assert!(Instant::now() < deadline, "owned server did not stop");
            thread::sleep(Duration::from_millis(5));
        }
        self.0.take();
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
fn connect(socket: &Path, argv: &[&str], tty: bool, cwd: &Path) -> UnixStream {
    // The socket path exists from bind() on, a moment before the server listens.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut stream = loop {
        match UnixStream::connect(socket) {
            Err(error) if error.kind() == ErrorKind::ConnectionRefused && Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(5))
            }
            result => break result.unwrap(),
        }
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    stream
        .write_all(
            serde_json::to_string(&json!({"argv":argv,"tty":tty,"cwd":cwd}))
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
    stream.write_all(b"\n").unwrap();
    stream
}
fn request(socket: &Path, argv: &[&str], cwd: &Path) -> String {
    let mut stream = connect(socket, argv, false, cwd);
    let mut output = String::new();
    stream.read_to_string(&mut output).unwrap();
    output
}
fn barrier(socket: &Path, cwd: &Path) {
    assert_eq!(
        request(socket, &["version"], cwd),
        format!("{}\n", mnemosyne::version::response())
    );
}
fn enter(stream: &mut UnixStream, line: &[u8], socket: &Path, cwd: &Path, expected: &str) {
    stream.write_all(line).unwrap();
    // A later normal request is a bounded, observable event-loop barrier. It
    // keeps text and Return distinct data events without sleeping for a guess.
    barrier(socket, cwd);
    stream.write_all(b"\r").unwrap();
    let mut output = vec![0; expected.len()];
    stream.read_exact(&mut output).unwrap();
    assert_eq!(output, expected.as_bytes());
}
#[test]
fn actual_binary_retains_two_terminal_actors_and_executes_through_the_selected_engine() {
    let scratch = Scratch::new();
    let (alpha, beta, profile) = scratch.fixture();
    let socket = scratch.0.join("app.sock");
    let child = scratch
        .command()
        .arg("serve")
        .arg(format!("profile={}", profile.display()))
        .arg(format!("socket={}", socket.display()))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut server = OwnedChild(Some(child));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket.exists() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    let mut a = connect(&socket, &[], true, &alpha);
    let mut b = connect(&socket, &[], true, &beta);
    barrier(&socket, &alpha);
    a.write_all(b"create path=\"From A.md\" content=\"hello ")
        .unwrap();
    barrier(&socket, &alpha);
    enter(
        &mut b,
        b"create path=\"From B.md\" content=beta",
        &socket,
        &alpha,
        "Created: From B.md\n",
    );
    assert!(!alpha.join("From A.md").exists());
    enter(
        &mut a,
        "🙂\"".as_bytes(),
        &socket,
        &alpha,
        "Created: From A.md\n",
    );
    assert_eq!(
        fs::read_to_string(alpha.join("From A.md")).unwrap(),
        "hello 🙂"
    );
    assert_eq!(fs::read_to_string(beta.join("From B.md")).unwrap(), "beta");
    enter(
        &mut a,
        b"vault:open \"Beta Space\"",
        &socket,
        &alpha,
        "Opened vault: Beta Space\n",
    );
    enter(&mut a, b"read path=Initial.md", &socket, &alpha, "beta\n");
    enter(
        &mut a,
        b"vault=Alpha read path=Initial.md",
        &socket,
        &alpha,
        "alpha\n",
    );
    // One-command vault selection does not change this terminal's current vault.
    enter(&mut a, b"read path=Initial.md", &socket, &alpha, "beta\n");
    enter(
        &mut b,
        b"vault:open Missing",
        &socket,
        &alpha,
        "Vault not found: Missing\n",
    );
    enter(&mut b, b"read path=Initial.md", &socket, &alpha, "beta\n");
    // History is local to b; Up repeats b's latest read rather than a's selector.
    b.write_all(b"\x1b[A").unwrap();
    barrier(&socket, &alpha);
    b.write_all(b"\r").unwrap();
    let mut output = [0; 5];
    b.read_exact(&mut output).unwrap();
    assert_eq!(&output, b"beta\n");
    a.write_all(b"\x03").unwrap();
    b.write_all(b"\x04").unwrap();
    let mut remainder = Vec::new();
    a.read_to_end(&mut remainder).unwrap();
    assert!(remainder.is_empty());
    b.read_to_end(&mut remainder).unwrap();
    assert!(remainder.is_empty());
    barrier(&socket, &alpha);
    server.stop();
    assert!(!socket.exists());
}
#[test]
fn ordinary_cli_discovers_an_owned_desktop_profile_and_honors_cli_enabled() {
    let scratch = Scratch::new();
    let (alpha, _, profile) = scratch.fixture();
    let desktop = scratch.0.join("Library/Application Support").join(APP_DIR);
    fs::create_dir_all(&desktop).unwrap();
    fs::copy(profile, desktop.join(REGISTRY_FILE)).unwrap();
    let output = scratch
        .command()
        .arg("read")
        .arg("path=Initial.md")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"alpha\n");
    assert!(output.stderr.is_empty());
    let output = scratch
        .command()
        .arg("vault=Missing")
        .arg("read")
        .arg("path=Initial.md")
        .output()
        .unwrap();
    assert_eq!(output.stdout, b"Vault not found.\n");
    fs::write(
        desktop.join(REGISTRY_FILE),
        serde_json::to_vec(&json!({"vaults":{"a":{"path":alpha,"open":true}}})).unwrap(),
    )
    .unwrap();
    let output = scratch.command().arg("version").output().unwrap();
    assert_eq!(
        output.stdout,
        format!("{}\n", mnemosyne_engine::vault_registry::CLI_DISABLED).as_bytes()
    );
}
