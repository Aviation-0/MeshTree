//! Serialized merge gate: `meshtree collect` (via the daemon) and
//! `meshtree promote` (`SPEC.md` §2 G4, §8 `collect`, §14, §16 `MT-030`–`MT-032`).

mod common;

use common::{assert_exit, commit, git, meshtree, spawn_task, DaemonProc, TempDir};
use meshtree_core::{Task, TaskStatus};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

const INTEGRATION: &str = "refs/heads/meshtree/integration";

fn config(validate: &str, promote_validate: &str) -> String {
    format!(
        r#"[project]
base_branch = "main"
integration_branch = "meshtree/integration"
validate = ["{validate}"]
promote_validate = ["{promote_validate}"]
"#
    )
}

const PASS: &str = "git --version";
const FAIL: &str = "git definitely-not-a-git-command";

fn repo(validate: &str, promote_validate: &str) -> TempDir {
    let repo = TempDir::repo_with_commit();
    assert_exit(&meshtree(repo.path(), &["init"]), 0);
    fs::write(
        repo.path().join(".meshtree/config.toml"),
        config(validate, promote_validate),
    )
    .unwrap();
    repo
}

fn spawn_claiming(root: &Path, name: &str, claim: &str) -> String {
    let out = meshtree(root, &["spawn", name, "--claim", claim]);
    assert_exit(&out, 0);
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("task_id: "))
        .unwrap()
        .to_string()
}

fn worktree(root: &Path, id: &str) -> PathBuf {
    root.join(".meshtree/worktrees").join(id)
}

/// Writes `path` = `body` in the task worktree and commits it on the task branch.
fn change(root: &Path, id: &str, path: &str, body: &str) {
    let wt = worktree(root, id);
    let file = wt.join(path);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(file, body).unwrap();
    git(&wt, &["add", path]);
    commit(&wt, &format!("{id}: {path}"));
}

fn read_task(root: &Path, id: &str) -> Task {
    let path = root.join(".meshtree/tasks").join(id).join("task.json");
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

fn rev(root: &Path, r: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--verify", "--quiet", r])
        .output()
        .unwrap();
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn is_ancestor(root: &Path, a: &str, b: &str) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["merge-base", "--is-ancestor", a, b])
        .status()
        .unwrap()
        .success()
}

#[test]
fn collect_merges_and_promotes_cleanly() {
    let repo = repo(PASS, PASS);
    let root = repo.path();
    let a = spawn_claiming(root, "task-a", "src/a/**");
    let b = spawn_claiming(root, "task-b", "src/b/**");
    change(root, &a, "src/a/one.txt", "a\n");
    change(root, &b, "src/b/two.txt", "b\n");
    let a_tip = rev(root, &format!("meshtree/{a}")).unwrap();
    let main_before = rev(root, "main").unwrap();

    // Without a daemon, collect is MT-010 (exit 30).
    let out = meshtree(root, &["collect", &a]);
    assert_exit(&out, 30);
    assert!(stderr(&out).contains("MT-010"));

    let daemon = DaemonProc::start(root);
    let out = meshtree(root, &["collect", &a]);
    assert_exit(&out, 0);
    assert!(String::from_utf8_lossy(&out.stdout).contains("merged"));

    // a is merged: claims released, worktree removed, integration holds it,
    // main untouched until promote.
    let task = read_task(root, &a);
    assert_eq!(task.status, TaskStatus::Merged);
    assert!(task.claims.is_empty());
    assert!(!worktree(root, &a).exists());
    let integration = rev(root, INTEGRATION).expect("integration branch created");
    assert!(is_ancestor(root, &a_tip, &integration));
    assert_eq!(rev(root, "main").unwrap(), main_before);

    // Collecting a merged task is an illegal transition (MT-004).
    let out = meshtree(root, &["collect", &a]);
    assert_exit(&out, 20);
    assert!(stderr(&out).contains("MT-004"), "{}", stderr(&out));

    // promote fast-forwards the checked-out main, working tree included.
    let out = meshtree(root, &["promote"]);
    assert_exit(&out, 0);
    assert_eq!(rev(root, "main").unwrap(), integration);
    assert_eq!(
        fs::read_to_string(root.join("src/a/one.txt")).unwrap(),
        "a\n"
    );
    assert_exit(&meshtree(root, &["promote"]), 0); // up to date

    // b merges on top; promote again.
    assert_exit(&meshtree(root, &["collect", &b]), 0);
    assert_eq!(read_task(root, &b).status, TaskStatus::Merged);
    let integration2 = rev(root, INTEGRATION).unwrap();
    assert!(is_ancestor(root, &integration, &integration2));
    assert_exit(&meshtree(root, &["promote"]), 0);
    assert!(root.join("src/b/two.txt").exists());

    // Two unclaimed edits of the same file: the second merge conflicts
    // (MT-031) and its task returns to active, integration unchanged.
    let c = spawn_task(root, "task-c");
    let d = spawn_task(root, "task-d");
    change(root, &c, "README.md", "from c\n");
    change(root, &d, "README.md", "from d\n");
    assert_exit(&meshtree(root, &["collect", &c]), 0);
    let before_d = rev(root, INTEGRATION).unwrap();
    let out = meshtree(root, &["collect", &d]);
    assert_exit(&out, 20);
    assert!(stderr(&out).contains("MT-031"), "{}", stderr(&out));
    assert!(stderr(&out).contains("README.md"), "{}", stderr(&out));
    assert_eq!(read_task(root, &d).status, TaskStatus::Active);
    assert_eq!(rev(root, INTEGRATION).unwrap(), before_d);

    // main checked out elsewhere: promote moves the ref only.
    git(root, &["checkout", "-q", "--detach"]);
    let out = meshtree(root, &["promote"]);
    assert_exit(&out, 0);
    assert_eq!(rev(root, "main").unwrap(), before_d);

    daemon.stop(root);
}

#[test]
fn collect_rejects_claim_overlap_mt032() {
    let repo = repo(PASS, PASS);
    let root = repo.path();
    let a = spawn_claiming(root, "task-a", "src/auth/**");
    let b = spawn_claiming(root, "task-b", "docs/**");
    // b edits inside a's claim.
    change(root, &b, "docs/guide.md", "guide\n");
    change(root, &b, "src/auth/login.rs", "// b\n");
    let daemon = DaemonProc::start(root);

    let out = meshtree(root, &["collect", &b]);
    assert_exit(&out, 20);
    let err = stderr(&out);
    assert!(err.contains("MT-032"), "{err}");
    assert!(err.contains(&a), "names the other task: {err}");
    assert!(err.contains("src/auth/login.rs"), "{err}");

    let task_b = read_task(root, &b);
    assert_eq!(task_b.status, TaskStatus::Failed);
    assert!(task_b.claims.is_empty(), "failed task's claims released");
    assert_eq!(rev(root, INTEGRATION), None, "nothing merged");
    let task_a = read_task(root, &a);
    assert_eq!(task_a.status, TaskStatus::Active);
    assert_eq!(task_a.claims, ["src/auth/**"]);

    // a's own change inside its claim merges fine.
    change(root, &a, "src/auth/login.rs", "// a\n");
    assert_exit(&meshtree(root, &["collect", &a]), 0);
    daemon.stop(root);
}

#[test]
fn collect_returns_active_on_validate_fail_mt030() {
    // Task validation fails.
    let repo = repo(FAIL, PASS);
    let root = repo.path();
    let a = spawn_claiming(root, "task-a", "src/a/**");
    change(root, &a, "src/a/x.txt", "x\n");
    let daemon = DaemonProc::start(root);
    let out = meshtree(root, &["collect", &a]);
    assert_exit(&out, 20);
    let err = stderr(&out);
    assert!(err.contains("MT-030"), "{err}");
    assert!(err.contains(FAIL), "names the command: {err}");
    let task = read_task(root, &a);
    assert_eq!(task.status, TaskStatus::Active);
    assert_eq!(task.claims, ["src/a/**"], "claims kept for the retry");
    assert!(worktree(root, &a).exists());
    daemon.stop(root);

    // Integration validation fails: the merge is discarded, the integration
    // branch does not move, and the task is active again.
    fs::write(root.join(".meshtree/config.toml"), config(PASS, FAIL)).unwrap();
    let daemon = DaemonProc::start(root);
    let before = rev(root, INTEGRATION);
    let out = meshtree(root, &["collect", &a]);
    assert_exit(&out, 20);
    assert!(stderr(&out).contains("MT-030"), "{}", stderr(&out));
    assert_eq!(read_task(root, &a).status, TaskStatus::Active);
    assert_eq!(rev(root, INTEGRATION), before);
    if let Some(tip) = &before {
        assert!(!is_ancestor(root, &format!("meshtree/{a}"), tip));
    }
    daemon.stop(root);

    // Fixed config: the retry merges.
    fs::write(root.join(".meshtree/config.toml"), config(PASS, PASS)).unwrap();
    let daemon = DaemonProc::start(root);
    assert_exit(&meshtree(root, &["collect", &a]), 0);
    assert_eq!(read_task(root, &a).status, TaskStatus::Merged);
    daemon.stop(root);
}
