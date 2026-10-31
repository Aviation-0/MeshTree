//! Task worktree removal shared by `abort`, `collect`, crash recovery and
//! `meshtree prune`.
//!
//! Authoritative specification: `SPEC.md` §8 (`abort`, `collect` step 5,
//! `prune`), §10 (Trap 2 teardown pipeline), §11 (shared directories are
//! links into the primary worktree), §16 (`MT-020`, `MT-021`).
//!
//! Share links are removed first, without following them, so deleting the
//! worktree can never reach the primary worktree's `node_modules` through a
//! junction or symlink. The worktree is then torn down by
//! [`teardown::teardown`]: `git worktree remove --force` with backoff, then
//! quarantine, then deferral. The result is never an error: callers report
//! a non-`Removed` outcome as a note and carry on.

use crate::repo::MESHTREE_DIR;
use meshtree_core::ShareConfig;
use meshtree_platform::gitops::{CliGitOps, GitOps, MergeOps};
use meshtree_platform::share;
use meshtree_platform::teardown::{self, Teardown, BACKOFF};
use std::io;
use std::path::{Path, PathBuf};

/// `.meshtree/quarantine` under `repo_root`.
pub fn quarantine_dir(repo_root: &Path) -> PathBuf {
    repo_root.join(MESHTREE_DIR).join("quarantine")
}

/// Unlinks the configured shares in `worktree`, then tears it down.
pub fn remove(repo_root: &Path, shares: &[ShareConfig], worktree: &Path) -> Teardown {
    if let Err(err) = share::unlink_all(worktree, shares) {
        // Deleting with a live link inside could reach the shared target.
        return Teardown::Deferred {
            path: worktree.to_path_buf(),
            error: format!("could not unlink shared directories: {err}"),
            reboot_registered: 0,
        };
    }
    let git = CliGitOps::new(repo_root);
    let name = worktree
        .file_name()
        .map_or_else(|| "worktree".into(), |n| n.to_string_lossy().into_owned());
    let outcome = teardown::teardown(
        worktree,
        &quarantine_dir(repo_root),
        &name,
        &BACKOFF,
        || git.worktree_remove(worktree).map_err(io::Error::other),
    );
    if !matches!(outcome, Teardown::Removed { .. }) {
        // Drop git's record of a worktree that was moved away.
        let _ = git.worktree_prune();
    }
    outcome
}

/// A note for a teardown that did not simply remove the tree.
pub fn note(outcome: &Teardown) -> Option<String> {
    (!matches!(outcome, Teardown::Removed { .. })).then(|| outcome.to_string())
}
