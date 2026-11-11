//! Local trusted-host transport. Socket possession is not actor authentication:
//! credentials are reloaded and checked for every bounded framed request.
use crate::{
    api::{Request, Response},
    auth::Credentials,
    ContentError, Service,
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Write},
    os::unix::{
        fs::{MetadataExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

pub const MAX_REQUEST_BYTES: usize = 128 * 1024 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 256 * 1024 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_CONFIG_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    pub token: String,
    pub request: Request,
}
fn transport_error() -> ContentError {
    ContentError::new("transport_error", "Local content transport failed")
}

pub fn credentials(path: &Path) -> Result<Credentials, ContentError> {
    let file = fs::File::open(path)
        .map_err(|_| ContentError::new("configuration", "Cannot read credentials"))?;
    let mut bytes = Vec::new();
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| transport_error())?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        return Err(ContentError::new(
            "configuration",
            "Credential file too large",
        ));
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| ContentError::new("configuration", "Invalid credentials"))
}
// One deadline covers the entire frame, so a byte-at-a-time sender cannot keep
// the single-flight server occupied indefinitely.
fn read_exact(stream: &mut UnixStream, bytes: &mut [u8], deadline: Instant) -> std::io::Result<()> {
    let mut at = 0;
    while at < bytes.len() {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::TimedOut))?;
        stream.set_read_timeout(Some(remaining))?;
        match stream.read(&mut bytes[at..]) {
            Ok(0) => return Err(std::io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => at += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
fn read_frame(
    stream: &mut UnixStream,
    max: usize,
    timeout: Duration,
) -> Result<Vec<u8>, ContentError> {
    let deadline = Instant::now() + timeout;
    let mut header = [0u8; 4];
    read_exact(stream, &mut header, deadline).map_err(|_| transport_error())?;
    let size = u32::from_be_bytes(header) as usize;
    if size == 0 || size > max {
        return Err(ContentError::new("invalid_request", "Invalid frame length"));
    }
    let mut body = vec![0; size];
    read_exact(stream, &mut body, deadline).map_err(|_| transport_error())?;
    Ok(body)
}
fn write_frame(stream: &mut UnixStream, bytes: &[u8], max: usize) -> Result<(), ContentError> {
    if bytes.len() > max {
        return Err(ContentError::new(
            "response_too_large",
            "Response exceeds transport bound",
        ));
    }
    let deadline = Instant::now() + IO_TIMEOUT;
    let header = (bytes.len() as u32).to_be_bytes();
    for part in [&header[..], bytes] {
        let mut at = 0;
        while at < part.len() {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(transport_error)?;
            stream
                .set_write_timeout(Some(remaining))
                .map_err(|_| transport_error())?;
            match stream.write(&part[at..]) {
                Ok(0) => return Err(transport_error()),
                Ok(n) => at += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return Err(transport_error()),
            }
        }
    }
    Ok(())
}
pub fn call(socket: &Path, token: String, request: Request) -> Result<Response, ContentError> {
    let mut stream = UnixStream::connect(socket).map_err(|_| transport_error())?;
    let bytes = serde_json::to_vec(&Envelope { token, request }).map_err(|_| transport_error())?;
    write_frame(&mut stream, &bytes, MAX_REQUEST_BYTES)?;
    let bytes = read_frame(&mut stream, MAX_RESPONSE_BYTES, Duration::from_secs(120))?;
    serde_json::from_slice(&bytes).map_err(|_| transport_error())
}

pub struct Server {
    listener: UnixListener,
    path: PathBuf,
    inode: u64,
    service: Service,
    credentials: PathBuf,
    // Held for the server's lifetime; dropped after the socket is removed.
    _lock: crate::grant::ServeLock,
}
impl Server {
    /// `lock` is `store`'s `ServeLock`, taken by the caller before binding.
    pub fn bind(
        store: &Path,
        credentials_path: &Path,
        socket: &Path,
        lock: crate::grant::ServeLock,
    ) -> Result<Self, ContentError> {
        credentials(credentials_path)?;
        let parent = socket
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let metadata = fs::symlink_metadata(parent).map_err(|_| transport_error())?;
        if !metadata.is_dir() || metadata.permissions().mode() & 0o077 != 0 {
            return Err(ContentError::new(
                "configuration",
                "Socket parent must be a private host directory",
            ));
        }
        let service = Service::open(store)?;
        // Bind fails on any existing path, including stale sockets and symlinks.
        let listener = UnixListener::bind(socket).map_err(|_| transport_error())?;
        let metadata = fs::symlink_metadata(socket).map_err(|_| transport_error())?;
        let server = Self {
            listener,
            path: socket.into(),
            inode: metadata.ino(),
            service,
            credentials: credentials_path.into(),
            _lock: lock,
        };
        fs::set_permissions(socket, fs::Permissions::from_mode(0o600))
            .map_err(|_| transport_error())?;
        server
            .listener
            .set_nonblocking(true)
            .map_err(|_| transport_error())?;
        Ok(server)
    }
    pub fn run(&self, stop: Arc<AtomicBool>) -> Result<(), ContentError> {
        while !stop.load(Ordering::SeqCst) {
            let (mut stream, _) = match self.listener.accept() {
                Ok(v) => v,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(transport_error()),
            };
            // BSD/macOS can inherit the listener's nonblocking mode. Framed
            // deadlines below require blocking accepted sockets on every Unix.
            stream
                .set_nonblocking(false)
                .map_err(|_| transport_error())?;
            // One connection may issue repeated requests. Idle/slow peers get a
            // fixed frame deadline; shutdown drains at most the current request.
            for _ in 0..16 {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                let body = match read_frame(&mut stream, MAX_REQUEST_BYTES, IO_TIMEOUT) {
                    Ok(body) => body,
                    Err(error) => {
                        let _ = write_frame(
                            &mut stream,
                            &serde_json::to_vec(&Response::failure(error)).unwrap(),
                            MAX_RESPONSE_BYTES,
                        );
                        break;
                    }
                };
                let response = (|| {
                    let envelope: Envelope = serde_json::from_slice(&body).map_err(|_| {
                        ContentError::new("invalid_request", "Invalid transport JSON")
                    })?;
                    let who = credentials(&self.credentials)?.authenticate(&envelope.token)?;
                    Ok(self.service.execute(&who, envelope.request))
                })()
                .unwrap_or_else(Response::failure);
                let mut bytes = serde_json::to_vec(&response).map_err(|_| transport_error())?;
                if bytes.len() > MAX_RESPONSE_BYTES {
                    bytes = serde_json::to_vec(&Response::failure(ContentError::new(
                        "response_too_large",
                        "Response exceeds transport bound",
                    )))
                    .unwrap();
                }
                if write_frame(&mut stream, &bytes, MAX_RESPONSE_BYTES).is_err() {
                    break;
                }
            }
        }
        Ok(())
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.path).is_ok_and(|m| m.ino() == self.inode) {
            let _ = fs::remove_file(&self.path);
        }
    }
}
