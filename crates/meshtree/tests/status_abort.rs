//! Integration tests for `meshtree status` and `meshtree abort`
//! (`SPEC.md` §7, §8, §14, §16).

mod common;

use common::{assert_exit, git, meshtree, TempDir};
use meshtree_core::{Task, TaskStatus, ERROR_CODE_ILLEGAL_TRANSITION};
use std::fs;
use std::path::Path;

fn initialized_repo() -> TempDir {
    let repo = TempDir::repo_with_commit();
    assert_exit(&meshtree(repo.path(), &["init"]), 0);
    repo
}

fn spawn(repo: &Path, name: &str) -> String {
    let out = meshtree(repo, &["spawn", name]);
    assert_exit(&out, 0);
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .find_map(|l| l.strip_prefix("task_id: "))
        .expect("spawn prints task_id")
        .to_string()
}

fn stdout(repo: &Path, args: &[&str], code: i32) -> String {
    let out = meshtree(repo, args);
    assert_exit(&out, code);
    String::from_utf8(out.stdout).unwrap()
}

fn status_json(repo: &Path) -> Vec<Task> {
    serde_json::from_str(&stdout(repo, &["status", "--json"], 0))
        .expect("status --json is a task array")
}

#[test]
fn status_lists_registry_entries() {
    let repo = initialized_repo();
    let a = spawn(repo.path(), "task-a");
    let b = spawn(repo.path(), "task-b");

    // JSON: every registry entry, sorted by task id, identical to task.json.
    let tasks = status_json(repo.path());
    let ids: Vec<String> = tasks.iter().map(|t| t.id.to_string()).collect();
    let mut expected = vec![a.clone(), b.clone()];
    expected.sort();
    assert_eq!(ids, expected);
    for task in &tasks {
        let on_disk: Task = serde_json::from_str(
            &fs::read_to_string(
                repo.path()
                    .join(".meshtree/tasks")
                    .join(task.id.as_str())
                    .join("task.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(task, &on_disk);
        assert_eq!(task.status, TaskStatus::Active);
    }

    // Text: one line per task with id, status, name, branch, base.
    let text = stdout(repo.path(), &["status"], 0);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2);
    for (line, id) in lines.iter().zip(&expected) {
        assert!(line.starts_with(id.as_str()), "{line}");
        assert!(line.contains("active"));
        assert!(line.contains(&format!("branch=meshtree/{id}")));
        assert!(line.contains("base=main"));
    }

    // Single task, text and JSON.
    let one = stdout(repo.path(), &["status", &a], 0);
    assert!(one.starts_with(&a) && one.contains("task-a"));
    let one: Task =
        serde_json::from_str(&stdout(repo.path(), &["status", &a, "--json"], 0)).unwrap();
    assert_eq!(one.id.as_str(), a);
}

#[test]
fn status_handles_empty_uninitialized_and_malformed_registries() {
    // Initialized repo with no tasks.
    let repo = initialized_repo();
    assert_eq!(stdout(repo.path(), &["status"], 0), "no tasks\n");
    assert!(status_json(repo.path()).is_empty());

    // Repo never initialized: no tasks/ directory is an empty registry.
    let bare = TempDir::repo_with_commit();
    assert_eq!(stdout(bare.path(), &["status"], 0), "no tasks\n");

    // Malformed records are reported on stderr and skipped; valid ones still list.
    let good = spawn(repo.path(), "good");
    let tasks = repo.path().join(".meshtree/tasks");
    let bad_id = "t-01JAXGZK7Q4D3RT2WFH8MB3VQY";
    fs::create_dir_all(tasks.join(bad_id)).unwrap();
    fs::write(tasks.join(bad_id).join("task.json"), "{ not json").unwrap();
    // A task dir holding only a merged handoff record is not an error (§13).
    fs::create_dir_all(tasks.join("t-01JAXGZK7Q4D3RT2WFH8MB3VQZ")).unwrap();
    fs::write(
        tasks.join("t-01JAXGZK7Q4D3RT2WFH8MB3VQZ/HANDOFF.md"),
        "# done",
    )
    .unwrap();

    let out = meshtree(repo.path(), &["status", "--json"]);
    assert_exit(&out, 0);
    let listed: Vec<Task> = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id.as_str(), good);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(bad_id) && stderr.contains("malformed"),
        "{stderr}"
    );
    assert!(!stderr.contains("VQZ"), "{stderr}");

    // Asking for the malformed task directly is a task error.
    assert_exit(&meshtree(repo.path(), &["status", bad_id]), 20);
}

#[test]
fn status_rejects_unknown_and_invalid_ids() {
    let repo = initialized_repo();
    assert_exit(
        &meshtree(repo.path(), &["status", "t-01JAXGZK7Q4D3RT2WFH8MB3VQY"]),
        20,
    );
    assert_exit(&meshtree(repo.path(), &["status", "not-a-task"]), 10);
}

#[test]
fn abort_removes_worktree_and_marks_abandoned() {
    let repo = initialized_repo();
    let keep = spawn(repo.path(), "keep");
    let dropped = spawn(repo.path(), "drop");
    let worktree = repo.path().join(".meshtree/worktrees").join(&dropped);
    assert!(worktree.is_dir());

    let out = stdout(repo.path(), &["abort", &dropped], 0);
    assert_eq!(out.trim(), format!("abandoned {dropped}"));

    // Worktree gone from disk and from git; branch kept for recovery.
    assert!(!worktree.exists());
    let list = git(repo.path(), &["worktree", "list", "--porcelain"]);
    assert!(!list.contains(&format!("refs/heads/meshtree/{dropped}")));
    assert!(list.contains(&format!("refs/heads/meshtree/{keep}")));
    git(
        repo.path(),
        &[
            "rev-parse",
            "--verify",
            &format!("refs/heads/meshtree/{dropped}"),
        ],
    );

    // Registry updated; the other task is untouched.
    let tasks = status_json(repo.path());
    let by_id = |id: &str| tasks.iter().find(|t| t.id.as_str() == id).unwrap().status;
    assert_eq!(by_id(&dropped), TaskStatus::Abandoned);
    assert_eq!(by_id(&keep), TaskStatus::Active);
}

#[test]
fn abort_non_active_task_is_mt004_without_side_effects() {
    let repo = initialized_repo();
    let id = spawn(repo.path(), "once");
    assert_exit(&meshtree(repo.path(), &["abort", &id]), 0);
    let task_json = repo
        .path()
        .join(".meshtree/tasks")
        .join(&id)
        .join("task.json");
    let before = fs::read(&task_json).unwrap();

    let out = meshtree(repo.path(), &["abort", &id]);
    assert_exit(&out, 20);
    assert!(String::from_utf8_lossy(&out.stderr).contains(ERROR_CODE_ILLEGAL_TRANSITION));
    assert_eq!(fs::read(&task_json).unwrap(), before);

    assert_exit(
        &meshtree(repo.path(), &["abort", "t-01JAXGZK7Q4D3RT2WFH8MB3VQY"]),
        20,
    );
    assert_exit(&meshtree(repo.path(), &["abort", "bogus"]), 10);
}
