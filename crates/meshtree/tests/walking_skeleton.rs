//! Week-1 exit smoke test (`SPEC.md` §8, §14, §20 Week 1): the scripted
//! `init → spawn → status → abort` walkthrough on one temp repository,
//! asserting the `.meshtree/tasks/<id>/task.json` registry at each step.

mod common;

use common::{assert_exit, git, meshtree, TempDir};
use meshtree_core::{Task, TaskStatus};
use std::fs;
use std::path::Path;

fn registry(repo: &Path, id: &str) -> Task {
    let path = repo.join(".meshtree/tasks").join(id).join("task.json");
    serde_json::from_str(&fs::read_to_string(path).expect("task.json exists"))
        .expect("task.json parses")
}

fn task_dirs(repo: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(repo.join(".meshtree/tasks"))
        .expect("tasks dir")
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn walking_skeleton_init_spawn_status_abort() {
    let repo = TempDir::repo_with_commit();
    let root = repo.path();

    // 1. init: config + layout, empty registry.
    assert_exit(&meshtree(root, &["init"]), 0);
    assert!(root.join(".meshtree/config.toml").is_file());
    assert!(task_dirs(root).is_empty());
    let status = meshtree(root, &["status"]);
    assert_exit(&status, 0);
    assert_eq!(String::from_utf8_lossy(&status.stdout), "no tasks\n");

    // 2. spawn: one active task with its worktree and branch.
    let out = meshtree(root, &["spawn", "walking-skeleton", "--agent", "smoke"]);
    assert_exit(&out, 0);
    let stdout = String::from_utf8(out.stdout).unwrap();
    let id = stdout
        .lines()
        .find_map(|l| l.strip_prefix("task_id: "))
        .expect("task_id printed")
        .to_string();
    assert_eq!(task_dirs(root), vec![id.clone()]);
    let task = registry(root, &id);
    assert_eq!(task.status, TaskStatus::Active);
    assert_eq!(task.name, "walking-skeleton");
    assert_eq!(task.agent.as_deref(), Some("smoke"));
    let worktree = root.join(&task.worktree);
    assert!(worktree.join("README.md").is_file());
    assert_eq!(
        git(&worktree, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
        task.branch
    );

    // 3. status: the registry entry, in text and JSON.
    let text = meshtree(root, &["status"]);
    assert_exit(&text, 0);
    let text = String::from_utf8(text.stdout).unwrap();
    assert_eq!(text.lines().count(), 1);
    assert!(text.starts_with(&id) && text.contains("active"));
    let json = meshtree(root, &["status", "--json"]);
    assert_exit(&json, 0);
    let listed: Vec<Task> = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(listed, vec![task.clone()]);

    // 4. abort: worktree gone, record abandoned, still listed.
    assert_exit(&meshtree(root, &["abort", &id]), 0);
    assert!(!worktree.exists());
    let aborted = registry(root, &id);
    assert_eq!(aborted.status, TaskStatus::Abandoned);
    assert_eq!(
        Task {
            status: TaskStatus::Active,
            ..aborted.clone()
        },
        task
    );
    let json = meshtree(root, &["status", &id, "--json"]);
    assert_exit(&json, 0);
    let one: Task = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(one, aborted);
    assert_eq!(task_dirs(root), vec![id]);
}
