//! Shared-operation FIFO queue through the daemon (`SPEC.md` §7
//! `QueueTicket`, §12, §14, §17, `ADR-0005`).

mod common;

use common::{assert_exit, meshtree, spawn_task, DaemonProc, TempDir};
use meshtree_platform::ipc::{Endpoint, IpcClient};
use meshtree_platform::lock::boot_id;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const CONFIG: &str = r#"[project]
base_branch = "main"

[queue.test]
concurrency = 1
command = "git rev-parse --abbrev-ref HEAD >> queue-log.txt"

[queue.hold]
concurrency = 1
command = "git cat-file --batch"
"#;

fn repo_with_queues() -> TempDir {
    let repo = TempDir::repo_with_commit();
    assert_exit(&meshtree(repo.path(), &["init"]), 0);
    fs::write(repo.path().join(".meshtree/config.toml"), CONFIG).unwrap();
    repo
}

/// One IPC call; panics on transport errors, returns the raw response.
fn rpc(repo: &Path, method: &str, params: Value) -> meshtree_platform::ipc::Response {
    let endpoint = Endpoint::for_repo(repo).unwrap();
    IpcClient::connect(&endpoint)
        .expect("daemon reachable")
        .call(method, params)
        .expect("ipc call")
}

fn ok(repo: &Path, method: &str, params: Value) -> Value {
    let resp = rpc(repo, method, params);
    assert!(resp.ok, "{method}: {resp:?}");
    resp.result.unwrap()
}

fn request(repo: &Path, task: &str, queue: &str) -> Value {
    ok(
        repo,
        "request_shared_op",
        json!({
            "task_id": task,
            "queue": queue,
            "pid": std::process::id(),
            "boot_id": boot_id().unwrap(),
        }),
    )
}

fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn queue_fifo_serializes_concurrent_requests() {
    let repo = repo_with_queues();
    let root = repo.path();
    let a = spawn_task(root, "task-a");
    let b = spawn_task(root, "task-b");
    let daemon = DaemonProc::start(root);

    // Six concurrent requesters on a concurrency-1 queue: never two holders
    // at once, and grants follow request order (ticket ids are allocated in
    // request order).
    let holders = AtomicUsize::new(0);
    let granted_order = Mutex::new(Vec::new());
    let mut requested: Vec<String> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..6)
            .map(|i| {
                let task = if i % 2 == 0 { &a } else { &b };
                let (holders, granted_order) = (&holders, &granted_order);
                s.spawn(move || {
                    let ticket = request(root, task, "test");
                    let id = ticket["id"].as_str().unwrap().to_string();
                    assert!(id.starts_with("q-"), "{ticket}");
                    assert_eq!(ticket["queue"], "test");
                    wait_until("grant", || {
                        ok(root, "ticket_status", json!({ "ticket_id": id }))["granted"] == true
                    });
                    let now = holders.fetch_add(1, Ordering::SeqCst) + 1;
                    assert_eq!(now, 1, "two holders in a concurrency-1 queue");
                    granted_order.lock().unwrap().push(id.clone());
                    std::thread::sleep(Duration::from_millis(30));
                    holders.fetch_sub(1, Ordering::SeqCst);
                    let released = ok(root, "release_shared_op", json!({ "ticket_id": id }));
                    assert_eq!(released["released"], true);
                    id
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    requested.sort();
    assert_eq!(
        *granted_order.lock().unwrap(),
        requested,
        "FIFO grant order"
    );
    assert_eq!(ok(root, "queue_status", json!({})), json!([]));

    // Two CLI processes run the queue command one at a time in task a's
    // worktree, each releasing its slot.
    let clients: Vec<_> = (0..2)
        .map(|_| {
            Command::new(env!("CARGO_BIN_EXE_meshtree"))
                .args(["queue", "test", "request", "--task", &a])
                .current_dir(root)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    for client in clients {
        let out = client.wait_with_output().unwrap();
        assert_exit(&out, 0);
        assert!(String::from_utf8_lossy(&out.stdout).contains("granted q-"));
    }
    let log = fs::read_to_string(
        root.join(".meshtree/worktrees")
            .join(&a)
            .join("queue-log.txt"),
    )
    .unwrap();
    let lines: Vec<&str> = log.lines().map(str::trim).collect();
    assert_eq!(lines, [format!("meshtree/{a}"), format!("meshtree/{a}")]);
    assert_eq!(ok(root, "queue_status", json!({})), json!([]));

    // Overrides outside allow_args are refused before a ticket exists (§17).
    let out = meshtree(
        root,
        &["queue", "test", "request", "--task", &a, "--", "--evil"],
    );
    assert_exit(&out, 10);
    assert!(String::from_utf8_lossy(&out.stderr).contains("not allow-listed"));
    assert_eq!(ok(root, "queue_status", json!({})), json!([]));

    // Unknown queue: usage error.
    assert_exit(
        &meshtree(root, &["queue", "nope", "request", "--task", &a]),
        10,
    );

    daemon.stop(root);

    // Queues live in the daemon: without one, MT-010 (exit 30).
    let out = meshtree(root, &["queue", "test", "request", "--task", &a]);
    assert_exit(&out, 30);
    assert!(String::from_utf8_lossy(&out.stderr).contains("MT-010"));
}

#[test]
fn queue_dead_holder_process_releases_slot() {
    let repo = repo_with_queues();
    let root = repo.path();
    let a = spawn_task(root, "task-a");
    let daemon = DaemonProc::start(root);

    // A CLI client takes the only slot and runs a command that blocks on its
    // stdin (inherited from this test).
    let mut holder = Command::new(env!("CARGO_BIN_EXE_meshtree"))
        .args(["queue", "hold", "request", "--task", &a])
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let holder_pid = holder.id();
    wait_until("the CLI holder's grant", || {
        let tickets = ok(root, "queue_status", json!({ "queue": "hold" }));
        tickets
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["holder"]["pid"] == holder_pid && t["granted"] == true)
    });

    // A second requester waits behind it.
    let mine = request(root, &a, "hold");
    let id = mine["id"].as_str().unwrap().to_string();
    assert_eq!(mine["granted"], false);
    assert_eq!(mine["position"], 1);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(
        ok(root, "ticket_status", json!({ "ticket_id": id }))["granted"],
        false,
        "a live holder keeps its slot"
    );

    // Kill the holder (no release): its dead pid frees the slot.
    holder.kill().unwrap();
    holder.wait().unwrap();
    wait_until("the dead holder's eviction", || {
        ok(root, "ticket_status", json!({ "ticket_id": id }))["granted"] == true
    });
    let tickets = ok(root, "queue_status", json!({ "queue": "hold" }));
    let pids: Vec<&Value> = tickets
        .as_array()
        .unwrap()
        .iter()
        .map(|t| &t["holder"]["pid"])
        .collect();
    assert_eq!(pids, [&json!(std::process::id())]);
    // Closing the inherited stdin lets the orphaned command exit.
    drop(holder.stdin.take());

    // A ticket of an evicted holder is unknown afterwards.
    let ours = ok(root, "release_shared_op", json!({ "ticket_id": id }));
    assert_eq!(ours["released"], true);
    let gone = rpc(root, "ticket_status", json!({ "ticket_id": id }));
    assert!(!gone.ok);
    daemon.stop(root);
}
