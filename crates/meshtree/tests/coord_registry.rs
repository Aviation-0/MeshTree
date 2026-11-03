//! Cross-machine task registry: `tasks/<id>.json` in `refs/meshtree/coord`
//! (`SPEC.md` §5, §7, §8 `spawn` step 4, §9).

mod common;

use common::{assert_exit, git, meshtree, spawn_task, TempDir};
use serde_json::Value;
use std::path::Path;
use std::process::Command;

/// A clone of `remote` with MeshTree initialized.
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

/// The registry record for `id` in `dir`'s local coordination ref.
fn record(dir: &Path, id: &str) -> Value {
    let text = git(
        dir,
        &["show", &format!("refs/meshtree/coord:tasks/{id}.json")],
    );
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}: {text}"))
}

fn sync(dir: &Path) {
    assert_exit(&meshtree(dir, &["sync"]), 0);
}

#[test]
fn spawn_registers_task_in_coord_ref_and_syncs_to_second_clone() {
    let seed = TempDir::repo_with_commit();
    let remote = TempDir::bare();
    git(seed.path(), &["push", "-q", &remote.file_url(), "main"]);
    let mac = clone(&remote);
    let win = clone(&remote);

    // spawn on the first clone registers the task and pushes it.
    let out = meshtree(mac.path(), &["spawn", "alpha", "--claim", "src/**"]);
    assert_exit(&out, 0);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let id = stdout
        .lines()
        .find_map(|l| l.strip_prefix("task_id: "))
        .unwrap()
        .to_string();
    let local = record(mac.path(), &id);
    assert_eq!(local["status"], "active");
    assert_eq!(local["name"], "alpha");
    assert_eq!(local["claims"], serde_json::json!(["src/**"]));
    assert!(local["revision"].as_u64().unwrap() > 0, "{local}");

    // The second clone reads it after a sync.
    sync(win.path());
    let seen = record(win.path(), &id);
    assert_eq!(seen, local);

    // A failed spawn on the second clone (claim conflict, MT-001) is
    // registered too and reaches the first clone.
    let out = meshtree(win.path(), &["spawn", "beta", "--claim", "src/lib.rs"]);
    assert_exit(&out, 20);
    let tasks_dir = win.path().join(".meshtree/tasks");
    let failed_id = std::fs::read_dir(&tasks_dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .find(|name| name.starts_with("t-"))
        .expect("failed task recorded locally");
    assert_eq!(record(win.path(), &failed_id)["status"], "failed");
    sync(mac.path());
    assert_eq!(record(mac.path(), &failed_id)["status"], "failed");
    assert_eq!(record(mac.path(), &id)["status"], "active", "kept on merge");

    // abort updates the record through the same CAS sync; the newer
    // revision wins over the second clone's older copy.
    assert_exit(&meshtree(mac.path(), &["abort", &id]), 0);
    let aborted = record(mac.path(), &id);
    assert_eq!(aborted["status"], "abandoned");
    assert!(aborted["revision"].as_u64() > local["revision"].as_u64());
    sync(win.path());
    assert_eq!(record(win.path(), &id), aborted);
    assert_eq!(record(win.path(), &failed_id)["status"], "failed");

    // Leases and the registry live side by side; both clones converge.
    let other = spawn_task(win.path(), "gamma");
    sync(mac.path());
    assert_eq!(record(mac.path(), &other)["status"], "active");
    let coord = |dir: &Path| git(dir, &["rev-parse", "refs/meshtree/coord"]);
    sync(win.path());
    assert_eq!(coord(mac.path()), coord(win.path()));
    let listing = git(
        mac.path(),
        &["ls-tree", "-r", "--name-only", "refs/meshtree/coord"],
    );
    assert!(listing.lines().any(|l| l.starts_with("log/")), "{listing}");
    assert_eq!(
        listing.lines().filter(|l| l.starts_with("tasks/")).count(),
        3,
        "{listing}"
    );
}
