//! The daemon decides claims in memory, journals their events, and commits
//! them in the background (`SPEC.md` §9 daemon claim path, §10 crash safety,
//! §15 `claim_paths` `sync?`; `ADR-0009` Part 2).

mod common;

use common::{assert_exit, meshtree, spawn_task, DaemonProc, TempDir};
use meshtree_platform::ipc::{Endpoint, IpcClient, Response};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn repo() -> TempDir {
    let repo = TempDir::repo_with_commit();
    assert_exit(&meshtree(repo.path(), &["init"]), 0);
    repo
}

fn rpc(root: &Path, method: &str, params: Value) -> Response {
    IpcClient::connect(&Endpoint::for_repo(root).unwrap())
        .expect("daemon reachable")
        .call(method, params)
        .unwrap()
}

fn ok(root: &Path, method: &str, params: Value) -> Value {
    let resp = rpc(root, method, params);
    assert!(resp.ok, "{method}: {resp:?}");
    resp.result.unwrap()
}

/// `log/` entries in the local coordination ref (0 if it does not exist).
fn logged(root: &Path) -> usize {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-tree", "-r", "--name-only", "refs/meshtree/coord"])
        .output()
        .unwrap();
    if !out.status.success() {
        return 0;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.starts_with("log/"))
        .count()
}

fn wait_for(what: &str, limit: Duration, mut cond: impl FnMut() -> bool) -> Duration {
    let start = Instant::now();
    while !cond() {
        assert!(start.elapsed() < limit, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
    start.elapsed()
}

#[test]
fn daemon_claim_acknowledged_from_memory_then_committed_within_250ms() {
    let repo = repo();
    let root = repo.path();
    let a = spawn_task(root, "a");
    let b = spawn_task(root, "b");
    let daemon = DaemonProc::start(root);
    assert_eq!(logged(root), 0);

    // Acknowledged from memory: the answer says the commit is pending.
    let claimed = ok(
        root,
        "claim_paths",
        json!({ "task_id": a, "paths": ["src/x/**"] }),
    );
    assert_eq!(claimed["coord"], "pending", "{claimed}");
    assert_eq!(claimed["paths"], json!(["src/x/**"]));

    // A conflicting claim is refused at once, committed or not.
    let resp = rpc(
        root,
        "claim_paths",
        json!({ "task_id": b, "paths": ["src/x/lib.rs"] }),
    );
    assert!(!resp.ok);
    assert_eq!(resp.error.as_ref().unwrap().code, "MT-001", "{resp:?}");

    // The writer commits it: the event reaches the ref, and the daemon
    // reports it waited at most 250 ms before committing (§9).
    wait_for("the commit", Duration::from_secs(5), || logged(root) == 1);
    let ping = ok(root, "ping", json!({}));
    assert_eq!(ping["pending_events"], 0, "{ping}");
    let waited = ping["last_flush_wait_ms"].as_u64().unwrap();
    assert!(waited <= 250, "events waited {waited} ms before the commit");

    // A burst of claims shares commits and all of them land.
    for i in 0..5 {
        let path = format!("burst/{i}/**");
        ok(
            root,
            "claim_paths",
            json!({ "task_id": b, "paths": [path] }),
        );
    }
    wait_for("the burst", Duration::from_secs(5), || logged(root) == 6);

    // `sync: true` answers only once the event is in the ref.
    let synced = ok(
        root,
        "claim_paths",
        json!({ "task_id": b, "paths": ["sync/**"], "sync": true }),
    );
    assert_eq!(synced["coord"], "committed", "{synced}");
    assert_eq!(logged(root), 7);

    // The CLI flag reaches the same path.
    let out = meshtree(root, &["claim", &a, "cli/**", "--sync"]);
    assert_exit(&out, 0);
    assert!(String::from_utf8_lossy(&out.stdout).contains("committed"));
    assert_eq!(logged(root), 8);

    // Releases go the same way; `sync` flushes and sees every lease.
    let released = ok(
        root,
        "release_paths",
        json!({ "task_id": b, "paths": ["sync/**"] }),
    );
    assert_eq!(released["coord"], "pending");
    let out = meshtree(root, &["sync"]);
    assert_exit(&out, 0);
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("leases: 7"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    daemon.stop(root);
}

#[test]
fn chaos_kill_between_claim_ack_and_flush_loses_no_claim() {
    let repo = repo();
    let root = repo.path();
    let a = spawn_task(root, "holder");
    let b = spawn_task(root, "rival");

    // A daemon that holds acknowledged events for a minute before committing.
    let mut child = Command::new(env!("CARGO_BIN_EXE_meshtree"))
        .arg("daemon")
        .current_dir(root)
        .env("MESHTREE_TEST_FLUSH_DELAY_MS", "60000")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    assert!(line.contains("listening on"), "{line:?}");

    let claimed = ok(
        root,
        "claim_paths",
        json!({ "task_id": a, "paths": ["src/**"] }),
    );
    assert_eq!(claimed["coord"], "pending");
    assert_eq!(logged(root), 0, "not committed yet");

    // Crash between acknowledgement and commit.
    child.kill().unwrap();
    child.wait().unwrap();
    drop(stdout);
    assert_eq!(logged(root), 0, "the claim exists only in the WAL");

    // Restart: recovery re-queues the journaled event. The claim holds at
    // once (a rival is refused) and reaches the ref shortly after.
    let daemon = DaemonProc::start(root);
    let resp = rpc(
        root,
        "claim_paths",
        json!({ "task_id": b, "paths": ["src/lib.rs"] }),
    );
    assert!(!resp.ok, "{resp:?}");
    let body = resp.error.unwrap();
    assert_eq!(body.code, "MT-001");
    assert!(body.message.contains(&a), "{body:?}");
    wait_for("the recovered commit", Duration::from_secs(5), || {
        logged(root) == 1
    });
    let out = meshtree(root, &["sync"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("leases: 1"));
    let task: Value = serde_json::from_str(
        &std::fs::read_to_string(root.join(".meshtree/tasks").join(&a).join("task.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(task["claims"], json!(["src/**"]));
    daemon.stop(root);
}
