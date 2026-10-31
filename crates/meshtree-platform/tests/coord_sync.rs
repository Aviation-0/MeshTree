//! Two-clone `file://` CAS race on `refs/meshtree/coord` (`SPEC.md` §9, §16, §18).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier};

use meshtree_core::lease::coord::lease_file;
use meshtree_core::lease::{EventType, LeaseEvent, ReplayConfig, ERROR_CODE_CLAIM_CONFLICT};
use meshtree_core::task::{TaskId, TaskIdAllocator};
use meshtree_core::time::Timestamp;
use meshtree_platform::coord::{CoordStore, COORD_REF, MAX_ATTEMPTS};
use meshtree_platform::gitops::{CliGitOps, GitError, GitOps, TreeEntry, WorktreeInfo};

const CONFIG: ReplayConfig = ReplayConfig {
    default_ttl_secs: 900,
    skew_grace_secs: 600,
};

struct TempRepo(PathBuf);

static COUNTER: AtomicU64 = AtomicU64::new(0);

impl TempRepo {
    fn new(bare: bool) -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "meshtree-coord-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        let repo = Self(path);
        repo.git(&[
            "init",
            "-q",
            if bare {
                "--bare"
            } else {
                "--initial-branch=main"
            },
        ]);
        repo
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

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempRepo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// `CliGitOps` whose *first* push waits at a barrier, so both clones compute
/// their commit against the same (absent) remote tip before either pushes:
/// exactly one first push can win the `--force-with-lease` CAS.
struct Gated {
    inner: CliGitOps,
    barrier: Arc<Barrier>,
    first_push: AtomicBool,
}

impl GitOps for Gated {
    fn worktree_add(&self, path: &Path, branch: &str, base: &str) -> Result<(), GitError> {
        self.inner.worktree_add(path, branch, base)
    }
    fn worktree_remove(&self, path: &Path) -> Result<(), GitError> {
        self.inner.worktree_remove(path)
    }
    fn worktree_list(&self) -> Result<Vec<WorktreeInfo>, GitError> {
        self.inner.worktree_list()
    }
    fn rev_parse(&self, rev: &str) -> Result<String, GitError> {
        self.inner.rev_parse(rev)
    }
    fn ref_read(&self, reference: &str) -> Result<Option<String>, GitError> {
        self.inner.ref_read(reference)
    }
    fn update_ref(&self, r: &str, new: &str, old: Option<&str>) -> Result<(), GitError> {
        self.inner.update_ref(r, new, old)
    }
    fn remote_url(&self, name: &str) -> Result<Option<String>, GitError> {
        self.inner.remote_url(name)
    }
    fn hash_object(&self, contents: &[u8]) -> Result<String, GitError> {
        self.inner.hash_object(contents)
    }
    fn mktree(&self, entries: &[TreeEntry]) -> Result<String, GitError> {
        self.inner.mktree(entries)
    }
    fn commit_tree(&self, tree: &str, parents: &[&str], msg: &str) -> Result<String, GitError> {
        self.inner.commit_tree(tree, parents, msg)
    }
    fn ls_tree(&self, treeish: &str) -> Result<Vec<TreeEntry>, GitError> {
        self.inner.ls_tree(treeish)
    }
    fn cat_blob(&self, oid: &str) -> Result<Vec<u8>, GitError> {
        self.inner.cat_blob(oid)
    }
    fn fetch_ref(&self, remote: &str, src: &str, dst: &str) -> Result<Option<String>, GitError> {
        self.inner.fetch_ref(remote, src, dst)
    }
    fn push_ref_cas(
        &self,
        remote: &str,
        reference: &str,
        new_oid: &str,
        expected_old: Option<&str>,
    ) -> Result<(), GitError> {
        if self.first_push.swap(false, Ordering::SeqCst) {
            self.barrier.wait();
        }
        self.inner
            .push_ref_cas(remote, reference, new_oid, expected_old)
    }
}

fn acquire(id: &str, task: &TaskId, machine: &str, secs: u64) -> LeaseEvent {
    LeaseEvent {
        id: id.to_string(),
        kind: EventType::Acquire,
        path: "src/auth/**".to_string(),
        task: task.clone(),
        machine: machine.to_string(),
        ts: Timestamp::from_unix_ms(secs * 1000),
        counter: 0,
        ttl_secs: Some(900),
    }
}

#[test]
fn two_clones_file_remote_cas_race_converges() {
    let remote = TempRepo::new(true);
    let (mac, win) = (TempRepo::new(false), TempRepo::new(false));
    for clone in [&mac, &win] {
        clone.git(&["remote", "add", "origin", &remote.file_url()]);
    }
    let mut alloc = TaskIdAllocator::new();
    let (task_mac, task_win) = (alloc.allocate(1, 1), alloc.allocate(1, 2));
    // The mac ACQUIRE is earlier, so it wins under the §9 ordering key no
    // matter which push lands first.
    let ev_mac = acquire("01JAY0000000000000000000MA", &task_mac, "mc-mac", 1_000);
    let ev_win = acquire("01JAY0000000000000000000WI", &task_win, "mc-win", 1_001);

    let barrier = Arc::new(Barrier::new(2));
    let gated = |repo: &TempRepo| Gated {
        inner: CliGitOps::new(repo.path()),
        barrier: Arc::clone(&barrier),
        first_push: AtomicBool::new(true),
    };
    let store_mac = CoordStore::new(gated(&mac), Some("origin".to_string()), CONFIG);
    let store_win = CoordStore::new(gated(&win), Some("origin".to_string()), CONFIG);

    let (race_mac, race_win) = std::thread::scope(|s| {
        let a = s.spawn(|| store_mac.sync(std::slice::from_ref(&ev_mac)));
        let b = s.spawn(|| store_win.sync(std::slice::from_ref(&ev_win)));
        (a.join().unwrap(), b.join().unwrap())
    });
    let race_mac = race_mac.expect("mac sync converges within the retry bound");
    let race_win = race_win.expect("win sync converges within the retry bound");
    for out in [&race_mac, &race_win] {
        assert!(out.attempts <= MAX_ATTEMPTS);
        assert!(out.pushed, "{out:?}");
        assert!(out.deferred.is_none() && out.issues.is_empty(), "{out:?}");
    }
    assert!(
        race_mac.attempts.max(race_win.attempts) >= 2,
        "both first pushes expected an absent ref; one must have lost the CAS"
    );

    // Both clones re-sync; their derived tables and refs are identical.
    let final_mac = store_mac.sync(&[]).expect("mac re-sync");
    let final_win = store_win.sync(&[]).expect("win re-sync");
    assert_eq!(final_mac.oid, final_win.oid);
    assert_eq!(
        serde_json::to_vec(&final_mac.replay).unwrap(),
        serde_json::to_vec(&final_win.replay).unwrap()
    );
    let lease = &final_mac.replay.leases["src/auth/**"];
    assert_eq!(lease.task, task_mac);

    // The loser's ACQUIRE is rejected (MT-001): the win daemon revokes its claim.
    assert_eq!(final_win.replay.rejected.len(), 1);
    let rejection = &final_win.replay.rejected[0];
    assert_eq!(rejection.code, ERROR_CODE_CLAIM_CONFLICT);
    assert_eq!(rejection.event, ev_win.id);
    assert_eq!(rejection.holder, task_mac);

    // The pushed tree holds both log entries and the derived snapshot, and
    // every ref (local ×2, remote) points at the same commit.
    let ops = CliGitOps::new(mac.path());
    let paths: Vec<String> = ops
        .ls_tree(&final_mac.oid)
        .unwrap()
        .into_iter()
        .map(|e| e.path)
        .collect();
    assert!(paths.contains(&format!("log/{}.json", ev_mac.id)));
    assert!(paths.contains(&format!("log/{}.json", ev_win.id)));
    assert!(paths.contains(&lease_file("src/auth/**")));
    for repo in [&mac, &win] {
        let local = CliGitOps::new(repo.path()).ref_read(COORD_REF).unwrap();
        assert_eq!(local.as_deref(), Some(final_mac.oid.as_str()));
    }
    let remote_tip = CliGitOps::new(remote.path()).ref_read(COORD_REF).unwrap();
    assert_eq!(remote_tip.as_deref(), Some(final_mac.oid.as_str()));
    // A further sync with nothing new is a no-op (no new commit).
    assert_eq!(store_win.sync(&[]).unwrap().oid, final_mac.oid);
}

#[test]
fn coord_sync_local_only_and_deferred_push() {
    let clone = TempRepo::new(false);
    let mut alloc = TaskIdAllocator::new();
    let task = alloc.allocate(1, 1);
    let ev = acquire("01JAY0000000000000000000L1", &task, "mc-local", 1_000);

    // No remote: local ref advances, nothing pushed, nothing deferred.
    assert_eq!(
        CliGitOps::new(clone.path()).remote_url("origin").unwrap(),
        None
    );
    let local_only = CoordStore::new(CliGitOps::new(clone.path()), None, CONFIG);
    let out = local_only.sync(std::slice::from_ref(&ev)).unwrap();
    assert!(!out.pushed && out.deferred.is_none());
    assert_eq!(out.attempts, 1);
    assert_eq!(out.replay.leases["src/auth/**"].task, task);

    // Unreachable remote: local progress is never blocked; the push is deferred.
    clone.git(&[
        "remote",
        "add",
        "origin",
        "file:///nonexistent/meshtree-remote",
    ]);
    assert_eq!(
        CliGitOps::new(clone.path())
            .remote_url("origin")
            .unwrap()
            .as_deref(),
        Some("file:///nonexistent/meshtree-remote")
    );
    let offline = CoordStore::new(
        CliGitOps::new(clone.path()),
        Some("origin".to_string()),
        CONFIG,
    );
    let ev2 = LeaseEvent {
        id: "01JAY0000000000000000000L2".to_string(),
        path: "docs/**".to_string(),
        ..ev.clone()
    };
    let out = offline.sync(std::slice::from_ref(&ev2)).unwrap();
    assert!(!out.pushed);
    assert!(out.deferred.is_some(), "{out:?}");
    assert_eq!(out.replay.leases.len(), 2);
    let local = CliGitOps::new(clone.path()).ref_read(COORD_REF).unwrap();
    assert_eq!(local.as_deref(), Some(out.oid.as_str()));
}
