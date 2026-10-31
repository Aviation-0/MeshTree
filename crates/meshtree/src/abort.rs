//! `meshtree abort <task-id>`: discard a task (`SPEC.md` §8 `abort`, §14, §16).
//!
//! Removes the task worktree, then records `active → abandoned`. The task
//! branch is kept so discarded work stays recoverable. Claim release lands
//! with claims (`T-014`); retry-with-backoff and quarantine on locked files
//! land in `T-024`, so a removal failure here leaves the task `active` for
//! a retry.

use crate::exit::{CliError, PLATFORM, TASK, USAGE};
use crate::registry;
use crate::repo::require_repo_root;
use meshtree_core::{Task, TaskError, TaskId, TaskStatus};
use meshtree_platform::gitops::{CliGitOps, GitOps};
use std::path::Path;

/// Runs `meshtree abort`; returns the updated task.
pub fn run(cwd: &Path, task_id: &str) -> Result<Task, CliError> {
    let repo_root = require_repo_root(cwd)?;
    let id = TaskId::parse(task_id)
        .ok_or_else(|| CliError::new(USAGE, format!("'{task_id}' is not a task id")))?;
    let mut task = registry::read_task(&repo_root, &id)?;

    // Check the §8 edge before touching the worktree, so aborting a finished
    // task is a pure MT-004 error with no side effects.
    if !task.status.can_transition_to(TaskStatus::Abandoned) {
        let err = TaskError::new(format!(
            "{id}: {} -> {} (only active tasks can be aborted)",
            task.status,
            TaskStatus::Abandoned
        ));
        return Err(CliError::new(TASK, err.to_string()));
    }

    let worktree = repo_root.join(&task.worktree);
    if worktree.exists() {
        CliGitOps::new(&repo_root)
            .worktree_remove(&worktree)
            .map_err(|err| {
                CliError::new(
                    PLATFORM,
                    format!("{id}: worktree removal failed, task left active: {err}"),
                )
            })?;
    }

    task.transition(TaskStatus::Abandoned)
        .map_err(|err| CliError::new(TASK, err.to_string()))?;
    registry::write_task(&repo_root, &task)?;
    Ok(task)
}
