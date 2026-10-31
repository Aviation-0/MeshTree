//! Task worktree removal shared by `abort`, `collect` and crash recovery.
//!
//! Authoritative specification: `SPEC.md` §8 (`abort`, `collect` step 5),
//! §11 (shared directories are links into the primary worktree). Share links
//! are removed first, without following them, so deleting the worktree can
//! never reach the primary worktree's `node_modules` through a junction or
//! symlink. Retry-with-backoff and quarantine are added by `T-024`.

use meshtree_core::ShareConfig;
use meshtree_platform::gitops::{CliGitOps, GitOps};
use meshtree_platform::share;
use std::path::Path;

/// Unlinks the configured shares in `worktree`, then removes the worktree.
pub fn remove(repo_root: &Path, shares: &[ShareConfig], worktree: &Path) -> Result<(), String> {
    share::unlink_all(worktree, shares)
        .map_err(|err| format!("unlink shared directories: {err}"))?;
    CliGitOps::new(repo_root)
        .worktree_remove(worktree)
        .map_err(|err| err.to_string())
}
