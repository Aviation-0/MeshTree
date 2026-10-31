//! Crash safety: `SIGKILL` the daemon mid-merge and restart it (`SPEC.md`
//! §10 Crash safety, §18 chaos tests, §20 Week 3, §22: recovery < 2 s, zero
//! orphaned `index.lock`).

mod common;

use common::{assert_exit, commit, git, meshtree, DaemonProc, TempDir};
use meshtree_core::{Task, TaskStatus};
use meshtree_platform::ipc::{Endpoint, IpcClient};
use meshtree_platform::wal;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

const INTEGRATION: &str = "refs/heads/meshtree/integration";
const PASS: &str = "git --version";

/// Runs long enough to be killed mid-`promote_validate`.
#[cfg(windows)]
const SLOW: &str = "ping -n 21 127.0.0.1 > NUL";
#[cfg(not(windows))]
const SLOW: &str = "sleep 20";

fn config(promote_validate: &str) -> String {
    format!(
        r#"[project]
base_branch = "main"
integration_branch = "meshtree/integration"
validate = ["{PASS}"]
promote_validate = ["{promote_validate}"]
"#
    )
}

fn repo(promote_validate: &str) -> TempDir {
    let repo = TempDir::repo_with_commit();
    assert_exit(&meshtree(repo.path(), &["init"]), 0);
    write_config(repo.path(), promote_validate);
    repo
}

fn write_config(root: &Path, promote_validate: &str) {
    fs::write(root.join(".meshtree/config.toml"), config(promote_validate)).unwrap();
}

/// Spawns a task claiming `claim` and commits one file inside it.
fn task_with_change(root: &Path, name: &str, claim: &str, file: &str) -> String {
    let out = meshtree(root, &["spawn", name, "--claim", claim]);
    assert_exit(&out, 0);
    let id = String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("task_id: "))
        .unwrap()
        .to_string();
    let wt = root.join(".meshtree/worktrees").join(&id);
    let path = wt.join(file);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, format!("{name}\n")).unwrap();
    git(&wt, &["add", file]);
    commit(&wt, name);
    id
}

fn task_path(root: &Path, id: &str) -> PathBuf {
    root.join(".meshtree/tasks").join(id).join("task.json")
}

fn read_task(root: &Path, id: &str) -> Task {
    serde_json::from_str(&fs::read_to_string(task_path(root, id)).unwrap()).unwrap()
}

fn rev(dir: &Path, r: &str) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--verify", "--quiet", r])
        .output()
        .unwrap();
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A lock file as a killed git process leaves it: present and old.
fn plant_lock(path: &Path) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let file = File::create(path).unwrap();
    file.set_modified(SystemTime::now() - Duration::from_secs(60))
        .unwrap();
}

#[test]
fn chaos_sigkill_daemon_mid_merge_recovers_under_2s() {
    let repo = repo(SLOW);
    let root = repo.path();
    let a = task_with_change(root, "task-a", "src/a/**", "src/a/one.txt");
    let main_tip = rev(root, "main").unwrap();
    let daemon = DaemonProc::start(root);

    // collect: validate passes, the merge is made, promote_validate hangs.
    let client = Command::new(env!("CARGO_BIN_EXE_meshtree"))
        .args(["collect", &a])
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let integration = root.join(".meshtree/integration");
    wait_until("the merge commit in the integration worktree", || {
        read_task(root, &a).status == TaskStatus::Merging && rev(&integration, "HEAD^2").is_some()
    });

    // Mid-merge SIGKILL.
    daemon.kill();
    let out = client.wait_with_output().unwrap();
    assert_ne!(out.status.code(), Some(0), "collect cannot succeed");
    assert_eq!(
        read_task(root, &a).status,
        TaskStatus::Merging,
        "crashed mid-merge"
    );

    // Orphaned git locks as killed git children would leave them.
    let gitdir = |dir: &Path| PathBuf::from(git(dir, &["rev-parse", "--absolute-git-dir"]).trim());
    let locks = [
        gitdir(&integration).join("index.lock"),
        gitdir(&root.join(".meshtree/worktrees").join(&a)).join("index.lock"),
        root.join(".git").join(format!("{INTEGRATION}.lock")),
        root.join(".git/refs/meshtree/coord.lock"),
    ];
    for lock in &locks {
        plant_lock(lock);
    }

    // Restart: recovery completes before the daemon listens, in < 2 s
    // (measured by the daemon; process launch is excluded).
    let start = Instant::now();
    let daemon = DaemonProc::start(root);
    let restart = start.elapsed();
    let ping = IpcClient::connect(&Endpoint::for_repo(root).unwrap())
        .unwrap()
        .call("ping", serde_json::json!({}))
        .unwrap();
    let recovery_ms = ping.result.unwrap()["recovery_ms"].as_u64().unwrap();
    assert!(
        recovery_ms < 2000,
        "recovery took {recovery_ms} ms (restart {restart:?})"
    );
    assert!(
        restart < Duration::from_secs(10),
        "restart took {restart:?}"
    );

    for lock in &locks {
        assert!(!lock.exists(), "orphaned lock left: {}", lock.display());
    }
    // The merge never reached the integration branch (the ref only moves
    // after promote_validate); the task is active again with its claims.
    assert_eq!(rev(root, INTEGRATION), None);
    assert_eq!(rev(&integration, "HEAD").unwrap(), main_tip);
    assert!(rev(&integration, "MERGE_HEAD").is_none());
    assert_eq!(git(&integration, &["status", "--porcelain"]), "");
    let task = read_task(root, &a);
    assert_eq!(task.status, TaskStatus::Active);
    assert_eq!(task.claims, ["src/a/**"]);
    assert!(root.join(&task.worktree).exists());

    // The WAL agrees with task.json and replays cleanly.
    let state = wal::replay(&wal::read(root).unwrap().entries);
    assert!(state.anomalies.is_empty(), "{:?}", state.anomalies);
    assert_eq!(state.tasks[&task.id].status, TaskStatus::Active);
    daemon.stop(root);

    // The recovered task collects normally once validation can pass.
    write_config(root, PASS);
    let daemon = DaemonProc::start(root);
    assert_exit(&meshtree(root, &["collect", &a]), 0);
    assert_eq!(read_task(root, &a).status, TaskStatus::Merged);
    let state = wal::replay(&wal::read(root).unwrap().entries);
    assert!(state.anomalies.is_empty(), "{:?}", state.anomalies);
    daemon.stop(root);
}

#[test]
fn wal_recovery_completes_landed_merge() {
    let repo = repo(PASS);
    let root = repo.path();
    let a = task_with_change(root, "task-a", "src/a/**", "src/a/one.txt");
    let b = task_with_change(root, "task-b", "src/b/**", "src/b/two.txt");

    // Crash states the daemon never journaled (no WAL yet): a's merge
    // landed (integration contains its branch) but a is still `merging`;
    // b was interrupted while `collecting`.
    let a_tip = rev(root, &format!("meshtree/{a}")).unwrap();
    git(root, &["update-ref", INTEGRATION, &a_tip]);
    for (id, status) in [(&a, "merging"), (&b, "collecting")] {
        let mut json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(task_path(root, id)).unwrap()).unwrap();
        json["status"] = status.into();
        fs::write(task_path(root, id), json.to_string()).unwrap();
    }

    let daemon = DaemonProc::start(root);
    let task_a = read_task(root, &a);
    assert_eq!(task_a.status, TaskStatus::Merged, "landed merge completed");
    assert!(task_a.claims.is_empty());
    assert!(!root.join(&task_a.worktree).exists());
    let task_b = read_task(root, &b);
    assert_eq!(
        task_b.status,
        TaskStatus::Active,
        "interrupted collect reactivated"
    );
    assert_eq!(task_b.claims, ["src/b/**"]);
    assert_eq!(
        rev(root, INTEGRATION).unwrap(),
        a_tip,
        "integration untouched"
    );

    // Both tasks were adopted into the WAL along their §8 paths.
    let state = wal::replay(&wal::read(root).unwrap().entries);
    assert!(state.anomalies.is_empty(), "{:?}", state.anomalies);
    assert_eq!(state.tasks[&task_a.id].status, TaskStatus::Merged);
    assert_eq!(state.tasks[&task_b.id].status, TaskStatus::Active);

    // a's claims were released in the coordination ref: its paths are free
    // for a new task.
    let out = meshtree(root, &["spawn", "task-c", "--claim", "src/a/**"]);
    assert_exit(&out, 0);
    daemon.stop(root);
}
