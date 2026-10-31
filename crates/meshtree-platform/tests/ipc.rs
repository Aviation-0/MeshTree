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
