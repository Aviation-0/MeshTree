//! §17 security boundaries, end to end through the CLI and the daemon
//! (`SPEC.md` §17, `ADR-0005`, `ADR-0003`).
//!
//! Each test asserts the boundary's *effect* (no command ran, no claim was
//! recorded anywhere), not only the exit code.

mod common;

use common::{assert_exit, git, meshtree, spawn_task, DaemonProc, TempDir};
use meshtree_platform::ipc::{Endpoint, IpcClient, Response};
use meshtree_platform::lock::boot_id;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

/// `ran.txt` is appended to only when the queue command actually runs.
const CONFIG: &str = r#"[project]
base_branch = "main"

[queue.test]
concurrency = 1
command = "echo ran >> ran.txt"
allow_args = ["--verbose"]
"#;

fn rpc(repo: &Path, method: &str, params: Value) -> Response {
    let endpoint = Endpoint::for_repo(repo).unwrap();
    IpcClient::connect(&endpoint)
        .expect("daemon reachable")
        .call(method, params)
        .expect("ipc call")
}

fn read_task(repo: &Path, id: &str) -> Value {
    let path = repo.join(".meshtree/tasks").join(id).join("task.json");
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

/// Event paths in the coordination log (empty if the ref does not exist).
fn coord_paths(repo: &Path) -> Vec<String> {
    let Ok(out) = std::process::Command::new("git")
        .args(["-C"])
        .arg(repo)
        .args([
            "ls-tree",
            "-r",
            "--name-only",
            "refs/meshtree/coord",
            "log/",
        ])
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|blob| git(repo, &["show", &format!("refs/meshtree/coord:{blob}")]))
        .map(|json| {
            serde_json::from_str::<Value>(&json).unwrap()["path"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect()
}

#[test]
fn security_rejects_unlisted_queue_override() {
    let repo = TempDir::repo_with_commit();
    let root = repo.path();
    assert_exit(&meshtree(root, &["init"]), 0);
    fs::write(root.join(".meshtree/config.toml"), CONFIG).unwrap();
    let task = spawn_task(root, "task-q");
    let worktree = root.join(read_task(root, &task)["worktree"].as_str().unwrap());
    let ran = worktree.join("ran.txt");
    let daemon = DaemonProc::start(root);

    // Unlisted overrides, including shell metacharacters smuggled in one
    // argument: refused before any ticket or command (usage, exit 10).
    for evil in [
        "--evil",
        "--verbose; touch pwned",
        "--verbose && echo pwned",
        "$(touch pwned)",
        "--verbose=1",
        "--VERBOSE",
    ] {
        let out = meshtree(
            root,
            &["queue", "test", "request", "--task", &task, "--", evil],
        );
        assert_exit(&out, 10);
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("allow-listed"),
            "{evil}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert!(
        !ran.exists(),
        "a rejected override must never run the command"
    );
    assert!(!worktree.join("pwned").exists() && !root.join("pwned").exists());

    // The daemon enforces it too, for any IPC client (not just this CLI).
    let resp = rpc(
        root,
        "request_shared_op",
        json!({
            "task_id": task,
            "queue": "test",
            "command_override": ["--verbose", "--evil"],
            "pid": std::process::id(),
            "boot_id": boot_id().unwrap(),
        }),
    );
    assert!(!resp.ok, "{resp:?}");
    assert_eq!(resp.error.unwrap().code, "usage");
    let tickets = rpc(root, "queue_status", json!({ "queue": "test" }));
    assert_eq!(
        tickets.result,
        Some(json!([])),
        "no ticket issued for a refused override"
    );
    assert!(!ran.exists());

    // An allow-listed override runs the committed command exactly once.
    assert_exit(
        &meshtree(
            root,
            &[
                "queue",
                "test",
                "request",
                "--task",
                &task,
                "--",
                "--verbose",
            ],
        ),
        0,
    );
    let log = fs::read_to_string(&ran).unwrap();
    assert_eq!(log.lines().count(), 1, "{log:?}");

    // allow_args itself cannot carry whitespace (config validation, MT-050).
    fs::write(
        root.join(".meshtree/config.toml"),
        CONFIG.replace(r#"["--verbose"]"#, r#"["--verbose; rm -rf ."]"#),
    )
    .unwrap();
    let out = meshtree(root, &["spawn", "after-bad-config"]);
    assert_exit(&out, 10);
    assert!(String::from_utf8_lossy(&out.stderr).contains("MT-050"));
    fs::write(root.join(".meshtree/config.toml"), CONFIG).unwrap();
    daemon.stop(root);
}

#[test]
fn security_rejects_dot_meshtree_write_claims() {
    let repo = TempDir::repo_with_commit();
    let root = repo.path();
    assert_exit(&meshtree(root, &["init"]), 0);
    let task = spawn_task(root, "task-c");
    let forbidden = [
        ".meshtree",
        ".meshtree/**",
        ".meshtree/tasks/**",
        "./.meshtree/config.toml",
        ".MeshTree/config.toml",
        ".MESHTREE/**",
        ".mesh*",
        "*.meshtree/x",
        ".meshtree\\tasks\\x",
        "src/../.meshtree/x",
    ];

    // Without a daemon (in-process path).
    for glob in forbidden {
        let out = meshtree(root, &["claim", &task, glob]);
        assert_exit(&out, 10);
    }
    // A batch with one forbidden pattern grants nothing (all-or-nothing).
    assert_exit(
        &meshtree(root, &["claim", &task, "src/**", ".meshtree/**"]),
        10,
    );
    assert_eq!(read_task(root, &task)["claims"], json!([]));

    // Through the daemon, for any IPC client.
    let daemon = DaemonProc::start(root);
    for glob in forbidden {
        let resp = rpc(
            root,
            "claim_paths",
            json!({ "task_id": task, "paths": [glob] }),
        );
        assert!(!resp.ok, "{glob}: {resp:?}");
        assert_eq!(resp.error.unwrap().code, "usage", "{glob}");
    }
    // A spawn that claims coordination state fails before creating anything.
    let before = fs::read_dir(root.join(".meshtree/tasks")).unwrap().count();
    assert_exit(
        &meshtree(root, &["spawn", "sneaky", "--claim", ".meshtree/**"]),
        10,
    );
    assert_eq!(
        fs::read_dir(root.join(".meshtree/tasks")).unwrap().count(),
        before
    );
    daemon.stop(root);

    // Nothing reached the task record or the coordination log.
    assert_eq!(read_task(root, &task)["claims"], json!([]));
    let logged = coord_paths(root);
    assert!(
        logged
            .iter()
            .all(|p| !p.to_ascii_lowercase().contains("meshtree") && !p.contains("mesh*")),
        "{logged:?}"
    );

    // Ordinary claims still work, so the rejections above are specific.
    assert_exit(&meshtree(root, &["claim", &task, "src/**"]), 0);
    assert_eq!(read_task(root, &task)["claims"], json!(["src/**"]));
}
