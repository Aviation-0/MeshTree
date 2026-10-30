//! Temp-repo integration tests for the `GitOps` CLI backend (T-001).
//! Authority: `SPEC.md` §4, §8, §9, Appendix A.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use meshtree_platform::gitops::{CliGitOps, GitError, GitOps};

/// A fresh temp git repo with one commit on `main` and a deterministic
/// identity (no reliance on the host's global git config). Stdlib-only so
/// the crate keeps zero dependencies; removed on drop.
struct TempRepo(PathBuf);

static COUNTER: AtomicU64 = AtomicU64::new(0);

impl TempRepo {
    fn new() -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "meshtree-gitops-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        let repo = Self(path);
        repo.git(&["init", "-b", "main"]);
        repo.git(&["config", "user.email", "meshtree-test@example.com"]);
        repo.git(&["config", "user.name", "MeshTree Test"]);
        repo.git(&["config", "commit.gpgsign", "false"]);
        repo.commit_file("README.md", "hello\n", "initial commit");
        repo
    }

    fn git(&self, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.0)
            .args(args)
            .output()
            .expect("spawn git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn commit_file(&self, name: &str, contents: &str, message: &str) {
        fs::write(self.0.join(name), contents).expect("write file");
        self.git(&["add", name]);
        self.git(&["commit", "-m", message]);
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempRepo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn gitops_worktree_add_list_remove() {
    let repo = TempRepo::new();
    let ops = CliGitOps::new(repo.path());

    let wt = repo.path().join("wt-t-001");
    ops.worktree_add(&wt, "meshtree/t-001", "main")
        .expect("worktree add");

    let list = ops.worktree_list().expect("worktree list");
    assert_eq!(list.len(), 2, "primary + new worktree");
    let added = list
        .iter()
        .find(|w| w.path == wt)
        .expect("new worktree listed");
    assert_eq!(added.branch.as_deref(), Some("refs/heads/meshtree/t-001"));
    assert_eq!(added.head, ops.rev_parse("main").unwrap());

    ops.worktree_remove(&wt).expect("worktree remove");
    let list = ops.worktree_list().expect("worktree list after remove");
    assert_eq!(list.len(), 1, "only the primary worktree remains");
    assert!(list.iter().all(|w| w.path != wt));
}

#[test]
fn gitops_ref_read_and_rev_parse() {
    let repo = TempRepo::new();
    let ops = CliGitOps::new(repo.path());

    let head = ops.rev_parse("HEAD").expect("rev-parse HEAD");
    let main = ops
        .ref_read("refs/heads/main")
        .expect("ref_read main")
        .expect("main exists");
    assert_eq!(head, main, "HEAD and refs/heads/main resolve identically");

    let missing = ops
        .ref_read("refs/heads/does-not-exist")
        .expect("ref_read missing ref");
    assert!(missing.is_none(), "missing ref reads as None, not an error");
}

#[test]
fn gitops_update_ref_cas_accepts_and_rejects() {
    let repo = TempRepo::new();
    let ops = CliGitOps::new(repo.path());

    let head1 = ops.rev_parse("HEAD").unwrap();
    // Create the coordination ref (Appendix A: `git update-ref <ref> <oid>`).
    ops.update_ref("refs/meshtree/coord", &head1, None)
        .expect("create ref without CAS");
    assert_eq!(
        ops.ref_read("refs/meshtree/coord").unwrap().as_deref(),
        Some(head1.as_str())
    );

    repo.commit_file("a.txt", "a\n", "second commit");
    let head2 = ops.rev_parse("HEAD").unwrap();
    assert_ne!(head1, head2);

    // CAS with the correct expected old OID is accepted.
    ops.update_ref("refs/meshtree/coord", &head2, Some(&head1))
        .expect("CAS with matching expected old");
    assert_eq!(
        ops.ref_read("refs/meshtree/coord").unwrap().as_deref(),
        Some(head2.as_str())
    );

    // CAS with a stale expected old OID is rejected.
    let err = ops
        .update_ref("refs/meshtree/coord", &head1, Some(&head1))
        .expect_err("CAS with stale expected old must fail");
    let is_cas_rejected = matches!(
        &err,
        GitError::CasRejected {
            reference,
            expected_old
        } if reference == "refs/meshtree/coord" && expected_old == &head1
    );
    assert!(is_cas_rejected, "expected CasRejected, got: {err}");
    // The ref still holds the CAS winner's value.
    assert_eq!(
        ops.ref_read("refs/meshtree/coord").unwrap().as_deref(),
        Some(head2.as_str())
    );
}
