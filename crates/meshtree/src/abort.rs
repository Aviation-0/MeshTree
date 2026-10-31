//! `meshtree abort <task-id>`: discard a task (`SPEC.md` §8 `abort`, §14, §16).
//!
//! Releases the task's claims, tears down its worktree (§10: backoff, then
//! quarantine `MT-021`, then deferral `MT-020` — never a failure), then
//! records `active → abandoned`. The task branch is kept so discarded work
//! stays recoverable.

use crate::exit::{CliError, TASK, USAGE};
use crate::repo::{load_config, require_repo_root};
use crate::{claims, registry};
use meshtree_core::{Task, TaskError, TaskId, TaskStatus};
use std::path::Path;

/// Runs `meshtree abort`; returns the updated task and a teardown note
/// when the worktree was quarantined or left locked.
pub fn run(cwd: &Path, task_id: &str) -> Result<(Task, Option<String>), CliError> {
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

    // Release first: if the coordination ref cannot be written, nothing
    // else has changed and the abort can simply be retried.
    if !task.claims.is_empty() {
        let config = load_config(&repo_root)?;
        claims::release(&repo_root, &config, &task.id, &task.claims)?;
        task.claims.clear();
    }

    let worktree = repo_root.join(&task.worktree);
    let note = if worktree.exists() {
        let config = load_config(&repo_root)?;
        crate::worktree::note(&crate::worktree::remove(
            &repo_root,
            &config.share,
            &worktree,
        ))
    } else {
        None
    };

    task.transition(TaskStatus::Abandoned)
        .map_err(|err| CliError::new(TASK, err.to_string()))?;
    registry::write_task(&repo_root, &task)?;
    Ok((task, note))
}
