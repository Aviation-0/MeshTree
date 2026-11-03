//! Daemon-driven status changes reach the coordination-ref task registry
//! (`SPEC.md` §5, §8 `collect` and the abandoned sweeper, §9; `ADR-0008`).

mod common;

use common::{assert_exit, commit, git, meshtree, spawn_task, DaemonProc, TempDir};
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

/// Short TTL so the `ttl_secs × 4` sweeper fires within the test.
const CONFIG: &str = r#"[project]
base_branch = "main"
integration_branch = "meshtree/integration"
validate = ["git --version"]
promote_validate = ["git --version"]

[lease]
ttl_secs = 3
heartbeat_secs = 1
"#;

fn clone(remote: &TempDir) -> TempDir {
    let dir = TempDir::new();
    let out = Command::new("git")
        .args(["clone", "-q", "-b", "main", &remote.file_url()])
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_exit(&meshtree(dir.path(), &["init"]), 0);
    dir
}

/// The registry record for `id` in `dir`'s coordination ref, if any.
fn record(dir: &Path, id: &str) -> Option<Value> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["show", &format!("refs/meshtree/coord:tasks/{id}.json")])
        .output()
        .unwrap();
    out.status
        .success()
        .then(|| serde_json::from_slice(&out.stdout).unwrap())
}

/// Syncs `dir` and returns the registry status of `id`.
fn remote_status(dir: &Path, id: &str) -> String {
    assert_exit(&meshtree(dir, &["sync"]), 0);
    record(dir, id)
        .map(|r| r["status"].as_str().unwrap().to_string())
        .unwrap_or_default()
}

#[test]
fn collect_and_sweep_publish_task_status_to_coord_registry() {
    let seed = TempDir::repo_with_commit();
    let remote = TempDir::bare();
    git(seed.path(), &["push", "-q", &remote.file_url(), "main"]);
    let mac = clone(&remote);
    let win = clone(&remote);
    fs::write(mac.path().join(".meshtree/config.toml"), CONFIG).unwrap();

    let merged = spawn_task(mac.path(), "merged");
    let idle = spawn_task(mac.path(), "idle");
    let daemon = DaemonProc::start(mac.path());

    // collect through the daemon: the second clone sees `merged`.
    let wt = mac.path().join(".meshtree/worktrees").join(&merged);
    fs::write(wt.join("work.txt"), "work\n").unwrap();
    git(&wt, &["add", "work.txt"]);
    commit(&wt, "work");
    assert_exit(&meshtree(mac.path(), &["collect", &merged]), 0);
    let published = record(mac.path(), &merged).expect("published locally");
    assert_eq!(published["status"], "merged");
    assert_eq!(remote_status(win.path(), &merged), "merged");

    // An illegal edge (collecting a merged task, MT-004) changes nothing.
    assert_exit(&meshtree(mac.path(), &["collect", &merged]), 20);
    assert_eq!(remote_status(win.path(), &merged), "merged");

    // The sweeper abandons the idle task after ttl_secs × 4 (12 s): the
    // second clone sees `abandoned`.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if remote_status(win.path(), &idle) == "abandoned" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "sweep never reached the registry: {:?}",
            record(mac.path(), &idle)
        );
        std::thread::sleep(Duration::from_millis(500));
    }
    let local: Value = serde_json::from_str(
        &fs::read_to_string(
            mac.path()
                .join(".meshtree/tasks")
                .join(&idle)
                .join("task.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(local["status"], "abandoned");
    daemon.stop(mac.path());
}
