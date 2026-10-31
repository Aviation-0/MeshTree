//! Temp-repo integration tests for the `GitOps` CLI backend (T-001).
//! Authority: `SPEC.md` §4, §8, §9, Appendix A.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use meshtree_platform::gitops::{CliGitOps, GitError, GitOps, TreeEntry, ERROR_CODE_CAS_REJECTED};

/// A fresh temp git repo with one commit on `main` and a deterministic
/// identity (no reliance on the host's global git config). Stdlib-only so
/// the crate keeps zero dependencies; removed on drop.
struct TempRepo(PathBuf);

static COUNTER: AtomicU64 = AtomicU64::new(0);

impl TempRepo {
    fn temp_path() -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "meshtree-gitops-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    /// A bare repository standing in for the GitHub remote.
    fn bare() -> Self {
        let repo = Self(Self::temp_path());
        repo.git(&["init", "--bare", "-q"]);
        repo
    }

    /// `file://` URL for this repository (`file:///C:/…` on Windows).
    fn file_url(&self) -> String {
        let path = self.0.to_string_lossy().replace('\\', "/");
        if path.starts_with('/') {
            format!("file://{path}")
        } else {
            format!("file:///{path}")
        }
    }

    fn new() -> Self {
        let repo = Self(Self::temp_path());
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
    // Compare canonicalized paths: git realpaths worktree entries, which
    // differs from `temp_dir()` on macOS (/var → /private/var) and on
    // Windows runners (8.3 short names).
    let wt_canon = fs::canonicalize(&wt).expect("canonicalize new worktree");
    let canon_eq = |p: &Path| fs::canonicalize(p).map(|c| c == wt_canon).unwrap_or(false);
    let added = list
        .iter()
        .find(|w| canon_eq(&w.path))
        .expect("new worktree listed");
    assert_eq!(added.branch.as_deref(), Some("refs/heads/meshtree/t-001"));
    assert_eq!(added.head, ops.rev_parse("main").unwrap());

    ops.worktree_remove(&wt).expect("worktree remove");
    let list = ops.worktree_list().expect("worktree list after remove");
    assert_eq!(list.len(), 1, "only the primary worktree remains");
    assert!(list.iter().all(|w| !canon_eq(&w.path)));
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

const COORD: &str = "refs/meshtree/coord";
const TRACKING: &str = "refs/meshtree/remotes/origin/coord";

/// Builds an orphan-ref commit whose tree is `log/<name>` = `body`
/// (hash-object → mktree ×2 → commit-tree, `SPEC.md` Appendix A).
fn coord_commit(ops: &CliGitOps, name: &str, body: &str, parents: &[&str]) -> String {
    let blob = ops.hash_object(body.as_bytes()).expect("hash-object");
    let log = ops
        .mktree(&[TreeEntry::blob(name, blob)])
        .expect("mktree log/");
    let root = ops
        .mktree(&[TreeEntry::tree("log", log)])
        .expect("mktree root");
    ops.commit_tree(&root, parents, "meshtree: coord")
        .expect("commit-tree")
}

fn assert_mt003(err: GitError) {
    assert_eq!(err.code(), Some(ERROR_CODE_CAS_REJECTED), "{err}");
    assert!(
        matches!(err, GitError::RemoteCasRejected { ref reference, .. } if reference == COORD),
        "{err}"
    );
}

#[test]
fn gitops_remote_cas_push_accepts_and_rejects_stale_oid() {
    let remote = TempRepo::bare();
    let (a, b) = (TempRepo::new(), TempRepo::new());
    for repo in [&a, &b] {
        repo.git(&["remote", "add", "origin", &remote.file_url()]);
    }
    let (ga, gb) = (CliGitOps::new(a.path()), CliGitOps::new(b.path()));

    // Absent remote ref: fetch reports None and leaves the tracking ref alone.
    assert_eq!(gb.fetch_ref("origin", COORD, TRACKING).unwrap(), None);
    assert_eq!(gb.ref_read(TRACKING).unwrap(), None);

    // A creates the orphan ref (expected old = absent).
    let c1 = coord_commit(&ga, "01A.json", "{\"a\":1}\n", &[]);
    ga.push_ref_cas("origin", COORD, &c1, None)
        .expect("create on empty remote");

    // B also expects "absent": stale → MT-003.
    let b1 = coord_commit(&gb, "01B.json", "{\"b\":1}\n", &[]);
    assert_mt003(
        gb.push_ref_cas("origin", COORD, &b1, None)
            .expect_err("create over existing ref"),
    );

    // B fetches A's commit and reads the tree back through plumbing.
    assert_eq!(
        gb.fetch_ref("origin", COORD, TRACKING).unwrap().as_deref(),
        Some(c1.as_str())
    );
    let entries = gb.ls_tree(&c1).expect("ls-tree");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].path, "log/01A.json");
    assert_eq!(entries[0].kind, "blob");
    assert_eq!(gb.cat_blob(&entries[0].oid).unwrap(), b"{\"a\":1}\n");

    // A advances with the correct lease.
    let c2 = coord_commit(&ga, "01C.json", "{}\n", &[&c1]);
    ga.push_ref_cas("origin", COORD, &c2, Some(&c1))
        .expect("CAS with current expected old");

    // B's lease on c1 is now stale → MT-003; the remote keeps A's value.
    let b2 = coord_commit(&gb, "01B.json", "{\"b\":1}\n", &[&c1]);
    assert_mt003(
        gb.push_ref_cas("origin", COORD, &b2, Some(&c1))
            .expect_err("CAS with stale expected old"),
    );
    assert_eq!(
        gb.fetch_ref("origin", COORD, TRACKING).unwrap().as_deref(),
        Some(c2.as_str())
    );

    // B retries on top of the winner and succeeds.
    let b3 = coord_commit(&gb, "01B.json", "{\"b\":1}\n", &[&c2]);
    gb.push_ref_cas("origin", COORD, &b3, Some(&c2))
        .expect("retry on fresh expected old");
    assert_eq!(
        ga.fetch_ref("origin", COORD, TRACKING).unwrap().as_deref(),
        Some(b3.as_str())
    );
}
