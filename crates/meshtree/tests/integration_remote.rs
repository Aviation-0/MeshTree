//! Integration-branch remote sync: a non-fast-forward push of
//! `meshtree/integration` is fetched, replay-merged and retried (bounded ×5)
//! (`SPEC.md` §8 `collect` step 4, §16 `MT-031`).

mod common;

use common::{assert_exit, commit, git, meshtree, spawn_task, DaemonProc, TempDir};
use meshtree_platform::ipc::{Endpoint, IpcClient};
use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use std::process::Command;

const INTEGRATION: &str = "refs/heads/meshtree/integration";

fn config(promote_validate: &str) -> String {
    let escaped = promote_validate.replace('\\', "\\\\").replace('"', "\\\"");
    format!(
        "[project]\nbase_branch = \"main\"\nintegration_branch = \"meshtree/integration\"\n\
         validate = [\"git --version\"]\npromote_validate = [\"{escaped}\"]\n"
    )
}

/// A clone of `remote` with MeshTree initialized and `promote_validate` set.
fn clone(remote: &TempDir, promote_validate: &str) -> TempDir {
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
    fs::write(
        dir.path().join(".meshtree/config.toml"),
        config(promote_validate),
    )
    .unwrap();
    dir
}

/// Commits `file` in the task's worktree.
fn work(root: &Path, id: &str, file: &str) {
    let wt = root.join(".meshtree/worktrees").join(id);
    fs::write(wt.join(file), format!("{file}\n")).unwrap();
    git(&wt, &["add", file]);
    commit(&wt, file);
}

fn collect(root: &Path, id: &str) -> Value {
    let endpoint = Endpoint::for_repo(root).unwrap();
    let resp = IpcClient::connect(&endpoint)
        .expect("daemon reachable")
        .call("collect_task", json!({ "task_id": id }))
        .unwrap();
    assert!(resp.ok, "collect {id}: {resp:?}");
    resp.result.unwrap()
}

fn rev(dir: &Path, r: &str) -> String {
    git(dir, &["rev-parse", r]).trim().to_string()
}

#[test]
fn collect_integration_non_ff_remote_retries_within_bound() {
    let seed = TempDir::repo_with_commit();
    let remote = TempDir::bare();
    git(seed.path(), &["push", "-q", &remote.file_url(), "main"]);

    // Clone B's promote_validate collects clone A's task: A's merge reaches
    // the remote integration branch between B's fetch and B's push, so B's
    // first push is a non-fast-forward. On B's retry A's task is already
    // merged; `|| git --version` keeps the validation green.
    let a = clone(&remote, "git --version");
    let exe = env!("CARGO_BIN_EXE_meshtree");
    let a_daemon = DaemonProc::start(a.path());
    let a_id = spawn_task(a.path(), "alpha");
    work(a.path(), &a_id, "a.txt");
    let hook = if cfg!(windows) {
        format!(
            "cd /d \"{}\" && \"{exe}\" collect {a_id} || git --version",
            a.path().display()
        )
    } else {
        format!(
            "cd '{}' && '{exe}' collect {a_id} || git --version",
            a.path().display()
        )
    };
    let b = clone(&remote, &hook);
    let b_daemon = DaemonProc::start(b.path());
    let b_id = spawn_task(b.path(), "beta");
    work(b.path(), &b_id, "b.txt");

    let result = collect(b.path(), &b_id);
    assert_eq!(result["status"], "merged", "{result}");
    assert_eq!(result["pushed"], true, "{result}");
    assert_eq!(
        result["attempts"], 2,
        "one lost race, then success: {result}"
    );
    assert!(result["attempts"].as_u64().unwrap() <= 5);

    // A's collect (run by the hook) landed first and pushed on its first try.
    let a_task: Value = serde_json::from_str(
        &fs::read_to_string(
            a.path()
                .join(".meshtree/tasks")
                .join(&a_id)
                .join("task.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(a_task["status"], "merged");

    // Both merges are on the remote integration branch; B's merge sits on
    // top of A's (replayed), and B's local branch matches the remote.
    let tip = rev(remote.path(), INTEGRATION);
    for file in ["a.txt", "b.txt"] {
        let shown = git(
            remote.path(),
            &["show", &format!("meshtree/integration:{file}")],
        );
        assert_eq!(shown.trim(), file);
    }
    assert_eq!(rev(b.path(), INTEGRATION), tip);
    let a_tip = rev(a.path(), INTEGRATION);
    assert_eq!(rev(remote.path(), &format!("{tip}^1")), a_tip);
    assert_eq!(result["integration"], tip);

    // A later collect on A fast-forwards from the remote and pushes again.
    let a2 = spawn_task(a.path(), "gamma");
    work(a.path(), &a2, "c.txt");
    let result = collect(a.path(), &a2);
    assert_eq!(result["attempts"], 1, "{result}");
    assert_eq!(result["pushed"], true);
    let tip2 = rev(remote.path(), INTEGRATION);
    assert_eq!(rev(remote.path(), &format!("{tip2}^1")), tip);

    a_daemon.stop(a.path());
    b_daemon.stop(b.path());
}
