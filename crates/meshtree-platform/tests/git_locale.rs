//! Every `git` subprocess runs untranslated (`LC_ALL=C`) so stderr
//! classification — notably the lost push race (`SPEC.md` §9, §16 `MT-003`) —
//! does not depend on the user's locale (T-044).
//!
//! This binary holds a single test because it mutates the process
//! environment to simulate a foreign-locale user.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use meshtree_platform::gitops::{CliGitOps, GitError, GitOps, ERROR_CODE_CAS_REJECTED};

const COORD: &str = "refs/meshtree/coord";

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!("mt-locale-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn git(&self, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.0)
            .args(args)
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn file_url(&self) -> String {
        let path = self.0.to_string_lossy().replace('\\', "/");
        if path.starts_with('/') {
            format!("file://{path}")
        } else {
            format!("file:///{path}")
        }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn git_runs_with_c_locale_under_foreign_lang() {
    // A German-speaking user (translations are used wherever git ships them).
    for (key, value) in [
        ("LANG", "de_DE.UTF-8"),
        ("LC_ALL", "de_DE.UTF-8"),
        ("LC_MESSAGES", "de_DE.UTF-8"),
        ("LANGUAGE", "de"),
    ] {
        std::env::set_var(key, value);
    }

    let remote = TempDir::new("remote");
    let clone = TempDir::new("clone");
    remote.git(&["init", "-q", "--bare"]);
    clone.git(&["init", "-q"]);
    clone.git(&["remote", "add", "origin", &remote.file_url()]);

    // A pre-push hook records the locale git's children actually see.
    let hook = clone.path().join(".git").join("hooks").join("pre-push");
    fs::write(
        &hook,
        "#!/bin/sh\nprintf '%s' \"$LC_ALL\" > locale-seen\nexit 0\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    }

    let ops = CliGitOps::new(clone.path());
    let tree = ops.mktree(&[]).unwrap();
    let first = ops.commit_tree(&tree, &[], "first").unwrap();
    let second = ops.commit_tree(&tree, &[first.as_str()], "second").unwrap();

    ops.push_ref_cas("origin", COORD, &first, None)
        .expect("first push creates the ref");
    assert_eq!(
        fs::read_to_string(clone.path().join("locale-seen")).unwrap(),
        "C",
        "git (and its hooks) must run with LC_ALL=C"
    );

    // Losing the race is still recognized: the stale lease is classified as
    // MT-003, not as a network failure.
    let err = ops
        .push_ref_cas("origin", COORD, &second, None)
        .expect_err("ref exists; expected-absent lease is stale");
    assert!(
        matches!(err, GitError::RemoteCasRejected { .. }),
        "lost race misclassified under a foreign locale: {err:?}"
    );
    assert_eq!(err.code(), Some(ERROR_CODE_CAS_REJECTED));

    // Other failures surface git's untranslated message.
    let err = ops
        .fetch_ref("nowhere", COORD, "refs/meshtree/remotes/nowhere/coord")
        .expect_err("no such remote");
    match err {
        GitError::NonZeroExit { stderr, .. } => assert!(
            stderr.contains("does not appear to be a git repository"),
            "expected English git output, got: {stderr}"
        ),
        other => panic!("expected NonZeroExit, got {other:?}"),
    }
}
