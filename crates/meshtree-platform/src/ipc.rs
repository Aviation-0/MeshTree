//! Daemon IPC: newline-delimited JSON over a Unix domain socket (macOS /
//! Linux) or a named pipe (Windows).
//!
//! Authoritative specification: `SPEC.md` §10 (IPC table, request/response
//! shape), §16 (`MT-010`), §17 (socket `0600` + parent directory check;
//! per-user named-pipe ACL; no TCP listener, ever).
//!
//! | | macOS / Linux | Windows |
//! |---|---|---|
//! | Endpoint | `<repo>/.meshtree/daemon.sock` | `\\.\pipe\meshtree-<repo-hash-16>` |
//! | Access | socket `0600`; parent owned by us, not group/world-writable | DACL: owner + SYSTEM only; remote clients rejected |
//!
//! One request per line, one response per line, in order, per connection.
//! A connection that sends a line that is not a valid request is closed.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

/// Error code for an unreachable daemon per `SPEC.md` §16.
pub const ERROR_CODE_DAEMON_UNREACHABLE: &str = "MT-010";

/// `{"id": 7, "method": "claim_paths", "params": {…}}`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// Caller-chosen id echoed in the response.
    pub id: u64,
    /// Method name (e.g. `claim_paths`).
    pub method: String,
    /// Method parameters.
    #[serde(default)]
    pub params: serde_json::Value,
}

/// `{"id": 7, "ok": true, "result": {…}}` or
/// `{"id": 7, "ok": false, "error": {"code": "MT-001", "message": "…"}}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    /// Id of the request answered.
    pub id: u64,
    /// `true` iff `result` is set.
    pub ok: bool,
    /// Success payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    /// Failure payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorBody>,
}

/// `{"code": "MT-001", "message": "…"}` (`SPEC.md` §16 codes).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    /// MeshTree error code.
    pub code: String,
    /// Human-readable detail.
    pub message: String,
}

impl Response {
    /// A success response.
    pub fn ok(id: u64, result: serde_json::Value) -> Self {
        Self {
            id,
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    /// A failure response.
    pub fn err(id: u64, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            id,
            ok: false,
            result: None,
            error: Some(ErrorBody {
                code: code.into(),
                message: message.into(),
            }),
        }
    }
}

/// IPC failure.
#[derive(Debug)]
pub enum IpcError {
    /// No daemon is listening (`MT-010`).
    Unreachable {
        /// Endpoint tried.
        endpoint: String,
        /// OS error.
        source: io::Error,
    },
    /// Any other I/O or protocol failure.
    Io(io::Error),
}

impl IpcError {
    /// MeshTree error code (`SPEC.md` §16), if any.
    pub fn code(&self) -> Option<&'static str> {
        match self {
            IpcError::Unreachable { .. } => Some(ERROR_CODE_DAEMON_UNREACHABLE),
            IpcError::Io(_) => None,
        }
    }
}

impl fmt::Display for IpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IpcError::Unreachable { endpoint, source } => write!(
                f,
                "[{ERROR_CODE_DAEMON_UNREACHABLE}] daemon unreachable at {endpoint} ({source}); \
                 is `meshtree daemon` running?"
            ),
            IpcError::Io(err) => write!(f, "ipc: {err}"),
        }
    }
}

impl std::error::Error for IpcError {}

impl From<io::Error> for IpcError {
    fn from(err: io::Error) -> Self {
        IpcError::Io(err)
    }
}

/// Where a repository's daemon listens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    path: PathBuf,
}

impl Endpoint {
    /// The endpoint for the repository at `repo_root` (§10 IPC table).
    ///
    /// Windows: `\\.\pipe\meshtree-<first 16 hex of sha1(canonical root)>`,
    /// lower-cased first since NTFS paths are case-insensitive.
    pub fn for_repo(repo_root: &Path) -> io::Result<Self> {
        if cfg!(windows) {
            let canonical = std::fs::canonicalize(repo_root)?;
            let key = canonical.to_string_lossy().to_lowercase();
            let hash = meshtree_core::lease::coord::sha1_hex(key.as_bytes());
            Ok(Self {
                path: PathBuf::from(format!(r"\\.\pipe\meshtree-{}", &hash[..16])),
            })
        } else {
            Ok(Self {
                path: repo_root.join(".meshtree").join("daemon.sock"),
            })
        }
    }

    /// Socket path or pipe name.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.path.display())
    }
}

/// A connected byte stream, split into a buffered reader and a writer.
struct Duplex {
    reader: BufReader<Box<dyn Read + Send>>,
    writer: Box<dyn Write + Send>,
}

impl Duplex {
    fn read_line(&mut self) -> io::Result<Option<String>> {
        let mut line = String::new();
        if self.reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        Ok(Some(line))
    }

    fn write_json<T: Serialize>(&mut self, value: &T) -> io::Result<()> {
        let mut bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
        bytes.push(b'\n');
        self.writer.write_all(&bytes)?;
        self.writer.flush()
    }
}

/// Client side of the daemon IPC.
pub struct IpcClient {
    conn: Duplex,
    next_id: u64,
}

impl IpcClient {
    /// Connects; `MT-010` if nothing is listening at `endpoint`.
    pub fn connect(endpoint: &Endpoint) -> Result<Self, IpcError> {
        let conn = sys::connect(endpoint.path()).map_err(|source| {
            if is_unreachable(&source) {
                IpcError::Unreachable {
                    endpoint: endpoint.to_string(),
                    source,
                }
            } else {
                IpcError::Io(source)
            }
        })?;
        Ok(Self { conn, next_id: 1 })
    }

    /// Sends one request and waits for its response.
    pub fn call(&mut self, method: &str, params: serde_json::Value) -> Result<Response, IpcError> {
        let id = self.next_id;
        self.next_id += 1;
        self.conn.write_json(&Request {
            id,
            method: method.to_string(),
            params,
        })?;
        let line = self.conn.read_line()?.ok_or_else(|| {
            IpcError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "daemon closed the connection",
            ))
        })?;
        let response: Response = serde_json::from_str(&line).map_err(io::Error::other)?;
        if response.id != id {
            return Err(IpcError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("response id {} does not match request id {id}", response.id),
            )));
        }
        Ok(response)
    }
}

fn is_unreachable(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
    )
}

/// Server side of the daemon IPC.
pub struct IpcListener {
    inner: sys::Listener,
}

impl IpcListener {
    /// Binds `endpoint` with the §17 access restrictions. Fails with
    /// `AddrInUse` if a live daemon already listens there; a stale Unix
    /// socket file left by a dead daemon is replaced.
    pub fn bind(endpoint: &Endpoint) -> io::Result<Self> {
        Ok(Self {
            inner: sys::Listener::bind(endpoint.path())?,
        })
    }

    /// Accepts connections forever on a background thread, serving each on
    /// its own thread with `handler`.
    pub fn serve<H>(self, handler: H) -> thread::JoinHandle<()>
    where
        H: Fn(Request) -> Response + Send + Sync + 'static,
    {
        let handler = Arc::new(handler);
        let mut listener = self.inner;
        thread::spawn(move || loop {
            match listener.accept() {
                Ok(conn) => {
                    let handler = Arc::clone(&handler);
                    thread::spawn(move || serve_connection(conn, handler.as_ref()));
                }
                Err(_) => thread::sleep(std::time::Duration::from_millis(10)),
            }
        })
    }
}

fn serve_connection<H: Fn(Request) -> Response>(mut conn: Duplex, handler: &H) {
    while let Ok(Some(line)) = conn.read_line() {
        let Ok(request) = serde_json::from_str::<Request>(&line) else {
            return;
        };
        if conn.write_json(&handler(request)).is_err() {
            return;
        }
    }
}

#[cfg(unix)]
mod sys {
    use super::Duplex;
    use std::fs;
    use std::io::{self, BufReader};
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};

    pub struct Listener {
        listener: UnixListener,
        _path: PathBuf,
    }

    fn duplex(stream: UnixStream) -> io::Result<Duplex> {
        let reader = stream.try_clone()?;
        Ok(Duplex {
            reader: BufReader::new(Box::new(reader)),
            writer: Box::new(stream),
        })
    }

    pub fn connect(path: &Path) -> io::Result<Duplex> {
        duplex(UnixStream::connect(path)?)
    }

    impl Listener {
        pub fn bind(path: &Path) -> io::Result<Self> {
            if path.exists() {
                if UnixStream::connect(path).is_ok() {
                    return Err(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        format!("a daemon is already listening at {}", path.display()),
                    ));
                }
                // Stale socket from a dead daemon.
                fs::remove_file(path)?;
            }
            let listener = UnixListener::bind(path)?;
            if let Err(err) = restrict(path) {
                let _ = fs::remove_file(path);
                return Err(err);
            }
            Ok(Self {
                listener,
                _path: path.to_path_buf(),
            })
        }

        pub fn accept(&mut self) -> io::Result<Duplex> {
            let (stream, _) = self.listener.accept()?;
            duplex(stream)
        }
    }

    /// §17: socket `0600`; its directory must belong to the same user and
    /// not be writable by group or others (else another user could swap the
    /// socket).
    fn restrict(path: &Path) -> io::Result<()> {
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        let socket = fs::metadata(path)?;
        let parent = path.parent().unwrap_or(Path::new("."));
        let dir = fs::metadata(parent)?;
        if dir.uid() != socket.uid() || dir.mode() & 0o022 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{} must be owned by the daemon user and not group/world-writable",
                    parent.display()
                ),
            ));
        }
        Ok(())
    }
}

#[cfg(windows)]
mod sys {
    use super::Duplex;
    use std::ffi::c_void;
    use std::fs::{File, OpenOptions};
    use std::io::{self, BufReader};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use std::path::Path;
    use std::time::{Duration, Instant};

    type Handle = *mut c_void;

    const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;
    const PIPE_ACCESS_DUPLEX: u32 = 0x0000_0003;
    const FILE_FLAG_FIRST_PIPE_INSTANCE: u32 = 0x0008_0000;
    const PIPE_TYPE_BYTE: u32 = 0x0000_0000;
    const PIPE_READMODE_BYTE: u32 = 0x0000_0000;
    const PIPE_WAIT: u32 = 0x0000_0000;
    const PIPE_REJECT_REMOTE_CLIENTS: u32 = 0x0000_0008;
    const PIPE_UNLIMITED_INSTANCES: u32 = 255;
    const ERROR_PIPE_CONNECTED: u32 = 535;
    const ERROR_PIPE_BUSY: i32 = 231;
    const SDDL_REVISION_1: u32 = 1;
    /// Protected DACL: full access for the object owner and SYSTEM only.
    const PIPE_SDDL: &str = "D:P(A;;GA;;;OW)(A;;GA;;;SY)";

    #[repr(C)]
    struct SecurityAttributes {
        length: u32,
        security_descriptor: *mut c_void,
        inherit_handle: i32,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateNamedPipeW(
            name: *const u16,
            open_mode: u32,
            pipe_mode: u32,
            max_instances: u32,
            out_buffer_size: u32,
            in_buffer_size: u32,
            default_timeout_ms: u32,
            security_attributes: *const SecurityAttributes,
        ) -> Handle;
        fn ConnectNamedPipe(pipe: Handle, overlapped: *mut c_void) -> i32;
        fn GetLastError() -> u32;
        fn LocalFree(mem: *mut c_void) -> *mut c_void;
        fn CloseHandle(handle: Handle) -> i32;
    }

    #[link(name = "advapi32")]
    extern "system" {
        fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl: *const u16,
            revision: u32,
            descriptor: *mut *mut c_void,
            size: *mut u32,
        ) -> i32;
    }

    fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
        s.encode_wide().chain(Some(0)).collect()
    }

    fn duplex(file: File) -> io::Result<Duplex> {
        let reader = file.try_clone()?;
        Ok(Duplex {
            reader: BufReader::new(Box::new(reader)),
            writer: Box::new(file),
        })
    }

    pub fn connect(path: &Path) -> io::Result<Duplex> {
        // All instances busy between a server accept and its next instance:
        // retry briefly. A missing pipe is NotFound (=> MT-010).
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match OpenOptions::new().read(true).write(true).open(path) {
                Ok(file) => return duplex(file),
                Err(err)
                    if err.raw_os_error() == Some(ERROR_PIPE_BUSY) && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(err) => return Err(err),
            }
        }
    }

    pub struct Listener {
        name: Vec<u16>,
        next: Handle,
    }

    // The pending pipe handle is only touched by the accept thread.
    unsafe impl Send for Listener {}

    impl Drop for Listener {
        fn drop(&mut self) {
            // SAFETY: `next` is a pipe handle we created and still own.
            unsafe { CloseHandle(self.next) };
        }
    }

    impl Listener {
        pub fn bind(path: &Path) -> io::Result<Self> {
            let name = wide(path.as_os_str());
            // FIRST_PIPE_INSTANCE: fail if another daemon already owns the name.
            let next = create_instance(&name, true).map_err(|err| {
                if err.kind() == io::ErrorKind::PermissionDenied {
                    io::Error::new(
                        io::ErrorKind::AddrInUse,
                        format!("a daemon is already listening at {}", path.display()),
                    )
                } else {
                    err
                }
            })?;
            Ok(Self { name, next })
        }

        pub fn accept(&mut self) -> io::Result<Duplex> {
            // SAFETY: `self.next` is a valid, unconnected pipe instance.
            let connected = unsafe { ConnectNamedPipe(self.next, std::ptr::null_mut()) };
            // SAFETY: plain FFI call with no arguments.
            if connected == 0 && unsafe { GetLastError() } != ERROR_PIPE_CONNECTED {
                return Err(io::Error::last_os_error());
            }
            let handle = self.next;
            self.next = create_instance(&self.name, false)?;
            // SAFETY: `handle` is a connected pipe we own; File takes ownership.
            duplex(unsafe { File::from_raw_handle(handle) })
        }
    }

    fn create_instance(name: &[u16], first: bool) -> io::Result<Handle> {
        let sddl = wide(std::ffi::OsStr::new(PIPE_SDDL));
        let mut descriptor: *mut c_void = std::ptr::null_mut();
        // SAFETY: `sddl` is NUL-terminated; `descriptor` receives a LocalAlloc'd
        // buffer that is freed below.
        let converted = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        };
        if converted == 0 {
            return Err(io::Error::last_os_error());
        }
        let attributes = SecurityAttributes {
            length: std::mem::size_of::<SecurityAttributes>() as u32,
            security_descriptor: descriptor,
            inherit_handle: 0,
        };
        let open_mode = PIPE_ACCESS_DUPLEX
            | if first {
                FILE_FLAG_FIRST_PIPE_INSTANCE
            } else {
                0
            };
        // SAFETY: `name` is NUL-terminated UTF-16; `attributes` outlives the call.
        let handle = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                open_mode,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                64 * 1024,
                64 * 1024,
                0,
                &attributes,
            )
        };
        let result = if handle == INVALID_HANDLE_VALUE {
            Err(io::Error::last_os_error())
        } else {
            Ok(handle)
        };
        // SAFETY: `descriptor` was allocated by the conversion call above.
        unsafe { LocalFree(descriptor) };
        result
    }
}
