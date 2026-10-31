//! Shared helpers for `meshtree` CLI integration tests. Stdlib-only,
//! mirroring the `meshtree-platform` gitops tests.

// Each test binary uses a different subset of these helpers.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

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
