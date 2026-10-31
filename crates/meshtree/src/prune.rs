//! `meshtree prune [--stale-only]`: reclaim worktrees nothing uses any more.
//!
//! Authoritative specification: `SPEC.md` §8 (`abort` / `prune`), §10 (Trap 2:
//! quarantine and GC sweep), §14, §16 (`MT-020`, `MT-021`).
//!
//! 1. **Stale worktrees:** a task in a terminal status (`merged`, `failed`,
//!    `abandoned` — e.g. swept by the daemon for inactivity) whose worktree
//!    still exists is torn down with the §10 pipeline.
//! 2. **Quarantine GC** (skipped with `--stale-only`): every tree in
//!    `.meshtree/quarantine/` is deleted if it no longer holds a lock.
//!
//! Active tasks are never touched. Locked leftovers are reported, not errors.

use crate::exit::CliError;
use crate::repo::{load_config, require_repo_root};
use crate::{registry, worktree};
use meshtree_platform::teardown::{gc_quarantine, Teardown, ERROR_CODE_FILE_LOCKED};
use std::path::Path;

/// Lines to print.
#[derive(Debug, Default)]
pub struct Report {
    pub stdout: Vec<String>,
    pub notes: Vec<String>,
}

/// Runs `meshtree prune`.
pub fn run(cwd: &Path, stale_only: bool) -> Result<Report, CliError> {
    let repo_root = require_repo_root(cwd)?;
    let config = load_config(&repo_root)?;
    let mut report = Report::default();

    for task in registry::list_tasks(&repo_root)?.tasks {
        let path = repo_root.join(&task.worktree);
        if !task.status.is_terminal() || !path.exists() {
            continue;
        }
        match worktree::remove(&repo_root, &config.share, &path) {
            Teardown::Removed { .. } => report
                .stdout
                .push(format!("removed worktree of {} ({})", task.id, task.status)),
            other => report.notes.push(format!("{}: {other}", task.id)),
        }
    }

    if !stale_only {
        let gc = gc_quarantine(&worktree::quarantine_dir(&repo_root));
        for path in gc.removed {
            report
                .stdout
                .push(format!("removed quarantined {}", path.display()));
        }
        for (path, error) in gc.locked {
            report.notes.push(format!(
                "[{ERROR_CODE_FILE_LOCKED}] quarantined {} is still locked ({error}); retry later",
                path.display()
            ));
        }
    }
    if report.stdout.is_empty() && report.notes.is_empty() {
        report.stdout.push("nothing to prune".to_string());
    }
    Ok(report)
}
