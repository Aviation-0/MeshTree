//! Integration tests for `meshtree init` (`SPEC.md` §5, §6, §14, §16).

use meshtree_core::{Config, ERROR_CODE_CONFIG_INVALID};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

/// Temporary directory removed on drop. Stdlib-only, mirroring the
/// `meshtree-platform` gitops tests.
struct TempDir(PathBuf);

static COUNTER: AtomicU64 = AtomicU64::new(0);

impl TempDir {
    fn new() -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "meshtree-init-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    /// A temp directory holding a fresh `git init` repository.
    fn repo() -> Self {
        let dir = Self::new();
        git(dir.path(), &["init", "-q", "-b", "main"]);
        dir
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf-8 git output")
}

fn meshtree_init(cwd: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_meshtree"))
        .arg("init")
        .current_dir(cwd)
        .output()
        .expect("run meshtree init")
}

fn assert_exit(out: &Output, code: i32) {
    assert_eq!(
        out.status.code(),
        Some(code),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn init_writes_valid_template() {
    let repo = TempDir::repo();
    let out = meshtree_init(repo.path());
    assert_exit(&out, 0);

    let mt = repo.path().join(".meshtree");
    let text = fs::read_to_string(mt.join("config.toml")).expect("config.toml written");
    assert_eq!(text, Config::template());
    let config = Config::from_toml_str(&text).expect("template must validate");
    assert_eq!(config.project.base_branch, "main");

    for sub in ["worktrees", "tasks", "quarantine"] {
        assert!(mt.join(sub).is_dir(), ".meshtree/{sub} must exist");
    }

    // Only the committed files (§5/§6) are visible to git; state stays ignored.
    fs::write(mt.join("daemon.lock"), "pid").unwrap();
    fs::create_dir_all(mt.join("tasks/t-x")).unwrap();
    fs::write(mt.join("tasks/t-x/task.json"), "{}").unwrap();
    fs::write(mt.join("tasks/t-x/HANDOFF.md"), "# handoff").unwrap();
    let status = git(
        repo.path(),
        &["status", "--porcelain", "--untracked-files=all"],
    );
    let mut untracked: Vec<&str> = status.lines().collect();
    untracked.sort_unstable();
    assert_eq!(
        untracked,
        [
            "?? .meshtree/.gitignore",
            "?? .meshtree/config.toml",
            "?? .meshtree/tasks/t-x/HANDOFF.md",
        ]
    );
}

#[test]
fn init_twice_is_idempotent() {
    let repo = TempDir::repo();
    assert_exit(&meshtree_init(repo.path()), 0);

    // A user edit to the committed config must survive a second init.
    let config_path = repo.path().join(".meshtree/config.toml");
    let edited = Config::template().replace("ttl_secs = 900", "ttl_secs = 1200");
    assert_ne!(edited, Config::template());
    fs::write(&config_path, &edited).unwrap();
    let gitignore = fs::read(repo.path().join(".meshtree/.gitignore")).unwrap();

    let second = meshtree_init(repo.path());
    assert_exit(&second, 0);
    assert!(String::from_utf8_lossy(&second.stdout).contains("kept existing"));
    assert_eq!(fs::read_to_string(&config_path).unwrap(), edited);
    assert_eq!(
        fs::read(repo.path().join(".meshtree/.gitignore")).unwrap(),
        gitignore
    );

    let third = meshtree_init(repo.path());
    assert_exit(&third, 0);
    assert_eq!(third.stdout, second.stdout);
}

#[test]
fn init_from_subdirectory_targets_repo_root() {
    let repo = TempDir::repo();
    let nested = repo.path().join("src/deep");
    fs::create_dir_all(&nested).unwrap();

    assert_exit(&meshtree_init(&nested), 0);
    assert!(repo.path().join(".meshtree/config.toml").is_file());
    assert!(!nested.join(".meshtree").exists());
}

#[test]
fn init_existing_invalid_config_reports_mt050_and_keeps_file() {
    let repo = TempDir::repo();
    let mt = repo.path().join(".meshtree");
    fs::create_dir_all(&mt).unwrap();
    let invalid = "[lease]\nttl_secs = 0\n";
    fs::write(mt.join("config.toml"), invalid).unwrap();

    let out = meshtree_init(repo.path());
    assert_exit(&out, 10);
    assert!(String::from_utf8_lossy(&out.stderr).contains(ERROR_CODE_CONFIG_INVALID));
    assert_eq!(fs::read_to_string(mt.join("config.toml")).unwrap(), invalid);
}

#[test]
fn init_outside_git_repository_exits_usage() {
    let dir = TempDir::new();
    if dir.path().ancestors().any(|d| d.join(".git").exists()) {
        // Temp dir lives inside some repository on this host; nothing to assert.
        return;
    }
    let out = meshtree_init(dir.path());
    assert_exit(&out, 10);
    assert!(!dir.path().join(".meshtree").exists());
}

#[test]
fn unknown_subcommand_exits_usage() {
    let out = Command::new(env!("CARGO_BIN_EXE_meshtree"))
        .arg("no-such-command")
        .output()
        .expect("run meshtree");
    assert_exit(&out, 10);
}
