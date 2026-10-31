//! Shared helpers for `meshtree` CLI integration tests. Stdlib-only,
//! mirroring the `meshtree-platform` gitops tests.

// Each test binary uses a different subset of these helpers.
#![allow(dead_code)]

use meshtree_platform::ipc::{Endpoint, IpcClient};
use serde_json::json;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Temporary directory removed on drop.
pub struct TempDir(PathBuf);

static COUNTER: AtomicU64 = AtomicU64::new(0);

impl TempDir {
    pub fn new() -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "meshtree-cli-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    /// A temp directory holding a fresh `git init` repository on `main`.
    pub fn repo() -> Self {
        let dir = Self::new();
        git(dir.path(), &["init", "-q", "-b", "main"]);
        dir
    }

    /// Like [`TempDir::repo`], with one commit on `main` adding `README.md`.
    pub fn repo_with_commit() -> Self {
        let dir = Self::repo();
        fs::write(dir.path().join("README.md"), "hello\n").unwrap();
        git(dir.path(), &["add", "README.md"]);
        commit(dir.path(), "initial");
        dir
    }

    /// A bare repository standing in for the GitHub remote.
    pub fn bare() -> Self {
        let dir = Self::new();
        git(dir.path(), &["init", "-q", "--bare"]);
        dir
    }

    /// `file://` URL for this directory (`file:///C:/…` on Windows).
    pub fn file_url(&self) -> String {
        let path = self.0.to_string_lossy().replace('\\', "/");
        if path.starts_with('/') {
            format!("file://{path}")
        } else {
            format!("file:///{path}")
        }
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Runs `git -C <dir> <args>`, asserting success; returns stdout.
pub fn git(dir: &Path, args: &[&str]) -> String {
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

/// Commits staged changes with a fixed test identity (no host git config).
pub fn commit(dir: &Path, message: &str) {
    git(
        dir,
        &[
            "-c",
            "user.name=MeshTree Test",
            "-c",
            "user.email=test@meshtree.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            message,
        ],
    );
}

/// Runs the `meshtree` binary in `cwd`.
pub fn meshtree(cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_meshtree"))
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("run meshtree")
}

/// Asserts the exit code, printing both streams on mismatch.
pub fn assert_exit(out: &Output, code: i32) {
    assert_eq!(
        out.status.code(),
        Some(code),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A running `meshtree daemon`, killed on drop if still alive. Its stdout
/// stays open (`_stdout`) for the daemon's lifetime.
pub struct DaemonProc {
    child: Child,
    _stdout: BufReader<ChildStdout>,
}

impl DaemonProc {
    /// Starts the daemon in `repo` and waits for its "listening" line.
    pub fn start(repo: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_meshtree"))
            .arg("daemon")
            .current_dir(repo)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn daemon");
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        if !line.contains("listening on") {
            let _ = child.kill();
            let out = child.wait_with_output().unwrap();
            panic!(
                "daemon did not start: {line:?} / {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Self {
            child,
            _stdout: stdout,
        }
    }

    /// The daemon's pid.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Kills the daemon without warning (`SIGKILL` / `TerminateProcess`).
    pub fn kill(mut self) {
        self.child.kill().expect("kill daemon");
        self.child.wait().expect("reap daemon");
    }

    /// Sends `shutdown` and waits for a clean exit.
    pub fn stop(mut self, repo: &Path) {
        let endpoint = Endpoint::for_repo(repo).unwrap();
        let resp = IpcClient::connect(&endpoint)
            .expect("daemon reachable")
            .call("shutdown", json!({}))
            .unwrap();
        assert!(resp.ok, "{resp:?}");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "daemon exit status {status}");
                return;
            }
            assert!(Instant::now() < deadline, "daemon did not stop");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for DaemonProc {
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// `meshtree spawn <name>` (asserting success); returns the task id.
pub fn spawn_task(repo: &Path, name: &str) -> String {
    let out = meshtree(repo, &["spawn", name]);
    assert_exit(&out, 0);
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("task_id: "))
        .expect("spawn prints task_id")
        .to_string()
}
