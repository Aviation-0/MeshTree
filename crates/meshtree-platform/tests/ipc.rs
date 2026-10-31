//! NDJSON IPC over UDS / named pipe (`SPEC.md` §10, §16 `MT-010`, §17).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use meshtree_platform::ipc::{
    Endpoint, IpcClient, IpcError, IpcListener, Response, ERROR_CODE_DAEMON_UNREACHABLE,
};
use serde_json::json;

/// Short temp directory (Unix socket paths are limited to ~104 bytes on macOS)
/// with a `.meshtree/` inside; removed on drop.
struct TempRepo(PathBuf);

static COUNTER: AtomicU64 = AtomicU64::new(0);

impl TempRepo {
    fn new() -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "mt-ipc-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(path.join(".meshtree")).expect("create temp repo");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempRepo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn echo_server(endpoint: &Endpoint) {
    IpcListener::bind(endpoint)
        .expect("bind")
        .serve(|req| match req.method.as_str() {
            "echo" => Response::ok(req.id, json!({ "echo": req.params })),
            other => Response::err(req.id, "MT-050", format!("unknown method {other}")),
        });
}

#[test]
fn ipc_ndjson_roundtrip_concurrent_clients() {
    // Wire shape (§10).
    assert_eq!(
        serde_json::to_string(&Response::ok(7, json!({}))).unwrap(),
        r#"{"id":7,"ok":true,"result":{}}"#
    );
    assert_eq!(
        serde_json::to_string(&Response::err(7, "MT-001", "held")).unwrap(),
        r#"{"id":7,"ok":false,"error":{"code":"MT-001","message":"held"}}"#
    );

    let repo = TempRepo::new();
    let endpoint = Endpoint::for_repo(repo.path()).unwrap();
    let listener = IpcListener::bind(&endpoint).expect("bind");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(endpoint.path(), repo.path().join(".meshtree/daemon.sock"));
        let mode = fs::metadata(endpoint.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "socket must be 0600 (§17)");
    }
    #[cfg(windows)]
    {
        let name = endpoint.path().to_string_lossy().into_owned();
        assert!(name.starts_with(r"\\.\pipe\meshtree-"), "{name}");
        assert_eq!(name.len(), r"\\.\pipe\meshtree-".len() + 16);
    }

    // A second daemon cannot bind while the first is live.
    let err = IpcListener::bind(&endpoint)
        .err()
        .expect("second bind fails");
    assert_eq!(err.kind(), io::ErrorKind::AddrInUse, "{err}");

    listener.serve(|req| match req.method.as_str() {
        "echo" => Response::ok(req.id, json!({ "echo": req.params })),
        other => Response::err(req.id, "MT-050", format!("unknown method {other}")),
    });

    std::thread::scope(|s| {
        for client_no in 0..8u64 {
            let endpoint = &endpoint;
            s.spawn(move || {
                let mut client = IpcClient::connect(endpoint).expect("connect");
                for n in 0..25u64 {
                    let params = json!({ "client": client_no, "n": n });
                    let resp = client.call("echo", params.clone()).expect("call");
                    assert!(resp.ok, "{resp:?}");
                    assert_eq!(resp.id, n + 1);
                    assert_eq!(resp.result, Some(json!({ "echo": params })));
                }
                let resp = client.call("nope", json!(null)).expect("call");
                assert!(!resp.ok);
                assert_eq!(resp.error.unwrap().code, "MT-050");
            });
        }
    });
}

#[test]
fn ipc_unreachable_returns_mt010() {
    let repo = TempRepo::new();
    let endpoint = Endpoint::for_repo(repo.path()).unwrap();

    // Nothing ever listened here.
    let err = IpcClient::connect(&endpoint).err().expect("no daemon");
    assert_eq!(err.code(), Some(ERROR_CODE_DAEMON_UNREACHABLE), "{err}");
    assert!(matches!(err, IpcError::Unreachable { .. }));
    assert!(err.to_string().contains("MT-010"));

    // A daemon that went away (stale socket file on Unix, closed pipe on
    // Windows) is also unreachable, and a new daemon can take over.
    drop(IpcListener::bind(&endpoint).expect("bind"));
    let err = IpcClient::connect(&endpoint).err().expect("daemon gone");
    assert_eq!(err.code(), Some(ERROR_CODE_DAEMON_UNREACHABLE), "{err}");

    echo_server(&endpoint);
    let mut client = IpcClient::connect(&endpoint).expect("connect after rebind");
    let resp = client.call("echo", json!([1, 2, 3])).unwrap();
    assert_eq!(resp.result, Some(json!({ "echo": [1, 2, 3] })));
}

/// Removes a directory on drop.
#[cfg(unix)]
struct Cleanup(PathBuf);

#[cfg(unix)]
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A repository nested so deep that `.meshtree/daemon.sock` exceeds the
/// socket path limit (`ADR-0007`), plus a private fallback base. The base is
/// short and under `/tmp` like the real one: macOS's `$TMPDIR` alone is
/// ~50 bytes, which would push the fallback socket itself past the limit.
#[cfg(unix)]
fn deep_repo(tmp: &TempRepo) -> (PathBuf, PathBuf, Cleanup) {
    let repo = tmp.path().join("x".repeat(100));
    fs::create_dir_all(repo.join(".meshtree")).unwrap();
    let base = PathBuf::from(format!(
        "/tmp/mtb-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&base).unwrap();
    (repo, base.clone(), Cleanup(base))
}

#[cfg(unix)]
fn my_uid(tmp: &TempRepo) -> u32 {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(tmp.path()).unwrap().uid()
}

#[cfg(unix)]
#[test]
fn ipc_long_repo_path_uses_tmp_fallback() {
    use meshtree_platform::ipc::MAX_SOCKET_PATH;
    use std::os::unix::fs::PermissionsExt;

    let tmp = TempRepo::new();
    let (repo, base, _cleanup) = deep_repo(&tmp);
    assert!(repo.join(".meshtree/daemon.sock").as_os_str().len() > MAX_SOCKET_PATH);

    let endpoint = Endpoint::for_repo_in(&repo, &base).unwrap();
    assert!(endpoint.is_fallback());
    let dir = base.join(format!("meshtree-{}", my_uid(&tmp)));
    assert_eq!(endpoint.path().parent(), Some(dir.as_path()));
    let name = endpoint
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert!(name.ends_with(".sock") && name.len() == 16 + 5, "{name}");
    assert!(endpoint.path().as_os_str().len() <= MAX_SOCKET_PATH);
    // Daemon and clients derive the same endpoint independently.
    assert_eq!(Endpoint::for_repo_in(&repo, &base).unwrap(), endpoint);
    // The production base is /tmp.
    let default = Endpoint::for_repo(&repo).unwrap();
    assert!(
        default.path().starts_with("/tmp"),
        "{}",
        default.path().display()
    );
    assert!(default.path().to_string_lossy().contains("/meshtree-"));
    // A short repository path keeps the in-repo socket.
    assert!(!Endpoint::for_repo_in(tmp.path(), &base)
        .unwrap()
        .is_fallback());

    // Bind creates the private directory; the daemon serves through it.
    echo_server(&endpoint);
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(mode(endpoint.path()), 0o600);
    let mut client = IpcClient::connect(&endpoint).expect("connect via fallback");
    let resp = client.call("echo", json!({ "deep": true })).unwrap();
    assert_eq!(resp.result, Some(json!({ "echo": { "deep": true } })));
}

#[cfg(unix)]
#[test]
fn ipc_fallback_dir_wrong_owner_or_mode_refused() {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    let tmp = TempRepo::new();
    let (repo, base, _cleanup) = deep_repo(&tmp);
    let endpoint = Endpoint::for_repo_in(&repo, &base).unwrap();
    let dir = endpoint.path().parent().unwrap().to_path_buf();
    let refused = |what: &str| {
        let err = IpcListener::bind(&endpoint).err().expect(what);
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{what}: {err}");
        match IpcClient::connect(&endpoint) {
            Err(IpcError::Io(err)) => {
                assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{what}")
            }
            Err(other) => panic!("{what}: expected a refusal, got {other}"),
            Ok(_) => panic!("{what}: connected"),
        }
    };

    // Group/world-accessible directory (someone could swap the socket).
    fs::DirBuilder::new().mode(0o755).create(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    refused("mode 0755");

    // A symlink, even to a private directory we own, is not accepted: it
    // may have been planted to redirect the socket.
    fs::remove_dir(&dir).unwrap();
    let elsewhere = tmp.path().join("elsewhere");
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&elsewhere)
        .unwrap();
    std::os::unix::fs::symlink(&elsewhere, &dir).unwrap();
    refused("symlink");
    // (A directory owned by another uid fails the same check; creating one
    // needs root, so it is not exercised here.)

    // No directory: no daemon (MT-010); binding creates it correctly.
    fs::remove_file(&dir).unwrap();
    let err = IpcClient::connect(&endpoint)
        .err()
        .expect("nothing listening");
    assert_eq!(err.code(), Some(ERROR_CODE_DAEMON_UNREACHABLE), "{err}");
    echo_server(&endpoint);
    let mut client = IpcClient::connect(&endpoint).unwrap();
    assert!(client.call("echo", json!(1)).unwrap().ok);
}
