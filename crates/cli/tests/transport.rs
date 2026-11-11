#![cfg(unix)]
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::{json, Value};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    os::unix::net::UnixListener,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = PathBuf::from("/tmp").join(format!(
            "mn-relay-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        Self(root.canonicalize().unwrap())
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
struct OwnedChild(Option<Child>);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
fn manifest() -> Value {
    serde_json::from_str(include_str!("fixtures/native_transport.json")).unwrap()
}
fn decode(value: &Value) -> Vec<u8> {
    STANDARD.decode(value.as_str().unwrap()).unwrap()
}
fn command(scratch: &Scratch, args: &Value) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_mnemosyne"));
    let args: Vec<String> = serde_json::from_value(args.clone()).unwrap();
    c.args(args)
        .env("MNEMOSYNE_SOCKET", scratch.0.join("app.sock"))
        .current_dir(&scratch.0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    c
}
fn listener(scratch: &Scratch) -> UnixListener {
    let listener = UnixListener::bind(scratch.0.join("app.sock")).unwrap();
    listener.set_nonblocking(true).unwrap();
    listener
}
fn peer(
    listener: UnixListener,
    input: Vec<u8>,
    reply: Vec<u8>,
    signal: bool,
) -> (mpsc::Receiver<Value>, thread::JoinHandle<Value>) {
    let (send, receive) = mpsc::channel();
    let worker = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        let stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "client did not connect");
                    thread::sleep(Duration::from_millis(2));
                }
                Err(e) => panic!("accept: {e}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut stream = BufReader::new(stream);
        let mut raw = Vec::new();
        stream.read_until(b'\n', &mut raw).unwrap();
        let request: Value = serde_json::from_slice(&raw).unwrap();
        send.send(request).unwrap();
        if signal {
            let mut rest = Vec::new();
            stream.read_to_end(&mut rest).unwrap();
            return json!({"closed_after_signal":rest.is_empty()});
        }
        let mut received = vec![0; input.len()];
        stream.read_exact(&mut received).unwrap();
        for chunk in reply.chunks(37) {
            stream.get_mut().write_all(chunk).unwrap();
        }
        json!({"input":STANDARD.encode(received)})
    });
    (receive, worker)
}
#[test]
fn non_tty_relay_matches_captured_native_request_bytes_streams_and_signals() {
    for case in manifest()["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c.get("args").is_some())
    {
        let scratch = Scratch::new();
        let signal = case["signal"].as_i64();
        let (ready, worker) = peer(
            listener(&scratch),
            decode(&case["input_base64"]),
            decode(&case["reply_base64"]),
            signal.is_some(),
        );
        let mut child = OwnedChild(Some(command(&scratch, &case["args"]).spawn().unwrap()));
        let mut stdin = child.0.as_mut().unwrap().stdin.take().unwrap();
        stdin.write_all(&decode(&case["input_base64"])).unwrap();
        drop(stdin);
        let mut request = ready.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(request["cwd"], scratch.0.to_string_lossy().as_ref());
        request["cwd"] = json!("$fixture");
        assert_eq!(
            request, case["reference"]["request"],
            "{} request",
            case["id"]
        );
        if let Some(signal) = signal {
            assert!(Command::new("/bin/kill")
                .arg(format!("-{signal}"))
                .arg(child.0.as_ref().unwrap().id().to_string())
                .status()
                .unwrap()
                .success());
        }
        let output = child.0.take().unwrap().wait_with_output().unwrap();
        let observed = worker.join().unwrap();
        for (k, v) in observed.as_object().unwrap() {
            assert_eq!(v, &case["reference"][k], "{} {k}", case["id"]);
        }
        assert_eq!(
            output.status.code(),
            Some(case["reference"]["exit_code"].as_i64().unwrap() as i32)
        );
        assert_eq!(output.stdout, decode(&case["reference"]["stdout"]));
        assert_eq!(output.stderr, decode(&case["reference"]["stderr"]));
    }
}
#[test]
fn interactive_prompt_without_newline_is_visible_before_input() {
    use std::os::fd::AsRawFd;
    let capture = manifest();
    let expected = &capture["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["id"] == "no-newline-prompt")
        .unwrap()["reference"];
    let scratch = Scratch::new();
    let listener = listener(&scratch);
    // This peer must write before reading: a line-buffered client would deadlock
    // the interaction even though an eventual-output-only assertion could pass.
    let worker = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        let stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline);
                    thread::sleep(Duration::from_millis(2));
                }
                Err(e) => panic!("accept: {e}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut reader = BufReader::new(stream);
        let mut raw = Vec::new();
        reader.read_until(b'\n', &mut raw).unwrap();
        let mut request: Value = serde_json::from_slice(&raw).unwrap();
        request["cwd"] = json!("$fixture");
        reader.get_mut().write_all(b"prompt?").unwrap();
        let mut input = [0; 4];
        reader.read_exact(&mut input).unwrap();
        reader.get_mut().write_all(b"done\n").unwrap();
        (request, input)
    });
    let mut child = OwnedChild(Some(
        command(&scratch, &expected["request"]["argv"])
            .spawn()
            .unwrap(),
    ));
    let mut stdout = child.0.as_mut().unwrap().stdout.take().unwrap();
    let mut readiness = libc::pollfd {
        fd: stdout.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    assert_eq!(
        unsafe { libc::poll(&mut readiness, 1, 3000) },
        1,
        "prompt was buffered while input was pending"
    );
    let mut prompt = [0; 7];
    stdout.read_exact(&mut prompt).unwrap();
    assert_eq!(&prompt, b"prompt?");
    child
        .0
        .as_mut()
        .unwrap()
        .stdin
        .take()
        .unwrap()
        .write_all(b"ack\n")
        .unwrap();
    let output = child.0.take().unwrap().wait_with_output().unwrap();
    let mut received = prompt.to_vec();
    stdout.read_to_end(&mut received).unwrap();
    let (request, input) = worker.join().unwrap();
    assert_eq!(request, expected["request"]);
    assert_eq!(input.as_slice(), decode(&expected["input"]));
    assert_eq!(received, decode(&expected["stdout"]));
    assert_eq!(output.stderr, decode(&expected["stderr"]));
    assert_eq!(output.status.code(), Some(0));
}
#[test]
fn missing_socket_matches_native_startup_streams_and_exit() {
    let scratch = Scratch::new();
    let output = command(&scratch, &json!(["help"])).output().unwrap();
    let expected = &manifest()["startup"];
    assert_eq!(
        output.status.code(),
        Some(expected["exit_code"].as_i64().unwrap() as i32)
    );
    assert_eq!(output.stdout, decode(&expected["stdout"]));
    assert_eq!(output.stderr, decode(&expected["stderr"]));
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn tty_relay_preserves_native_output_processing_and_restores_owned_terminal() {
    use std::os::fd::{AsRawFd, FromRawFd};
    fn attrs(fd: i32) -> (u64, u64, u64, u64, Vec<u8>, u64, u64) {
        let mut storage = std::mem::MaybeUninit::<libc::termios>::uninit();
        assert_eq!(unsafe { libc::tcgetattr(fd, storage.as_mut_ptr()) }, 0);
        let t = unsafe { storage.assume_init() };
        (
            t.c_iflag as u64,
            t.c_oflag as u64,
            t.c_cflag as u64,
            t.c_lflag as u64,
            t.c_cc.to_vec(),
            unsafe { libc::cfgetispeed(&t) } as u64,
            unsafe { libc::cfgetospeed(&t) } as u64,
        )
    }
    for case in manifest()["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["id"].as_str().unwrap().starts_with("tty-"))
    {
        let scratch = Scratch::new();
        let signal = case["signal"].as_i64();
        let (ready, worker) = peer(
            listener(&scratch),
            decode(&case["input_base64"]),
            decode(&case["reply_base64"]),
            signal.is_some(),
        );
        let (mut master_fd, mut slave_fd) = (-1, -1);
        // openpty initializes two owned descriptors with default terminal settings.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master_fd,
                    &mut slave_fd,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0
        );
        let mut master = unsafe { fs::File::from_raw_fd(master_fd) };
        let slave = unsafe { fs::File::from_raw_fd(slave_fd) };
        let saved = attrs(slave.as_raw_fd());
        let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
        assert!(flags >= 0);
        assert_eq!(
            unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) },
            0
        );
        let mut c = command(&scratch, &case["reference"]["request"]["argv"]);
        c.stdin(slave.try_clone().unwrap())
            .stdout(slave.try_clone().unwrap());
        let mut child = OwnedChild(Some(c.spawn().unwrap()));
        let mut request = ready.recv_timeout(Duration::from_secs(5)).unwrap();
        request["cwd"] = json!("$fixture");
        assert_eq!(request, case["reference"]["request"]);
        let deadline = Instant::now() + Duration::from_secs(5);
        while attrs(slave.as_raw_fd()) == saved {
            assert!(Instant::now() < deadline, "raw input mode was not enabled");
            thread::sleep(Duration::from_millis(1));
        }
        if let Some(signal) = signal {
            assert!(Command::new("/bin/kill")
                .arg(format!("-{signal}"))
                .arg(child.0.as_ref().unwrap().id().to_string())
                .status()
                .unwrap()
                .success());
        } else {
            master.write_all(&decode(&case["input_base64"])).unwrap();
        }
        let mut bytes = Vec::new();
        let mut buffer = [0; 4096];
        let status = loop {
            match master.read(&mut buffer) {
                Ok(0) => {}
                Ok(n) => bytes.extend_from_slice(&buffer[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if let Some(status) = child.0.as_mut().unwrap().try_wait().unwrap() {
                        break status;
                    }
                }
                Err(e) => panic!("PTY read: {e}"),
            };
            assert!(Instant::now() < deadline, "TTY relay did not terminate");
            thread::sleep(Duration::from_millis(1));
        };
        let mut stderr = Vec::new();
        child
            .0
            .as_mut()
            .unwrap()
            .stderr
            .take()
            .unwrap()
            .read_to_end(&mut stderr)
            .unwrap();
        let observed = worker.join().unwrap();
        for (k, v) in observed.as_object().unwrap() {
            assert_eq!(v, &case["reference"][k], "{} {k}", case["id"]);
        }
        assert_eq!(status.code(), Some(0));
        assert_eq!(bytes, decode(&case["reference"]["stdout"]));
        assert_eq!(stderr, decode(&case["reference"]["stderr"]));
        assert_eq!(attrs(slave.as_raw_fd()), saved, "{} restore", case["id"]);
    }
}
