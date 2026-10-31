//! Integration tests for `meshtree claim` / `release` / `sync` and
//! `spawn --claim` (`SPEC.md` §8, §9, §14, §16).

mod common;

use common::{assert_exit, git, meshtree, TempDir};
use meshtree_core::{Task, TaskStatus, ERROR_CODE_CLAIM_CONFLICT};
use std::fs;
use std::path::Path;
use std::process::Output;

fn initialized_repo() -> TempDir {
    let repo = TempDir::repo_with_commit();
    assert_exit(&meshtree(repo.path(), &["init"]), 0);
    repo
}

fn task_id(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("task_id: "))
        .expect("spawn prints task_id")
        .to_string()
}

fn spawn(repo: &Path, args: &[&str]) -> String {
    let mut full = vec!["spawn"];
    full.extend_from_slice(args);
    let out = meshtree(repo, &full);
    assert_exit(&out, 0);
    task_id(&out)
}

fn read_task(repo: &Path, id: &str) -> Task {
    let path = repo.join(".meshtree/tasks").join(id).join("task.json");
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn coord(repo: &Path) -> String {
    git(repo, &["rev-parse", "--verify", "refs/meshtree/coord"])
        .trim()
        .to_string()
}

#[test]
fn cli_claim_conflict_returns_mt001_exit_20() {
    let repo = initialized_repo();
    let root = repo.path();

    // A claims at spawn; the claim is recorded in task.json and the coord ref.
    let a = spawn(root, &["task-a", "--claim", "src/auth/**"]);
    assert_eq!(read_task(root, &a).claims, ["src/auth/**"]);
    let coord_after_a = coord(root);

    // B overlaps (subtree of A's claim): MT-001, exit 20, nothing recorded.
    let b = spawn(root, &["task-b"]);
    let out = meshtree(root, &["claim", &b, "src/auth/login.rs"]);
    assert_exit(&out, 20);
    assert!(
        stderr(&out).contains(ERROR_CODE_CLAIM_CONFLICT),
        "{}",
        stderr(&out)
    );
    assert!(
        stderr(&out).contains(&a),
        "names the holder: {}",
        stderr(&out)
    );
    assert!(read_task(root, &b).claims.is_empty());

    // All-or-nothing: a batch with one conflicting path grants nothing.
    let out = meshtree(root, &["claim", &b, "docs/**", "src/auth/x.rs"]);
    assert_exit(&out, 20);
    assert!(read_task(root, &b).claims.is_empty());
    let c = spawn(root, &["task-c", "--claim", "docs/**"]);
    assert_eq!(
        read_task(root, &c).claims,
        ["docs/**"],
        "docs/** was released"
    );

    // A conflicting spawn fails fast: task failed, no worktree.
    let out = meshtree(root, &["spawn", "task-d", "--claim", "./src//auth/"]);
    assert_exit(&out, 20);
    assert!(stderr(&out).contains(ERROR_CODE_CLAIM_CONFLICT));
    let failed: Vec<Task> = fs::read_dir(root.join(".meshtree/tasks"))
        .unwrap()
        .map(|e| read_task(root, &e.unwrap().file_name().to_string_lossy()))
        .filter(|t| t.name == "task-d")
        .collect();
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].status, TaskStatus::Failed);
    assert!(!root.join(&failed[0].worktree).exists());

    // Invalid patterns and non-active tasks.
    assert_exit(&meshtree(root, &["claim", &b, ".meshtree/tasks/**"]), 10);
    assert_exit(&meshtree(root, &["claim", &b, "../escape"]), 10);
    assert_exit(
        &meshtree(root, &["claim", &failed[0].id.to_string(), "x"]),
        20,
    );
    assert_exit(&meshtree(root, &["claim", &b]), 10);

    // A releases; B's claim now succeeds.
    assert_exit(&meshtree(root, &["release", &a, "src/auth/**"]), 0);
    assert!(read_task(root, &a).claims.is_empty());
    let out = meshtree(root, &["claim", &b, "src/auth/login.rs"]);
    assert_exit(&out, 0);
    assert!(String::from_utf8_lossy(&out.stdout).contains("claimed src/auth/login.rs"));
    assert_eq!(read_task(root, &b).claims, ["src/auth/login.rs"]);
    assert_ne!(coord(root), coord_after_a);

    // Aborting B releases its claim, so A can re-claim.
    assert_exit(&meshtree(root, &["abort", &b]), 0);
    assert_exit(&meshtree(root, &["claim", &a, "src/auth/**"]), 0);
}

#[test]
fn cli_sync_flushes_and_pulls_coord_ref() {
    let remote = TempDir::bare();
    let (mac, win) = (initialized_repo(), initialized_repo());
    for clone in [&mac, &win] {
        git(
            clone.path(),
            &["remote", "add", "origin", &remote.file_url()],
        );
    }

    // mac claims with the remote reachable: the coord ref is pushed.
    let a = spawn(mac.path(), &["mac-task", "--claim", "docs/**"]);
    assert_eq!(coord(remote.path()), coord(mac.path()));

    // win pulls with `sync` and then sees mac's lease synchronously.
    let out = meshtree(win.path(), &["sync"]);
    assert_exit(&out, 0);
    assert_eq!(coord(win.path()), coord(remote.path()));
    assert!(String::from_utf8_lossy(&out.stdout).contains("leases: 1"));
    let out = meshtree(
        win.path(),
        &["spawn", "win-task", "--claim", "docs/guide.md"],
    );
    assert_exit(&out, 20);
    assert!(stderr(&out).contains(ERROR_CODE_CLAIM_CONFLICT));
    assert!(stderr(&out).contains(&a));

    // Remote unreachable: mac's claim still succeeds locally, push deferred.
    git(
        mac.path(),
        &[
            "remote",
            "set-url",
            "origin",
            "file:///nonexistent/meshtree-remote",
        ],
    );
    let out = meshtree(mac.path(), &["claim", &a, "src/**"]);
    assert_exit(&out, 0);
    assert!(stderr(&out).contains("deferred"), "{}", stderr(&out));
    assert_ne!(coord(mac.path()), coord(remote.path()));

    // Remote back: `sync` flushes the pending event.
    git(
        mac.path(),
        &["remote", "set-url", "origin", &remote.file_url()],
    );
    let out = meshtree(mac.path(), &["sync"]);
    assert_exit(&out, 0);
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("pushed"),
        "{:?}",
        out
    );
    assert_eq!(coord(mac.path()), coord(remote.path()));

    // win pulls again: both clones hold byte-identical coordination state.
    assert_exit(&meshtree(win.path(), &["sync"]), 0);
    assert_eq!(coord(win.path()), coord(mac.path()));
    let out = meshtree(win.path(), &["sync"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("leases: 2"));
}
