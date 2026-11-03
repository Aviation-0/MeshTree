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
//! `promote_validate`, so promote only refuses non-fast-forwards.
//!
//! With an `origin` remote, steps 4–5 form one round that ends with a
//! `--force-with-lease` push of the validated merge to `origin/<integration>`
//! before the local ref moves (`T-039`). Each round fetches the remote tip
//! and starts from it (fast-forward), or, when the local branch holds merges
//! the remote lacks (an earlier push was deferred), replay-merges the remote
//! tip into it. A lost push race reruns the round on the new remote tip, at
//! most [`MAX_PUSH_ATTEMPTS`] times, then `MT-031` (`merging → active`). A
//! fetch or push failure that is not a race (network, auth) does not fail
//! the merge: the local ref moves, the result reports the push as deferred,
//! and the next collect publishes it.

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

/// Bound on fetch, merge, validate, push rounds (`SPEC.md` §8 step 4,
/// "bounded ×5").
pub const MAX_PUSH_ATTEMPTS: u32 = 5;

/// Local ref holding the last fetched remote integration tip.
const REMOTE_INTEGRATION_REF: &str = "refs/meshtree/remote-integration";

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

    // 4–5. Merge into the integration branch in its own worktree, validate,
    // then publish: remote CAS push first (when `origin` exists), local ref
    // last. A lost push race reruns the round on the new remote tip.
    let integration_branch = &config.project.integration_branch;
    let integration_ref = format!("refs/heads/{integration_branch}");
    let remote = git
        .remote_url(claims::REMOTE)
        .map_err(git_error)?
        .map(|_| claims::REMOTE);
    let integration = integration_worktree(&repo_root);
    let wt = CliGitOps::new(&integration);
    let message = message
        .map(str::to_string)
        .unwrap_or_else(|| format!("meshtree: merge {id} ({})", task.name));
    let mut attempt = 0;
    let (merged, pushed, deferred) = loop {
        attempt += 1;
        let local = git.ref_read(&integration_ref).map_err(git_error)?;
        let (remote_tip, mut deferred) = match remote {
            None => (None, None),
            Some(remote) => match git.fetch_ref(remote, &integration_ref, REMOTE_INTEGRATION_REF) {
                Ok(tip) => (tip, None),
                Err(err) => (None, Some(format!("fetch failed: {err}"))),
            },
        };
        let remote_known = remote.is_some() && deferred.is_none();

        // The branch starts at the base branch and is only created by a
        // validated merge.
        let start = match local.clone().or_else(|| remote_tip.clone()) {
            Some(start) => start,
            None => git
                .rev_parse(&config.project.base_branch)
                .map_err(git_error)?,
        };
        if integration.exists() {
            wt.reset_clean(&start).map_err(git_error)?;
        } else {
            git.worktree_add_detached(&integration, &start)
                .map_err(git_error)?;
        }
        if let (Some(ours), Some(theirs)) = (&local, &remote_tip) {
            if ours != theirs && !git.is_ancestor(theirs, ours).map_err(git_error)? {
                if git.is_ancestor(ours, theirs).map_err(git_error)? {
                    wt.reset_clean(theirs).map_err(git_error)?;
                } else {
                    // Local merges the remote lacks (an earlier push was
                    // deferred): replay-merge the remote tip first.
                    let sync = format!("meshtree: sync {integration_branch} with origin");
                    if let MergeOutcome::Conflict(paths) =
                        wt.merge_no_ff(theirs, &sync).map_err(git_error)?
                    {
                        wt.reset_clean(&start).map_err(git_error)?;
                        host.transition(task, TaskStatus::Active)?;
                        return Err(CliError::new(
                            TASK,
                            format!(
                                "[{ERROR_CODE_MERGE_CONFLICT}] local {integration_branch} \
                                 diverged from origin with conflicts in {}; reconcile it \
                                 and collect {id} again",
                                paths.join(", ")
                            ),
                        ));
                    }
                }
            }
        }
        let tip = wt.rev_parse("HEAD").map_err(git_error)?;

        let merged = match wt.merge_no_ff(&task.branch, &message).map_err(git_error)? {
            MergeOutcome::Merged(oid) => oid,
            MergeOutcome::Conflict(paths) => {
                wt.reset_clean(&tip).map_err(git_error)?;
                host.transition(task, TaskStatus::Active)?;
                return Err(CliError::new(
                    TASK,
                    format!(
                        "[{ERROR_CODE_MERGE_CONFLICT}] {id} conflicts with {integration_branch}: \
                         {}; merge {integration_branch} into the task branch and collect again",
                        paths.join(", "),
                    ),
                ));
            }
        };

        // Validate the merge before the integration branch moves.
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

        let mut pushed = false;
        if let (Some(remote), true) = (remote, remote_known) {
            match git.push_ref_cas(remote, &integration_ref, &merged, remote_tip.as_deref()) {
                Ok(()) => pushed = true,
                Err(GitError::RemoteCasRejected { .. }) if attempt < MAX_PUSH_ATTEMPTS => {
                    backoff(attempt);
                    continue;
                }
                Err(GitError::RemoteCasRejected { .. }) => {
                    wt.reset_clean(&tip).map_err(git_error)?;
                    host.transition(task, TaskStatus::Active)?;
                    return Err(CliError::new(
                        TASK,
                        format!(
                            "[{ERROR_CODE_MERGE_CONFLICT}] {remote}/{integration_branch} kept \
                             moving: the push of {id}'s merge lost {attempt} races; collect again"
                        ),
                    ));
                }
                Err(err) => deferred = Some(format!("push failed: {err}")),
            }
        }
        git.update_ref(
            &integration_ref,
            &merged,
            Some(local.as_deref().unwrap_or(ZERO_OID)),
        )
        .map_err(git_error)?;
        break (merged, pushed, deferred);
    };
    host.release_claims(task)?;
    let cleanup = if worktree.exists() {
        crate::worktree::note(&crate::worktree::remove(
            &repo_root,
            &config.share,
            &worktree,
        ))
    } else {
        None
    };
    host.transition(task, TaskStatus::Merged)?;
    Ok(json!({
        "task_id": id,
        "status": task.status,
        "integration_branch": config.project.integration_branch,
        "integration": merged,
        "attempts": attempt,
        "pushed": pushed,
        "push_deferred": deferred,
        "cleanup": cleanup,
    }))
}

/// Jittered pause before retrying a lost push race.
fn backoff(attempt: u32) {
    let jitter = u64::try_from(crate::clock::entropy() % 50).unwrap_or(0);
    std::thread::sleep(std::time::Duration::from_millis(
        (10 + jitter) * u64::from(attempt),
    ));
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
