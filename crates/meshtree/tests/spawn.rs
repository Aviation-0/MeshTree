//! Integration tests for `meshtree spawn` (`SPEC.md` §5, §7, §8, §14).

mod common;

use common::{assert_exit, commit, git, meshtree, TempDir};
use meshtree_core::{Config, Task, TaskId, TaskStatus};
use std::fs;
use std::path::Path;

/// Runs `meshtree spawn <args>` and returns the printed task ID.
fn spawn_ok(repo: &Path, args: &[&str]) -> TaskId {
    let mut full = vec!["spawn"];
    full.extend_from_slice(args);
    let out = meshtree(repo, &full);
    assert_exit(&out, 0);
    let stdout = String::from_utf8(out.stdout).unwrap();
    let id = stdout
        .lines()
        .find_map(|l| l.strip_prefix("task_id: "))
        .expect("spawn prints task_id");
    TaskId::parse(id).expect("printed task_id is canonical")
}

fn read_task(repo: &Path, id: &TaskId) -> Task {
    let path = repo
        .join(".meshtree/tasks")
        .join(id.as_str())
        .join("task.json");
    serde_json::from_str(&fs::read_to_string(path).expect("task.json exists"))
        .expect("task.json parses as Task")
}

fn initialized_repo() -> TempDir {
    let repo = TempDir::repo_with_commit();
    assert_exit(&meshtree(repo.path(), &["init"]), 0);
    repo
}

#[test]
fn spawn_creates_worktree_branch_and_task_json() {
    let repo = initialized_repo();
    let id = spawn_ok(repo.path(), &["fix-auth-middleware", "--agent", "vibe-cli"]);
    let branch = format!("meshtree/{id}");

    // Worktree at .meshtree/worktrees/<id>, on branch meshtree/<id>, at main.
    let wt = repo.path().join(".meshtree/worktrees").join(id.as_str());
    assert!(wt.join("README.md").is_file(), "worktree checked out");
    assert_eq!(
        git(&wt, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
        branch
    );
    assert_eq!(
        git(&wt, &["rev-parse", "HEAD"]),
        git(repo.path(), &["rev-parse", "main"])
    );
    let list = git(repo.path(), &["worktree", "list", "--porcelain"]);
    assert!(list.contains(&format!("branch refs/heads/{branch}")));

    // task.json registry entry (§7), status active after setup (§8).
    let task = read_task(repo.path(), &id);
    assert_eq!(task.id, id);
    assert_eq!(task.name, "fix-auth-middleware");
    assert_eq!(task.agent.as_deref(), Some("vibe-cli"));
    assert_eq!(task.branch, branch);
    assert_eq!(task.worktree, format!(".meshtree/worktrees/{id}"));
    assert_eq!(task.base, "main");
    assert_eq!(task.status, TaskStatus::Active);
    assert!(task.claims.is_empty());
    // The template sets no [machine] id, so it is derived from the host.
    assert!(task.machine.starts_with("mc-"), "machine: {}", task.machine);
    assert!(task.machine.len() > "mc-".len());
    assert_eq!(task.created_at.len(), "2026-11-03T14:02:11Z".len());
    assert!(task.created_at.ends_with('Z'));
    assert!(!repo
        .path()
        .join(".meshtree/tasks")
        .join(id.as_str())
        .join("task.json.tmp")
        .exists());

    // Worktree and machine-local state stay invisible to git in the primary checkout.
    let status = git(
        repo.path(),
        &["status", "--porcelain", "--untracked-files=all"],
    );
    let mut untracked: Vec<&str> = status.lines().collect();
    untracked.sort_unstable();
    assert_eq!(
        untracked,
        ["?? .meshtree/.gitignore", "?? .meshtree/config.toml"]
    );
}

#[test]
fn spawn_with_base_flag_checks_out_that_ref() {
    let repo = initialized_repo();
    git(repo.path(), &["checkout", "-q", "-b", "dev"]);
    fs::write(repo.path().join("dev.txt"), "dev\n").unwrap();
    git(repo.path(), &["add", "dev.txt"]);
    commit(repo.path(), "dev work");
    git(repo.path(), &["checkout", "-q", "main"]);

    let id = spawn_ok(repo.path(), &["on-dev", "--base", "dev"]);
    let wt = repo.path().join(".meshtree/worktrees").join(id.as_str());
    assert!(wt.join("dev.txt").is_file());
    assert_eq!(
        git(&wt, &["rev-parse", "HEAD"]),
        git(repo.path(), &["rev-parse", "dev"])
    );
    assert_eq!(read_task(repo.path(), &id).base, "dev");
}

#[test]
fn spawn_uses_configured_machine_id() {
    let repo = initialized_repo();
    let config = Config::template().replace("# id = \"custom-id\"", "id = \"mc-test-box\"");
    assert_ne!(config, Config::template());
    fs::write(repo.path().join(".meshtree/config.toml"), config).unwrap();

    let id = spawn_ok(repo.path(), &["with-machine"]);
    assert_eq!(read_task(repo.path(), &id).machine, "mc-test-box");
}

#[test]
fn spawn_twice_allocates_distinct_ids_and_worktrees() {
    let repo = initialized_repo();
    let a = spawn_ok(repo.path(), &["task-a"]);
    let b = spawn_ok(repo.path(), &["task-b"]);
    assert_ne!(a, b);
    let list = git(repo.path(), &["worktree", "list", "--porcelain"]);
    assert!(list.contains(&format!("branch refs/heads/meshtree/{a}")));
    assert!(list.contains(&format!("branch refs/heads/meshtree/{b}")));
    assert_eq!(read_task(repo.path(), &a).status, TaskStatus::Active);
    assert_eq!(read_task(repo.path(), &b).status, TaskStatus::Active);
}

#[test]
fn spawn_unknown_base_exits_usage_and_creates_nothing() {
    let repo = initialized_repo();
    let out = meshtree(repo.path(), &["spawn", "typo", "--base", "no-such-ref"]);
    assert_exit(&out, 10);
    let tasks = fs::read_dir(repo.path().join(".meshtree/tasks")).unwrap();
    assert_eq!(tasks.count(), 0, "no task record for a rejected base");
    let list = git(repo.path(), &["worktree", "list", "--porcelain"]);
    assert!(!list.contains("refs/heads/meshtree/"));
}

#[test]
fn spawn_without_init_exits_usage() {
    let repo = TempDir::repo_with_commit();
    let out = meshtree(repo.path(), &["spawn", "x"]);
    assert_exit(&out, 10);
    assert!(String::from_utf8_lossy(&out.stderr).contains("meshtree init"));
    assert!(!repo.path().join(".meshtree").exists());
}
