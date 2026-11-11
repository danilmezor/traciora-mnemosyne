//! Unix CLI byte relay. The application chooses command behavior; this client
//! forwards argv and terminal input without parsing the response.
#![cfg(unix)]
use std::io::{self, IsTerminal, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::Path;

pub const STARTUP_ERROR: &str =
    "The CLI is unable to find the app. Please make sure the app is running and try again.\n";

#[derive(Debug)]
pub enum RelayError {
    Startup,
    InitialRequest,
}

struct TerminalMode(Option<libc::termios>);
impl TerminalMode {
    fn raw(enabled: bool) -> Self {
        if !enabled {
            return Self(None);
        }
        let mut saved = std::mem::MaybeUninit::<libc::termios>::uninit();
        // fd 0 is process stdin. tcgetattr initializes the complete struct on
        // success; no uninitialized fields are inspected on failure.
        if unsafe { libc::tcgetattr(0, saved.as_mut_ptr()) } != 0 {
            return Self(None);
        }
        let saved = unsafe { saved.assume_init() };
        let mut raw = saved;
        // Both functions receive valid, initialized termios storage.
        unsafe {
            libc::cfmakeraw(&mut raw);
        }
        // Pinned arm64 main 0x1000007c4-0x1000007cc re-enables output
        // processing and NL->CRNL after cfmakeraw; the PTY oracle verifies it.
        raw.c_oflag |= libc::OPOST | libc::ONLCR;
        unsafe {
            libc::tcsetattr(0, libc::TCSAFLUSH, &raw);
        }
        Self(Some(saved))
    }
}
impl Drop for TerminalMode {
    fn drop(&mut self) {
        if let Some(saved) = &self.0 {
            // Restore only the stdin terminal state captured by this relay.
            unsafe {
                libc::tcsetattr(0, libc::TCSAFLUSH, saved);
            }
        }
    }
}

/// Called by the CLI process once. Its stdin reader ends with the process,
/// while socket EOF controls completion, as in the native client's relay.
pub fn relay(socket: &Path, argv: &[String]) -> Result<(), RelayError> {
    let mut stream = UnixStream::connect(socket).map_err(|_| RelayError::Startup)?;
    let tty = io::stdin().is_terminal() && io::stdout().is_terminal();
    let cwd = std::env::current_dir()
        .ok()
        .map(|path| path.to_string_lossy().into_owned())
        .filter(|path| path.len() < 4096)
        .unwrap_or_default();
    let request = mnemosyne_engine::server::CliRequest {
        argv: argv.to_vec(),
        tty,
        cwd,
    };
    let mut bytes = serde_json::to_vec(&request).map_err(|_| RelayError::InitialRequest)?;
    bytes.push(b'\n');
    let signal_stream = stream.try_clone().map_err(|_| RelayError::InitialRequest)?;
    ctrlc::set_handler(move || {
        let _ = signal_stream.shutdown(Shutdown::Both);
    })
    .map_err(|_| RelayError::InitialRequest)?;
    // The development server uses the termination feature's HUP support;
    // the native client only handles INT and TERM and retains default HUP.
    // This changes only the current client process's signal disposition.
    unsafe {
        libc::signal(libc::SIGHUP, libc::SIG_DFL);
    }
    stream
        .write_all(&bytes)
        .map_err(|_| RelayError::InitialRequest)?;
    let _terminal = TerminalMode::raw(tty);
    let mut input_stream = stream.try_clone().map_err(|_| RelayError::InitialRequest)?;
    let _ = std::thread::Builder::new()
        .name("cli-stdin".into())
        .spawn(move || {
            let mut buffer = [0; 4096];
            'input: loop {
                // Match native raw fd reads, without a buffered line reader.
                // The initialized array provides valid writable storage.
                let count = unsafe { libc::read(0, buffer.as_mut_ptr().cast(), buffer.len()) };
                if count <= 0 { break; }
                let mut written = 0;
                while written < count as usize {
                    match input_stream.write(&buffer[written..count as usize]) {
                        Ok(0) | Err(_) => break 'input,
                        Ok(count) => written += count,
                    }
                }
            }
            // Stdin EOF stops only this direction; do not half-close the socket.
        });
    let mut buffer = [0; 4096];
    loop {
        let count = match stream.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(count) => count,
        };
        let mut written = 0;
        while written < count {
            // Native output is unbuffered: prompts without LF must reach the
            // terminal before stdin is supplied. Failed writes abandon this
            // chunk; socket EOF still controls completion.
            let output = unsafe { libc::write(1, buffer[written..count].as_ptr().cast(), count - written) };
            if output <= 0 { break; }
            written += output as usize;
        }
    }
    let _ = stream.shutdown(Shutdown::Both);
    Ok(())
}
