//! Windows mandatory locking (Trap 2): an exclusive handle on a worktree file
//! must never fail a merge or an abort (`SPEC.md` §10, §16 `MT-020`/`MT-021`,
//! §18, §20 Week 4; `ADR-0006`).
#![cfg(windows)]

mod common;

use common::{assert_exit, commit, git, meshtree, DaemonProc, TempDir};
use meshtree_core::{Task, TaskStatus};
use std::fs::{self, File};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Output;

const CONFIG: &str = r#"[project]
base_branch = "main"
validate = ["git --version"]
promote_validate = ["git --version"]
"#;

fn spawn_with_file(root: &Path, name: &str) -> (String, PathBuf) {
    let out = meshtree(root, &["spawn", name]);
    assert_exit(&out, 0);
    let id = String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("task_id: "))
        .unwrap()
        .to_string();
    let wt = root.join(".meshtree/worktrees").join(&id);
    fs::create_dir_all(wt.join("src")).unwrap();
    fs::write(wt.join(format!("src/{name}.txt")), "work\n").unwrap();
    git(&wt, &["add", "."]);
    commit(&wt, name);
    (id, wt)
}

/// `CreateFile` with `dwShareMode = 0`, as an IDE or indexer might hold it.
fn lock_exclusively(path: &Path) -> File {
    fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(path)
        .expect("exclusive handle")
}

fn read_task(root: &Path, id: &str) -> Task {
    let path = root.join(".meshtree/tasks").join(id).join("task.json");
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn quarantine_note(err: &str) -> bool {
    err.contains("MT-021") || err.contains("MT-020")
}

#[test]
fn windows_locked_handle_quarantines_never_fails_merge() {
    let repo = TempDir::repo_with_commit();
    let root = repo.path();
    assert_exit(&meshtree(root, &["init"]), 0);
    fs::write(root.join(".meshtree/config.toml"), CONFIG).unwrap();

    // collect: the merge lands even though the worktree cannot be removed.
    let (a, wt_a) = spawn_with_file(root, "task-a");
    let held_a = wt_a.join("src/task-a.txt");
    let handle_a = lock_exclusively(&held_a);
    let daemon = DaemonProc::start(root);
    let out = meshtree(root, &["collect", &a]);
    assert_exit(&out, 0);
    assert!(quarantine_note(&stderr(&out)), "{}", stderr(&out));
    assert_eq!(read_task(root, &a).status, TaskStatus::Merged);
    let integration = git(root, &["rev-parse", "meshtree/integration"]);
    let branch = git(root, &["rev-parse", &format!("meshtree/{a}")]);
    let merged = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "merge-base",
            "--is-ancestor",
            branch.trim(),
            integration.trim(),
        ])
        .status()
        .unwrap();
    assert!(
        merged.success(),
        "the task branch is in the integration branch"
    );
    // Everything but the locked file is gone (ADR-0006 fallback) or the
    // whole tree was quarantined.
    assert!(!wt_a.join("README.md").exists());

    // abort: the task is abandoned even though its worktree is locked.
    let (b, wt_b) = spawn_with_file(root, "task-b");
    let handle_b = lock_exclusively(&wt_b.join("src/task-b.txt"));
    let out = meshtree(root, &["abort", &b]);
    assert_exit(&out, 0);
    assert!(quarantine_note(&stderr(&out)), "{}", stderr(&out));
    assert_eq!(read_task(root, &b).status, TaskStatus::Abandoned);
    daemon.stop(root);

    // Once the handles close, prune reclaims everything.
    drop(handle_a);
    drop(handle_b);
    assert_exit(&meshtree(root, &["prune"]), 0);
    assert!(!wt_a.exists() && !wt_b.exists());
    let quarantine = root.join(".meshtree/quarantine");
    assert!(
        fs::read_dir(&quarantine).map_or(true, |mut d| d.next().is_none()),
        "quarantine emptied"
    );
    let listed = git(root, &["worktree", "list", "--porcelain"]);
    assert!(!listed.contains(&a) && !listed.contains(&b), "{listed}");
}
