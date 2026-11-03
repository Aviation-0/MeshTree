//! A coordination sync round costs the same number of git subprocesses
//! however long the event log is (`SPEC.md` §9 daemon claim path, §18;
//! `ADR-0009` Part 1).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use meshtree_core::lease::{EventType, LeaseEvent, ReplayConfig};
use meshtree_core::task::{TaskId, TaskIdAllocator};
use meshtree_core::time::Timestamp;
use meshtree_platform::coord::CoordStore;
use meshtree_platform::gitops::{CliGitOps, GitError, GitOps, TreeEntry, WorktreeInfo};

const CONFIG: ReplayConfig = ReplayConfig {
    default_ttl_secs: 900,
    skew_grace_secs: 600,
};

struct TempRepo(PathBuf);

static COUNTER: AtomicU64 = AtomicU64::new(0);

impl TempRepo {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "meshtree-cost-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        let out = Command::new("git")
            .arg("-C")
            .arg(&path)
            .args(["init", "-q", "--initial-branch=main"])
            .output()
            .unwrap();
        assert!(out.status.success());
        Self(path)
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

/// Counts every git invocation; `batched = false` falls back to the
/// one-blob-at-a-time trait defaults (the pre-`ADR-0009` behavior).
struct Counting {
    inner: CliGitOps,
    calls: AtomicUsize,
    batched: bool,
}

impl Counting {
    fn new(repo: &TempRepo, batched: bool) -> Self {
        Self {
            inner: CliGitOps::new(repo.path()),
            calls: AtomicUsize::new(0),
            batched,
        }
    }

    fn tick(&self) {
        self.calls.fetch_add(1, Ordering::SeqCst);
    }

    fn take(&self) -> usize {
        self.calls.swap(0, Ordering::SeqCst)
    }
}

impl GitOps for Counting {
    fn worktree_add(&self, path: &Path, branch: &str, base: &str) -> Result<(), GitError> {
        self.tick();
        self.inner.worktree_add(path, branch, base)
    }
    fn worktree_remove(&self, path: &Path) -> Result<(), GitError> {
        self.tick();
        self.inner.worktree_remove(path)
    }
    fn worktree_list(&self) -> Result<Vec<WorktreeInfo>, GitError> {
        self.tick();
        self.inner.worktree_list()
    }
    fn rev_parse(&self, rev: &str) -> Result<String, GitError> {
        self.tick();
        self.inner.rev_parse(rev)
    }
    fn ref_read(&self, reference: &str) -> Result<Option<String>, GitError> {
        self.tick();
        self.inner.ref_read(reference)
    }
    fn update_ref(&self, r: &str, new: &str, old: Option<&str>) -> Result<(), GitError> {
        self.tick();
        self.inner.update_ref(r, new, old)
    }
    fn remote_url(&self, name: &str) -> Result<Option<String>, GitError> {
        self.tick();
        self.inner.remote_url(name)
    }
    fn hash_object(&self, contents: &[u8]) -> Result<String, GitError> {
        self.tick();
        self.inner.hash_object(contents)
    }
    fn mktree(&self, entries: &[TreeEntry]) -> Result<String, GitError> {
        self.tick();
        self.inner.mktree(entries)
    }
    fn commit_tree(&self, tree: &str, parents: &[&str], msg: &str) -> Result<String, GitError> {
        self.tick();
        self.inner.commit_tree(tree, parents, msg)
    }
    fn ls_tree(&self, treeish: &str) -> Result<Vec<TreeEntry>, GitError> {
        self.tick();
        self.inner.ls_tree(treeish)
    }
    fn cat_blob(&self, oid: &str) -> Result<Vec<u8>, GitError> {
        self.tick();
        self.inner.cat_blob(oid)
    }
    fn cat_blobs(&self, oids: &[String]) -> Result<Vec<Vec<u8>>, GitError> {
        if !self.batched {
            return oids.iter().map(|oid| self.cat_blob(oid)).collect();
        }
        if !oids.is_empty() {
            self.tick();
        }
        self.inner.cat_blobs(oids)
    }
    fn hash_objects(&self, contents: &[&[u8]]) -> Result<Vec<String>, GitError> {
        if !self.batched {
            return contents.iter().map(|c| self.hash_object(c)).collect();
        }
        if !contents.is_empty() {
            self.tick();
        }
        self.inner.hash_objects(contents)
    }
    fn fetch_ref(&self, remote: &str, src: &str, dst: &str) -> Result<Option<String>, GitError> {
        self.tick();
        self.inner.fetch_ref(remote, src, dst)
    }
    fn push_ref_cas(
        &self,
        remote: &str,
        reference: &str,
        new_oid: &str,
        expected_old: Option<&str>,
    ) -> Result<(), GitError> {
        self.tick();
        self.inner
            .push_ref_cas(remote, reference, new_oid, expected_old)
    }
}

/// `n` ACQUIREs on distinct paths (so the lease table grows with the log),
/// ids allocated in order from `start_ms`.
fn events(task: &TaskId, start_ms: u64, n: usize, prefix: &str) -> Vec<LeaseEvent> {
    let mut alloc = TaskIdAllocator::new();
    (0..n)
        .map(|i| {
            let ms = start_ms + i as u64;
            LeaseEvent {
                id: alloc.allocate(ms, i as u128 + 1).as_str()[2..].to_string(),
                kind: EventType::Acquire,
                path: format!("{prefix}/{i}/**"),
                task: task.clone(),
                machine: "mc-bench".to_string(),
                ts: Timestamp::from_unix_ms(ms),
                counter: i as u64,
                ttl_secs: Some(900),
            }
        })
        .collect()
}

/// Git invocations of one claim round (one new ACQUIRE) on a log of `n`
/// events, plus the resulting tree.
fn claim_round_cost(n: usize, batched: bool) -> (usize, String) {
    let repo = TempRepo::new();
    let git = Counting::new(&repo, batched);
    let task = TaskId::from_ulid(7);
    let store = CoordStore::new(git, None, CONFIG);
    store.sync(&events(&task, 1_000_000, n, "seed")).unwrap();
    store.git().take();
    let claim = events(&task, 2_000_000, 1, "claim");
    let outcome = store.sync(&claim).unwrap();
    let calls = store.git().take();
    let tree = store
        .git()
        .rev_parse(&format!("{}^{{tree}}", outcome.oid))
        .unwrap();
    (calls, tree)
}

#[test]
fn coord_sync_subprocess_count_independent_of_log_length() {
    let (small, small_tree) = claim_round_cost(100, true);
    let (large, large_tree) = claim_round_cost(2_000, true);
    assert_eq!(
        small, large,
        "git invocations per claim round: {small} at 100 events, {large} at 2,000"
    );
    assert!(small <= 12, "{small} git invocations for one claim round");

    // Same bytes as the one-blob-at-a-time path: batching changes the cost,
    // never the coordination tree (§9 byte-identical replay).
    let (unbatched, unbatched_tree) = claim_round_cost(100, false);
    assert_eq!(unbatched_tree, small_tree);
    assert!(
        unbatched > small,
        "unbatched {unbatched} vs batched {small}"
    );
    assert_ne!(small_tree, large_tree);
}
