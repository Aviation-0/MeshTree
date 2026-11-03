//! `meshtree claim` / `release` / `sync` and `spawn --claim`.
//!
//! Authoritative specification: `SPEC.md` §8, §9 (lease protocol, enforcement
//! model), §14 (CLI, exit codes), §16 (`MT-001`, `MT-002`, `MT-003`).
//!
//! Claims are written as lease events to `refs/meshtree/coord` through
//! [`CoordStore`], which replays the union of the local and remote logs. On
//! one machine that replay is the synchronous arbiter; across machines it is
//! advisory and `sync` reports claims a remote winner revoked. A claim batch
//! is all-or-nothing: if any path is rejected, the granted ones are released.
//! The remote is `origin` when configured; otherwise the ref is local-only.

use crate::clock;
use crate::daemon;
use crate::exit::{CliError, DAEMON, PLATFORM, TASK, USAGE};
use crate::registry;
use crate::repo::{load_config, machine_id, require_repo_root};
use meshtree_core::lease::{EventType, LeaseEvent, ReplayConfig, ERROR_CODE_CLAIM_CONFLICT};
use meshtree_core::pattern::normalize;
use meshtree_core::{Config, Task, TaskId, TaskIdAllocator, TaskStatus, Timestamp};
use meshtree_platform::coord::{CoordError, CoordStore, SyncOutcome};
use meshtree_platform::gitops::{CliGitOps, GitOps};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Remote that carries the coordination ref.
pub const REMOTE: &str = "origin";

/// Normalizes claim globs (§7), dropping duplicates; invalid ones are usage errors.
pub fn normalize_all(globs: &[String]) -> Result<Vec<String>, CliError> {
    let mut out: Vec<String> = Vec::new();
    for glob in globs {
        let path = normalize(glob).map_err(|err| CliError::new(USAGE, err.to_string()))?;
        if !out.contains(&path) {
            out.push(path);
        }
    }
    Ok(out)
}

/// The repository's coordination store (remote = `origin` if configured).
pub fn store(repo_root: &Path, config: &Config) -> Result<CoordStore<CliGitOps>, CliError> {
    let git = CliGitOps::new(repo_root);
    let remote = git
        .remote_url(REMOTE)
        .map_err(|err| CliError::new(PLATFORM, err.to_string()))?
        .map(|_| REMOTE.to_string());
    Ok(CoordStore::new(git, remote, replay_config(config)))
}

/// Lease replay parameters from `[lease]`.
pub fn replay_config(config: &Config) -> ReplayConfig {
    ReplayConfig {
        default_ttl_secs: config.lease.ttl_secs,
        skew_grace_secs: config.lease.skew_grace_secs,
    }
}

/// The `MT-001` error for rejected claims.
pub(crate) fn conflict_error(rejected: &[&meshtree_core::Rejection]) -> CliError {
    let detail: Vec<String> = rejected
        .iter()
        .map(|r| format!("{} held by {} ({})", r.path, r.holder, r.holder_path))
        .collect();
    CliError::new(
        TASK,
        format!(
            "[{ERROR_CODE_CLAIM_CONFLICT}] claim conflict: {}",
            detail.join("; ")
        ),
    )
}

pub(crate) fn coord_error(err: CoordError) -> CliError {
    match err.code() {
        Some(_) => CliError::new(TASK, err.to_string()),
        None => CliError::new(PLATFORM, err.to_string()),
    }
}

/// One event per path with fresh ULID ids. `counter` is the wall clock in
/// nanoseconds plus the batch index, so a rollback `RELEASE` always orders
/// after the `ACQUIRE` it undoes even within the same millisecond (§9 key).
pub(crate) fn events(
    kind: EventType,
    task: &TaskId,
    paths: &[String],
    machine: &str,
    ttl: u64,
) -> Vec<LeaseEvent> {
    let now = clock::now_unix_ms();
    let base_counter = clock::now_unix_nanos();
    let mut alloc = TaskIdAllocator::new();
    paths
        .iter()
        .enumerate()
        .map(|(i, path)| LeaseEvent {
            // A task id is "t-" + ULID; event ids are the bare ULID.
            id: alloc.allocate(now, clock::entropy()).as_str()[2..].to_string(),
            kind,
            path: path.clone(),
            task: task.clone(),
            machine: machine.to_string(),
            ts: Timestamp::from_unix_ms(now),
            counter: base_counter + i as u64,
            ttl_secs: (kind == EventType::Acquire).then_some(ttl),
        })
        .collect()
}

/// Acquires every path for `task`, or none (`MT-001` on any conflict).
/// `ttl_secs` overrides `lease.ttl_secs` for these leases (§15
/// `claim_paths`).
pub fn acquire(
    repo_root: &Path,
    config: &Config,
    task: &TaskId,
    paths: &[String],
    ttl_secs: Option<u64>,
) -> Result<SyncOutcome, CliError> {
    let machine = machine_id(config);
    let store = store(repo_root, config)?;
    let batch = events(
        EventType::Acquire,
        task,
        paths,
        &machine,
        ttl_secs.unwrap_or(config.lease.ttl_secs),
    );
    let outcome = store.sync(&batch).map_err(coord_error)?;
    let ids: BTreeSet<&str> = batch.iter().map(|e| e.id.as_str()).collect();
    let rejected: Vec<_> = outcome
        .replay
        .rejected
        .iter()
        .filter(|r| ids.contains(r.event.as_str()))
        .collect();
    if rejected.is_empty() {
        return Ok(outcome);
    }
    let lost: BTreeSet<&str> = rejected.iter().map(|r| r.path.as_str()).collect();
    let granted: Vec<String> = paths
        .iter()
        .filter(|p| !lost.contains(p.as_str()))
        .cloned()
        .collect();
    if !granted.is_empty() {
        let undo = events(EventType::Release, task, &granted, &machine, 0);
        store.sync(&undo).map_err(coord_error)?;
    }
    Err(conflict_error(&rejected))
}

/// Releases `paths` held by `task`.
pub fn release(
    repo_root: &Path,
    config: &Config,
    task: &TaskId,
    paths: &[String],
) -> Result<SyncOutcome, CliError> {
    let machine = machine_id(config);
    let batch = events(EventType::Release, task, paths, &machine, 0);
    store(repo_root, config)?.sync(&batch).map_err(coord_error)
}

/// Loads an `active` task for a claim operation.
pub(crate) fn active_task(repo_root: &Path, raw_id: &str) -> Result<Task, CliError> {
    let id = TaskId::parse(raw_id)
        .ok_or_else(|| CliError::new(USAGE, format!("'{raw_id}' is not a task id")))?;
    let task = registry::read_task(repo_root, &id)?;
    if task.status != TaskStatus::Active {
        return Err(CliError::new(
            TASK,
            format!(
                "task {id} is {}; only active tasks hold claims",
                task.status
            ),
        ));
    }
    Ok(task)
}

/// Result of a claim command, for printing (also the shape of the daemon's
/// `claim_paths` / `release_paths` result).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// Normalized paths acted on.
    pub paths: Vec<String>,
    /// Coordination status: [`push_status`] in-process; from a daemon
    /// `pending`, `committed` or `pushed` (`ADR-0009`).
    pub coord: String,
    /// Why the push was deferred, if it was.
    pub deferred: Option<String>,
}

impl Report {
    fn new(paths: Vec<String>, outcome: &SyncOutcome) -> Self {
        Self {
            paths,
            coord: push_status(outcome),
            deferred: outcome.deferred.clone(),
        }
    }

    /// Parses the daemon's `{paths, coord, deferred}` result.
    pub fn from_json(value: &serde_json::Value) -> Result<Self, CliError> {
        let bad = || CliError::new(DAEMON, format!("malformed daemon claim result: {value}"));
        let paths = value["paths"]
            .as_array()
            .ok_or_else(bad)?
            .iter()
            .map(|p| p.as_str().map(str::to_string).ok_or_else(bad))
            .collect::<Result<_, _>>()?;
        Ok(Self {
            paths,
            coord: value["coord"].as_str().ok_or_else(bad)?.to_string(),
            deferred: value["deferred"].as_str().map(str::to_string),
        })
    }
}

/// `meshtree claim <task-id> <glob>…`: through the daemon when one is
/// running (`sync`: wait for the commit and push), else in-process (always
/// synchronous).
pub fn claim(cwd: &Path, raw_id: &str, globs: &[String], sync: bool) -> Result<Report, CliError> {
    let repo_root = require_repo_root(cwd)?;
    let params = serde_json::json!({ "task_id": raw_id, "paths": globs, "sync": sync });
    if let Some(result) = daemon::call(&repo_root, "claim_paths", params) {
        return Report::from_json(&result?);
    }
    let config = load_config(&repo_root)?;
    Ok(claim_in(&repo_root, &config, raw_id, globs, None)?.1)
}

/// Claims `globs` for an active task and records them in `task.json`.
pub fn claim_in(
    repo_root: &Path,
    config: &Config,
    raw_id: &str,
    globs: &[String],
    ttl_secs: Option<u64>,
) -> Result<(Task, Report), CliError> {
    let paths = normalize_all(globs)?;
    let mut task = active_task(repo_root, raw_id)?;
    let outcome = acquire(repo_root, config, &task.id, &paths, ttl_secs)?;
    for path in &paths {
        if !task.claims.contains(path) {
            task.claims.push(path.clone());
        }
    }
    registry::write_task(repo_root, &task)?;
    Ok((task, Report::new(paths, &outcome)))
}

/// `meshtree release <task-id> <glob>…`: through the daemon when one is
/// running, else in-process.
pub fn release_cmd(cwd: &Path, raw_id: &str, globs: &[String]) -> Result<Report, CliError> {
    let repo_root = require_repo_root(cwd)?;
    let params = serde_json::json!({ "task_id": raw_id, "paths": globs });
    if let Some(result) = daemon::call(&repo_root, "release_paths", params) {
        return Report::from_json(&result?);
    }
    let config = load_config(&repo_root)?;
    Ok(release_in(&repo_root, &config, raw_id, globs)?.1)
}

/// Releases `globs` held by an active task and records it in `task.json`.
pub fn release_in(
    repo_root: &Path,
    config: &Config,
    raw_id: &str,
    globs: &[String],
) -> Result<(Task, Report), CliError> {
    let paths = normalize_all(globs)?;
    let mut task = active_task(repo_root, raw_id)?;
    let outcome = release(repo_root, config, &task.id, &paths)?;
    task.claims.retain(|c| !paths.contains(c));
    registry::write_task(repo_root, &task)?;
    Ok((task, Report::new(paths, &outcome)))
}

/// Publishes `task` to the cross-machine registry (`tasks/<id>.json` in
/// `refs/meshtree/coord`, §5, §8 `spawn` step 4) through the same CAS sync
/// as claims. The record is the task as in `task.json` plus a `revision`
/// (this machine's clock, ns) so the newest write wins across tips.
pub fn register_task(
    repo_root: &Path,
    config: &Config,
    task: &Task,
) -> Result<SyncOutcome, CliError> {
    let mut record = serde_json::to_value(task)
        .map_err(|err| CliError::new(crate::exit::PLATFORM, err.to_string()))?;
    record["revision"] = serde_json::json!(clock::now_unix_nanos());
    let mut bytes = serde_json::to_vec_pretty(&record)
        .map_err(|err| CliError::new(crate::exit::PLATFORM, err.to_string()))?;
    bytes.push(b'\n');
    let records = std::collections::BTreeMap::from([(task.id.to_string(), bytes)]);
    store(repo_root, config)?
        .sync_with(&[], &records)
        .map_err(coord_error)
}

/// [`register_task`] as a best-effort step of a lifecycle command: never
/// fails it, returns a note when the record was not published remotely.
pub fn publish_task(repo_root: &Path, config: &Config, task: &Task) -> Option<String> {
    match register_task(repo_root, config, task) {
        Ok(outcome) => outcome
            .deferred
            .map(|why| format!("task registry push deferred ({why}); run `meshtree sync` later")),
        Err(err) => Some(format!("task registry not updated: {}", err.message)),
    }
}

/// Re-asserts `paths` held by `task` (`HEARTBEAT` events, §9).
pub fn heartbeat(
    repo_root: &Path,
    config: &Config,
    task: &TaskId,
    paths: &[String],
) -> Result<SyncOutcome, CliError> {
    let machine = machine_id(config);
    let batch = events(EventType::Heartbeat, task, paths, &machine, 0);
    store(repo_root, config)?.sync(&batch).map_err(coord_error)
}

/// A local claim revoked by a remote winner (§9: the claiming daemon is
/// notified and revokes its local claim).
pub struct Revocation {
    /// Local task that lost.
    pub task: TaskId,
    /// Revoked path.
    pub path: String,
    /// Winning task.
    pub holder: TaskId,
}

/// `meshtree sync`: flush local events and pull remote ones; revoke local
/// claims that replay now rejects.
pub fn sync(cwd: &Path) -> Result<(SyncOutcome, Vec<Revocation>, PathBuf), CliError> {
    let repo_root = require_repo_root(cwd)?;
    let config = load_config(&repo_root)?;
    let (outcome, revoked) = sync_in(&repo_root, &config)?;
    Ok((outcome, revoked, repo_root))
}

/// [`sync`] for an already-resolved repository (also run by the daemon).
pub fn sync_in(
    repo_root: &Path,
    config: &Config,
) -> Result<(SyncOutcome, Vec<Revocation>), CliError> {
    let outcome = store(repo_root, config)?.sync(&[]).map_err(coord_error)?;
    let revoked = revoke_rejected(repo_root, &outcome)?;
    Ok((outcome, revoked))
}

/// Drops from `task.json` every local claim that `outcome`'s replay now
/// rejects in favor of another task (§9: a remote winner revokes it).
pub fn revoke_rejected(
    repo_root: &Path,
    outcome: &SyncOutcome,
) -> Result<Vec<Revocation>, CliError> {
    let mut revoked = Vec::new();
    for mut task in registry::list_tasks(repo_root)?.tasks {
        let lost: Vec<&meshtree_core::Rejection> = outcome
            .replay
            .rejected
            .iter()
            .filter(|r| r.task == task.id && task.claims.contains(&r.path))
            .filter(|r| {
                !matches!(outcome.replay.leases.get(&r.path), Some(lease) if lease.task == task.id)
            })
            .collect();
        if lost.is_empty() {
            continue;
        }
        for r in &lost {
            revoked.push(Revocation {
                task: task.id.clone(),
                path: r.path.clone(),
                holder: r.holder.clone(),
            });
        }
        task.claims.retain(|c| !lost.iter().any(|r| &r.path == c));
        registry::write_task(repo_root, &task)?;
    }
    Ok(revoked)
}

/// One-line push status for stdout.
pub fn push_status(outcome: &SyncOutcome) -> String {
    match (&outcome.deferred, outcome.pushed) {
        (Some(reason), _) => format!("coord {} (push deferred: {reason})", short(&outcome.oid)),
        (None, true) => format!("coord {} (pushed to {REMOTE})", short(&outcome.oid)),
        (None, false) => format!("coord {} (local)", short(&outcome.oid)),
    }
}

fn short(oid: &str) -> &str {
    &oid[..oid.len().min(12)]
}
