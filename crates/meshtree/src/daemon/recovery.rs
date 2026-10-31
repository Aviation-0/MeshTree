//! Crash recovery on daemon startup.
//!
//! Authoritative specification: `SPEC.md` §10 (Crash safety: replay the WAL,
//! adopt orphaned tasks, re-enter idle; recovery < 2 s, zero orphaned
//! `index.lock`), §8 (lifecycle edges), §18 (chaos: `SIGKILL` mid-merge).
//!
//! Runs after the WAL replay and before the IPC endpoint is bound, so no
//! client sees a half-recovered daemon:
//!
//! 1. **Reconcile.** `task.json` is written before its WAL entry, so a crash
//!    can leave the WAL one edge behind. Tasks the WAL never saw are adopted
//!    along their §8 path; a WAL status one legal edge behind is advanced.
//! 2. **Stale git locks.** `index.lock` of the integration worktree and of
//!    every interrupted task worktree, and the lock files of the refs the
//!    daemon writes (`refs/heads/<integration>`, `refs/meshtree/coord`). A
//!    git child of the killed daemon may still be finishing, so a lock is
//!    removed only once it is [`LOCK_GRACE`] old. The primary worktree's
//!    index is never touched: only the user's own git uses it.
//! 3. **Integration worktree** reset to the integration tip: an interrupted
//!    merge never reached the branch (the ref moves only after
//!    `promote_validate`).
//! 4. **Interrupted collects.** A `merging` task whose branch is already in
//!    the integration branch crashed after the ref moved: its claims are
//!    released, its worktree removed, and it becomes `merged`. Every other
//!    `collecting` / `merging` task returns to `active` so its agent can
//!    collect again.
//!
//! Leases are task-scoped (§7 `Lease` carries no pid); the process-scoped
//! holders of §12 are queue tickets, which live in daemon memory and so do
//! not survive the crash that would orphan them.

use super::Daemon;
use crate::exit::CliError;
use crate::merge_gate::{self, Host};
use crate::registry;
use meshtree_core::{Task, TaskId, TaskStatus};
use meshtree_platform::gitops::{CliGitOps, GitOps, MergeOps};
use meshtree_platform::wal::{WalOp, WalState};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Minimum age of a lock file before recovery treats it as orphaned.
pub const LOCK_GRACE: Duration = Duration::from_secs(1);

/// What [`Daemon::recover`] did.
#[derive(Debug, Default)]
pub struct Report {
    /// Tasks adopted into the WAL.
    pub adopted: usize,
    /// WAL entries appended to catch up with `task.json`.
    pub reconciled: usize,
    /// Interrupted collects returned to `active`.
    pub reactivated: Vec<TaskId>,
    /// Interrupted collects whose merge had landed, now `merged`.
    pub completed: Vec<TaskId>,
    /// Lock files removed.
    pub locks: Vec<PathBuf>,
}

impl Report {
    /// One-line summary, `None` when there was nothing to do.
    pub fn summary(&self) -> Option<String> {
        if self.adopted + self.reconciled == 0
            && self.reactivated.is_empty()
            && self.completed.is_empty()
            && self.locks.is_empty()
        {
            return None;
        }
        let ids = |v: &[TaskId]| {
            v.iter()
                .map(|t| t.to_string())
                .collect::<Vec<_>>()
                .join(" ")
        };
        Some(format!(
            "recovery: adopted {}, reconciled {}, reactivated [{}], completed [{}], cleared {} lock(s)",
            self.adopted,
            self.reconciled,
            ids(&self.reactivated),
            ids(&self.completed),
            self.locks.len()
        ))
    }
}

/// `true` for statuses a crash can interrupt mid-`collect`.
fn interrupted(status: TaskStatus) -> bool {
    matches!(status, TaskStatus::Collecting | TaskStatus::Merging)
}

impl Daemon {
    /// Runs startup recovery against the replayed `wal` state.
    pub(super) fn recover(&self, wal: &WalState) -> Result<Report, CliError> {
        let mut report = Report::default();
        let tasks = registry::list_tasks(&self.repo_root)?.tasks;

        // 1. Reconcile the WAL with task.json.
        {
            let mut inner = self.lock();
            for task in &tasks {
                match wal.tasks.get(&task.id) {
                    None if !task.status.is_terminal() => {
                        inner.ensure_journaled(task)?;
                        report.adopted += 1;
                    }
                    Some(seen)
                        if seen.status != task.status
                            && seen.status.can_transition_to(task.status) =>
                    {
                        inner.journal(WalOp::Transition {
                            task: task.id.clone(),
                            from: seen.status,
                            to: task.status,
                        })?;
                        report.reconciled += 1;
                    }
                    Some(seen) if seen.status != task.status => note!(
                        "WAL has {} as {}, task.json as {}; keeping task.json",
                        task.id,
                        seen.status,
                        task.status
                    ),
                    _ => {}
                }
            }
        }

        let stuck: Vec<Task> = tasks
            .into_iter()
            .filter(|t| interrupted(t.status))
            .collect();
        let git = CliGitOps::new(&self.repo_root);
        let integration = merge_gate::integration_worktree(&self.repo_root);
        let integration_ref = format!("refs/heads/{}", self.config.project.integration_branch);

        // 2. Orphaned git locks in the places the daemon's git children write.
        let mut locks = Vec::new();
        if let Some(common) = common_dir(&self.repo_root) {
            locks.push(common.join(format!("{integration_ref}.lock")));
            locks.push(common.join("refs/meshtree/coord.lock"));
        }
        let worktrees = std::iter::once(integration.clone())
            .chain(stuck.iter().map(|t| self.repo_root.join(&t.worktree)));
        for dir in worktrees.filter(|d| d.exists()) {
            if let Ok(gitdir) = CliGitOps::new(&dir).rev_parse("--absolute-git-dir") {
                locks.push(PathBuf::from(gitdir).join("index.lock"));
            }
        }
        report.locks = clear_stale_locks(&locks, LOCK_GRACE);

        // 3. Discard any half-done merge in the integration worktree.
        let tip = git.ref_read(&integration_ref).map_err(git_error)?;
        if integration.exists() {
            let reset_to = match &tip {
                Some(tip) => tip.clone(),
                None => git
                    .rev_parse(&self.config.project.base_branch)
                    .map_err(git_error)?,
            };
            CliGitOps::new(&integration)
                .reset_clean(&reset_to)
                .map_err(git_error)?;
        }

        // 4. Settle interrupted collects.
        for mut task in stuck {
            let landed = task.status == TaskStatus::Merging
                && match &tip {
                    Some(tip) => git.is_ancestor(&task.branch, tip).map_err(git_error)?,
                    None => false,
                };
            if landed {
                self.release_claims(&mut task)?;
                let worktree = self.repo_root.join(&task.worktree);
                if worktree.exists() {
                    if let Err(err) = git.worktree_remove(&worktree) {
                        note!("{}: task worktree left in place: {err}", task.id);
                    }
                }
                self.transition(&mut task, TaskStatus::Merged)?;
                report.completed.push(task.id);
            } else {
                self.transition(&mut task, TaskStatus::Active)?;
                report.reactivated.push(task.id);
            }
        }
        Ok(report)
    }
}

fn git_error(err: meshtree_platform::gitops::GitError) -> CliError {
    CliError::new(crate::exit::PLATFORM, err.to_string())
}

/// The repository's common git directory (absolute).
fn common_dir(repo_root: &Path) -> Option<PathBuf> {
    let dir = PathBuf::from(
        CliGitOps::new(repo_root)
            .rev_parse("--git-common-dir")
            .ok()?,
    );
    Some(if dir.is_absolute() {
        dir
    } else {
        repo_root.join(dir)
    })
}

/// Removes the existing `paths` once the youngest is `grace` old (waiting at
/// most `grace` once). Returns the paths removed.
pub fn clear_stale_locks(paths: &[PathBuf], grace: Duration) -> Vec<PathBuf> {
    let youngest = paths
        .iter()
        .filter_map(|p| fs::metadata(p).ok())
        .map(|m| {
            m.modified()
                .ok()
                .and_then(|t| t.elapsed().ok())
                .unwrap_or(Duration::ZERO)
        })
        .min();
    if let Some(age) = youngest {
        if age < grace {
            std::thread::sleep(grace - age);
        }
    }
    paths
        .iter()
        .filter(|p| fs::remove_file(p).is_ok())
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mt-locks-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn stale_locks_wait_for_grace_then_go() {
        let dir = temp("grace");
        let (a, b, missing) = (dir.join("a.lock"), dir.join("b.lock"), dir.join("c.lock"));
        fs::write(&a, "").unwrap();
        fs::write(&b, "").unwrap();
        let start = Instant::now();
        let removed =
            clear_stale_locks(&[a.clone(), b.clone(), missing], Duration::from_millis(300));
        assert!(
            start.elapsed() >= Duration::from_millis(250),
            "fresh locks get the grace"
        );
        assert_eq!(removed, [a.clone(), b.clone()]);
        assert!(!a.exists() && !b.exists());

        // Nothing present: no wait.
        let start = Instant::now();
        assert!(clear_stale_locks(&[a], Duration::from_secs(5)).is_empty());
        assert!(start.elapsed() < Duration::from_secs(1));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn recovery_summary_is_silent_when_idle() {
        assert_eq!(Report::default().summary(), None);
        let report = Report {
            reactivated: vec![TaskId::from_ulid(1)],
            locks: vec![PathBuf::from("x.lock")],
            ..Report::default()
        };
        let line = report.summary().unwrap();
        assert!(line.contains("reactivated [t-"), "{line}");
        assert!(line.contains("cleared 1 lock(s)"), "{line}");
    }
}
