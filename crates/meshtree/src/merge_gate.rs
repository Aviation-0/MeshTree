//! Serialized merge gate: `collect` (in the daemon) and `meshtree promote`.
//!
//! Authoritative specification: `SPEC.md` §2 (G4), §8 (`collect` steps 1–6
//! and the lifecycle edges), §14, §16 (`MT-030`, `MT-031`, `MT-032`).
//!
//! `collect` walks one task through the §8 diagram:
//!
//! 1. `active → collecting`; run `project.validate` in the task worktree
//!    while holding a slot of the `validate` shared-op queue (§12;
//!    `[queue.validate]` if configured, else concurrency 1). Failure:
//!    `collecting → active`, `MT-030`.
//! 2. Claim check: every path in `git diff --name-only <base>...<branch>`
//!    against every *other* task's lease in the coordination ref. Overlap:
//!    claims released, `collecting → failed`, `MT-032` naming the holder.
//! 3. Take the per-machine merge mutex; `collecting → merging`.
//! 4. Merge the task branch (`--no-ff`) in the integration worktree
//!    (`.meshtree/integration`, detached at the integration tip; the branch
//!    is created from `project.base_branch` on first use). Conflict: merge
//!    aborted, `merging → active`, `MT-031` listing the paths.
//! 5. Run `project.promote_validate` on the merge commit (same queue). Only
//!    then does `refs/heads/<integration>` move (local CAS), so the
//!    integration branch only ever holds validated merges. Failure:
//!    `merging → active`, `MT-030`. Success: claims released, task worktree
//!    removed (a removal failure is reported but never fails the merge,
//!    §10), `merging → merged`.
//!
//! `meshtree promote` fast-forwards `project.base_branch` to the integration
//! tip (step 6, the human gate). Every integration commit already passed
//! `promote_validate`, so promote only refuses non-fast-forwards. Pushing the
//! integration branch to the remote (non-fast-forward retry) is `T-039`.

use crate::claims;
use crate::exit::{CliError, PLATFORM, TASK, USAGE};
use crate::queue::shell;
use crate::registry;
use crate::repo::{load_config, require_repo_root, MESHTREE_DIR};
use meshtree_core::pattern::conflicts_raw;
use meshtree_core::{Config, Task, TaskId, TaskStatus};
use meshtree_platform::gitops::{CliGitOps, GitError, GitOps, MergeOps, MergeOutcome};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::MutexGuard;

/// `MT-030`: validation failed (`SPEC.md` §16).
pub const ERROR_CODE_VALIDATE_FAILED: &str = "MT-030";
/// `MT-031`: merge conflict with integration.
pub const ERROR_CODE_MERGE_CONFLICT: &str = "MT-031";
/// `MT-032`: claim overlap at the merge gate.
pub const ERROR_CODE_CLAIM_OVERLAP: &str = "MT-032";

/// Shared-op queue that validation runs in.
pub const VALIDATE_QUEUE: &str = "validate";

/// Integration worktree, relative to the repository root.
pub const INTEGRATION_WORKTREE: &str = "integration";

/// Lines of command output kept in an `MT-030` message.
const OUTPUT_TAIL_LINES: usize = 20;

/// "Ref must not exist" for a local `update-ref` CAS.
const ZERO_OID: &str = "0000000000000000000000000000000000000000";

/// What the gate needs from the daemon that runs it.
pub trait Host {
    fn repo_root(&self) -> &Path;
    fn config(&self) -> &Config;
    /// Applies a §8 edge: `task.json` written and the transition journaled.
    fn transition(&self, task: &mut Task, to: TaskStatus) -> Result<(), CliError>;
    /// Runs `f` while holding a slot of shared-op queue `queue`.
    fn with_slot<T>(
        &self,
        queue: &str,
        task: &TaskId,
        f: impl FnOnce() -> T,
    ) -> Result<T, CliError>;
    /// Releases every claim of `task` (coordination ref + WAL) and records it.
    fn release_claims(&self, task: &mut Task) -> Result<(), CliError>;
    /// The per-machine merge mutex (§8 step 3).
    fn merge_mutex(&self) -> MutexGuard<'_, ()>;
}

fn git_error(err: GitError) -> CliError {
    CliError::new(PLATFORM, err.to_string())
}

/// Runs `collect` for `raw_id`; returns the merge result.
pub fn collect<H: Host>(host: &H, raw_id: &str, message: Option<&str>) -> Result<Value, CliError> {
    let id = TaskId::parse(raw_id)
        .ok_or_else(|| CliError::new(USAGE, format!("'{raw_id}' is not a task id")))?;
    let mut task = registry::read_task(host.repo_root(), &id)?;
    host.transition(&mut task, TaskStatus::Collecting)?;
    let result = gate(host, &mut task, message);
    if let Err(failure) = &result {
        if matches!(task.status, TaskStatus::Collecting | TaskStatus::Merging) {
            // An unexpected failure (git, filesystem): hand the task back to
            // the agent rather than leave it mid-collect.
            if let Err(err) = host.transition(&mut task, TaskStatus::Active) {
                return Err(CliError::new(
                    err.code,
                    format!(
                        "{}; also failed to reactivate {id}: {}",
                        failure.message, err.message
                    ),
                ));
            }
        }
    }
    result
}

/// Steps 1–5 for a task already in `collecting`. Domain failures apply
/// their own §8 edge; any other error leaves the status for [`collect`].
fn gate<H: Host>(host: &H, task: &mut Task, message: Option<&str>) -> Result<Value, CliError> {
    let id = task.id.clone();
    let repo_root = host.repo_root().to_path_buf();
    let config = host.config();

    // 1. Validate inside the task worktree, through the shared-op queue.
    let worktree = repo_root.join(&task.worktree);
    if let Err(err) = run_validation(host, task, &config.project.validate, &worktree, "task") {
        host.transition(task, TaskStatus::Active)?;
        return Err(err);
    }

    // 2. Claim check against every other task's leases.
    let git = CliGitOps::new(&repo_root);
    let changed = git
        .diff_names(&task.base, &task.branch)
        .map_err(git_error)?;
    let leases = claims::store(&repo_root, config)?
        .sync(&[])
        .map_err(|err| CliError::new(TASK, err.to_string()))?
        .replay
        .leases;
    let overlaps: Vec<String> = leases
        .iter()
        .filter(|(_, lease)| lease.task != task.id)
        .flat_map(|(pattern, lease)| {
            changed
                .iter()
                .filter(|path| conflicts_raw(pattern, path))
                .map(move |path| format!("{path} is claimed by {} ({pattern})", lease.task))
        })
        .collect();
    if !overlaps.is_empty() {
        host.release_claims(task)?;
        host.transition(task, TaskStatus::Failed)?;
        return Err(CliError::new(
            TASK,
            format!(
                "[{ERROR_CODE_CLAIM_OVERLAP}] {id} changes paths claimed by other tasks: {}",
                overlaps.join("; ")
            ),
        ));
    }

    // 3. One merge at a time on this machine.
    let _merge = host.merge_mutex();
    host.transition(task, TaskStatus::Merging)?;

    // 4. Merge into the integration branch in its own worktree. The branch
    // starts at the base branch and is only created by a validated merge.
    let integration_ref = format!("refs/heads/{}", config.project.integration_branch);
    let existing = git.ref_read(&integration_ref).map_err(git_error)?;
    let tip = match &existing {
        Some(tip) => tip.clone(),
        None => git
            .rev_parse(&config.project.base_branch)
            .map_err(git_error)?,
    };
    let integration = integration_worktree(&repo_root);
    let wt = CliGitOps::new(&integration);
    if integration.exists() {
        wt.reset_clean(&tip).map_err(git_error)?;
    } else {
        git.worktree_add_detached(&integration, &tip)
            .map_err(git_error)?;
    }
    let message = message
        .map(str::to_string)
        .unwrap_or_else(|| format!("meshtree: merge {id} ({})", task.name));
    let merged = match wt.merge_no_ff(&task.branch, &message).map_err(git_error)? {
        MergeOutcome::Merged(oid) => oid,
        MergeOutcome::Conflict(paths) => {
            wt.reset_clean(&tip).map_err(git_error)?;
            host.transition(task, TaskStatus::Active)?;
            return Err(CliError::new(
                TASK,
                format!(
                    "[{ERROR_CODE_MERGE_CONFLICT}] {id} conflicts with {}: {}; \
                     merge {} into the task branch and collect again",
                    config.project.integration_branch,
                    paths.join(", "),
                    config.project.integration_branch
                ),
            ));
        }
    };

    // 5. Validate the merge before the integration branch moves.
    if let Err(err) = run_validation(
        host,
        task,
        &config.project.promote_validate,
        &integration,
        "integration",
    ) {
        wt.reset_clean(&tip).map_err(git_error)?;
        host.transition(task, TaskStatus::Active)?;
        return Err(err);
    }
    git.update_ref(
        &integration_ref,
        &merged,
        Some(existing.as_deref().unwrap_or(ZERO_OID)),
    )
    .map_err(git_error)?;
    host.release_claims(task)?;
    let cleanup = if worktree.exists() {
        crate::worktree::remove(&repo_root, &config.share, &worktree)
            .err()
            .map(|err| format!("task worktree left in place: {err}"))
    } else {
        None
    };
    host.transition(task, TaskStatus::Merged)?;
    Ok(json!({
        "task_id": id,
        "status": task.status,
        "integration_branch": config.project.integration_branch,
        "integration": merged,
        "cleanup": cleanup,
    }))
}

/// `.meshtree/integration` under `repo_root`.
pub fn integration_worktree(repo_root: &Path) -> PathBuf {
    repo_root.join(MESHTREE_DIR).join(INTEGRATION_WORKTREE)
}

/// Runs `commands` in `dir` under a `validate` queue slot; the first failure
/// is `MT-030` with the tail of its output.
fn run_validation<H: Host>(
    host: &H,
    task: &Task,
    commands: &[String],
    dir: &Path,
    place: &str,
) -> Result<(), CliError> {
    if commands.is_empty() {
        return Ok(());
    }
    host.with_slot(VALIDATE_QUEUE, &task.id, || {
        for command in commands {
            let output = shell(command)
                .current_dir(dir)
                .output()
                .map_err(|err| CliError::new(PLATFORM, format!("run `{command}`: {err}")))?;
            if !output.status.success() {
                let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
                text.push_str(&String::from_utf8_lossy(&output.stderr));
                let lines: Vec<&str> = text.lines().collect();
                let tail = lines[lines.len().saturating_sub(OUTPUT_TAIL_LINES)..].join("\n");
                return Err(CliError::new(
                    TASK,
                    format!(
                        "[{ERROR_CODE_VALIDATE_FAILED}] validation failed on {place} for {}: \
                         `{command}` exited with {}\n{tail}",
                        task.id, output.status
                    ),
                ));
            }
        }
        Ok(())
    })?
}

/// `meshtree promote`: fast-forwards the base branch to the integration tip.
pub fn promote(cwd: &Path) -> Result<String, CliError> {
    let repo_root = require_repo_root(cwd)?;
    let config = load_config(&repo_root)?;
    let (base, integration) = (
        &config.project.base_branch,
        &config.project.integration_branch,
    );
    let git = CliGitOps::new(&repo_root);
    let base_ref = format!("refs/heads/{base}");
    let new = git
        .ref_read(&format!("refs/heads/{integration}"))
        .map_err(git_error)?
        .ok_or_else(|| {
            CliError::new(
                TASK,
                format!("nothing to promote: {integration} does not exist yet"),
            )
        })?;
    let old = git
        .ref_read(&base_ref)
        .map_err(git_error)?
        .ok_or_else(|| CliError::new(TASK, format!("base branch {base} does not exist")))?;
    if old == new {
        return Ok(format!("{base} is up to date with {integration}"));
    }
    if !git.is_ancestor(&old, &new).map_err(git_error)? {
        return Err(CliError::new(
            TASK,
            format!("{base} is not an ancestor of {integration}; promote only fast-forwards"),
        ));
    }
    if git.head_branch().map_err(git_error)?.as_deref() == Some(base_ref.as_str()) {
        // Checked out here: move the working tree with the branch.
        git.merge_ff_only(&new).map_err(git_error)?;
    } else {
        git.update_ref(&base_ref, &new, Some(&old))
            .map_err(git_error)?;
    }
    Ok(format!(
        "promoted {base} {}..{} from {integration}",
        &old[..12],
        &new[..12]
    ))
}
