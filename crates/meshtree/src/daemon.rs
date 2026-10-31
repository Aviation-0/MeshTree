//! `meshtree daemon`: the per-repository background service.
//!
//! Authoritative specification: `SPEC.md` §4 (daemon hosts state; CLI and
//! MCP are IPC clients), §8 (`ttl_secs × 4` abandoned sweeper), §9 (lease
//! heartbeats, deferred pushes), §10 (IPC, singleton `daemon.lock`, WAL),
//! §14 (exit codes), §16 (`MT-010`, `MT-011`).
//!
//! The daemon takes `daemon.lock` (adopting a stale one, `MT-011`), opens the
//! WAL, binds the IPC endpoint, and serves NDJSON requests. Every mutating
//! request runs under one operation mutex, so racing clients are serialized
//! and exactly one of two conflicting claims wins (`MT-001` for the other).
//! A supervisor loop heartbeats the leases of live tasks, sweeps tasks unseen
//! for `ttl_secs × 4` to `abandoned` (their worktrees stay in place for the
//! `T-024` quarantine GC), and retries deferred coordination-ref pushes.
//!
//! `claim` and `release` reach the daemon through [`call`] when it is
//! running; when no daemon is reachable the CLI performs the same operation
//! in-process, arbitrated by the coordination-ref CAS (`T-014`).

use crate::exit::{CliError, DAEMON, PLATFORM, TASK, USAGE};
use crate::repo::{load_config, machine_id, require_repo_root};
use crate::{claims, clock, registry};
use meshtree_core::{Config, Task, TaskId, TaskStatus};
use meshtree_platform::ipc::{
    Endpoint, ErrorBody, IpcClient, IpcError, IpcListener, Request, Response,
};
use meshtree_platform::lock::{DaemonLock, LockError};
use meshtree_platform::wal::{replay, Wal, WalOp};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Writes a line to stdout, ignoring failure: a daemon must outlive the
/// terminal or pipe that started it (`println!` panics on a closed pipe).
macro_rules! say {
    ($($arg:tt)*) => {{
        let mut out = io::stdout().lock();
        let _ = writeln!(out, $($arg)*);
        let _ = out.flush();
    }};
}

/// [`say!`] for stderr, prefixed `meshtree: `.
macro_rules! note {
    ($($arg:tt)*) => {{
        let _ = writeln!(io::stderr().lock(), "meshtree: {}", format_args!($($arg)*));
    }};
}

/// How often a deferred coordination push is retried.
const FLUSH_EVERY_SECS: u64 = 5;

/// Supervisor loop granularity (also the shutdown latency).
const TICK: Duration = Duration::from_millis(100);

/// Runs `meshtree daemon` until a `shutdown` request.
pub fn run(cwd: &Path) -> Result<(), CliError> {
    let repo_root = require_repo_root(cwd)?;
    let config = load_config(&repo_root)?;
    let machine = machine_id(&config);

    let acquired = DaemonLock::acquire(&repo_root, &machine).map_err(|err| match err {
        LockError::Held { .. } | LockError::Foreign { .. } => {
            CliError::new(DAEMON, err.to_string())
        }
        LockError::Io { .. } => CliError::new(PLATFORM, err.to_string()),
    })?;
    if let Some(stale) = &acquired.recovered {
        note!("{stale}");
    }

    let opened =
        Wal::open(&repo_root).map_err(|err| CliError::new(PLATFORM, format!("open WAL: {err}")))?;
    if let Some(torn) = opened.torn {
        note!("WAL recovery: {torn}");
    }
    let state = replay(&opened.entries);
    for anomaly in &state.anomalies {
        note!("WAL entry {} skipped: {}", anomaly.seq, anomaly.message);
    }

    let endpoint = Endpoint::for_repo(&repo_root)
        .map_err(|err| CliError::new(PLATFORM, format!("IPC endpoint: {err}")))?;
    let listener = IpcListener::bind(&endpoint).map_err(|err| {
        let code = if err.kind() == io::ErrorKind::AddrInUse {
            DAEMON
        } else {
            PLATFORM
        };
        CliError::new(code, format!("bind {}: {err}", endpoint.path().display()))
    })?;

    let started_ms = clock::now_unix_ms();
    let daemon = Arc::new(Daemon {
        repo_root,
        config,
        started_ms,
        ops: Mutex::new(Inner {
            wal: opened.wal,
            journaled: state.tasks.keys().cloned().collect(),
            last_seen: BTreeMap::new(),
        }),
        dirty: AtomicBool::new(false),
        shutdown: AtomicBool::new(false),
    });
    let handler = Arc::clone(&daemon);
    listener.serve(move |req| handler.handle(req));

    say!(
        "meshtree daemon {} listening on {}",
        std::process::id(),
        endpoint.path().display()
    );

    let mut last_beat = started_ms;
    let mut last_flush = started_ms;
    while !daemon.shutdown.load(Ordering::SeqCst) {
        std::thread::sleep(TICK);
        let now = clock::now_unix_ms();
        if due(last_beat, now, daemon.config.lease.heartbeat_secs) {
            last_beat = now;
            daemon.supervise(now);
        }
        if daemon.dirty.load(Ordering::SeqCst) && due(last_flush, now, FLUSH_EVERY_SECS) {
            last_flush = now;
            daemon.flush();
        }
    }
    say!("meshtree daemon {} stopped", std::process::id());
    drop(acquired);
    Ok(())
}

/// Sends one request to the repository's daemon. `None` when no daemon is
/// running (`MT-010`), so the caller can work in-process.
pub fn call(repo_root: &Path, method: &str, params: Value) -> Option<Result<Value, CliError>> {
    let endpoint = match Endpoint::for_repo(repo_root) {
        Ok(endpoint) => endpoint,
        Err(err) => return Some(Err(CliError::new(PLATFORM, format!("IPC endpoint: {err}")))),
    };
    let mut client = match IpcClient::connect(&endpoint) {
        Ok(client) => client,
        Err(IpcError::Unreachable { .. }) => return None,
        Err(err) => return Some(Err(CliError::new(DAEMON, err.to_string()))),
    };
    Some(match client.call(method, params) {
        Ok(Response {
            ok: true,
            result: Some(result),
            ..
        }) => Ok(result),
        Ok(Response {
            error: Some(body), ..
        }) => Err(cli_error(&body)),
        Ok(other) => Err(CliError::new(
            DAEMON,
            format!("malformed daemon response: {other:?}"),
        )),
        Err(err) => Err(CliError::new(DAEMON, err.to_string())),
    })
}

/// Wire error code for a CLI failure: the `MT-xxx` code named in its message
/// (`[MT-001] …`), else its §14 exit category.
pub fn error_code(err: &CliError) -> String {
    if let Some(start) = err.message.find("[MT-") {
        let code = &err.message.as_bytes()[start + 1..];
        if code.len() > 6 && code[3..6].iter().all(u8::is_ascii_digit) && code[6] == b']' {
            return String::from_utf8_lossy(&code[..6]).into_owned();
        }
    }
    match err.code {
        USAGE => "usage",
        TASK => "task",
        DAEMON => "daemon",
        _ => "platform",
    }
    .to_string()
}

/// Inverse of [`error_code`]: the §14 exit code for a daemon error body.
pub fn cli_error(body: &ErrorBody) -> CliError {
    let code = match body.code.as_str() {
        "usage" | "MT-050" => USAGE,
        "task" => TASK,
        "daemon" => DAEMON,
        "platform" => PLATFORM,
        c if c.starts_with("MT-00") || c.starts_with("MT-03") => TASK,
        c if c.starts_with("MT-01") => DAEMON,
        _ => PLATFORM,
    };
    CliError::new(code, body.message.clone())
}

/// `true` when at least `every_secs` have passed since `last_ms`.
fn due(last_ms: u64, now_ms: u64, every_secs: u64) -> bool {
    now_ms.saturating_sub(last_ms) >= every_secs.saturating_mul(1000)
}

/// What one supervisor pass does with the active tasks.
#[derive(Debug, Default, PartialEq, Eq)]
struct Plan {
    /// Live tasks whose claims get `HEARTBEAT` events.
    heartbeat: Vec<TaskId>,
    /// Tasks unseen for `ttl_secs × 4`: transition to `abandoned`.
    abandon: Vec<TaskId>,
}

/// Pure supervisor decision (`SPEC.md` §8 sweeper, §9 heartbeats). A task's
/// last sign of life is its latest request, or the daemon start if none.
fn plan(
    tasks: &[Task],
    last_seen: &BTreeMap<TaskId, u64>,
    started_ms: u64,
    now_ms: u64,
    ttl_secs: u64,
) -> Plan {
    let mut plan = Plan::default();
    for task in tasks.iter().filter(|t| t.status == TaskStatus::Active) {
        let seen = last_seen.get(&task.id).copied().unwrap_or(started_ms);
        if due(seen, now_ms, ttl_secs.saturating_mul(4)) {
            plan.abandon.push(task.id.clone());
        } else if !task.claims.is_empty() {
            plan.heartbeat.push(task.id.clone());
        }
    }
    plan
}

struct Daemon {
    repo_root: PathBuf,
    config: Config,
    started_ms: u64,
    /// Held for every state-changing operation (serializes clients).
    ops: Mutex<Inner>,
    /// A coordination push was deferred and should be retried.
    dirty: AtomicBool,
    shutdown: AtomicBool,
}

struct Inner {
    wal: Wal,
    /// Tasks with a `spawn` entry in the WAL.
    journaled: BTreeSet<TaskId>,
    /// Last request per task (unix ms).
    last_seen: BTreeMap<TaskId, u64>,
}

impl Inner {
    fn journal(&mut self, op: WalOp) -> Result<(), CliError> {
        self.wal
            .append(clock::now_unix_ms(), op)
            .map(drop)
            .map_err(|err| CliError::new(PLATFORM, format!("WAL append: {err}")))
    }

    /// Adopts a task spawned outside the daemon into the WAL (`spawn`, plus
    /// `spawning → active` when it is active).
    fn ensure_journaled(&mut self, task: &Task) -> Result<(), CliError> {
        if self.journaled.contains(&task.id) {
            return Ok(());
        }
        self.journal(WalOp::Spawn {
            task: task.id.clone(),
            branch: task.branch.clone(),
            worktree: task.worktree.clone(),
            base: task.base.clone(),
        })?;
        if task.status == TaskStatus::Active {
            self.journal(WalOp::Transition {
                task: task.id.clone(),
                from: TaskStatus::Spawning,
                to: TaskStatus::Active,
            })?;
        }
        self.journaled.insert(task.id.clone());
        Ok(())
    }

    fn touch(&mut self, task: &TaskId) {
        self.last_seen.insert(task.clone(), clock::now_unix_ms());
    }
}

impl Daemon {
    fn handle(&self, req: Request) -> Response {
        match self.dispatch(&req.method, &req.params) {
            Ok(result) => Response::ok(req.id, result),
            Err(err) => Response::err(req.id, error_code(&err), err.message),
        }
    }

    fn dispatch(&self, method: &str, params: &Value) -> Result<Value, CliError> {
        match method {
            "ping" => Ok(json!({ "pid": std::process::id() })),
            "claim_paths" | "release_paths" => {
                let raw_id = str_param(params, "task_id")?;
                let paths = paths_param(params)?;
                let mut inner = self.lock();
                let (task, report) = if method == "claim_paths" {
                    claims::claim_in(&self.repo_root, &self.config, &raw_id, &paths)?
                } else {
                    claims::release_in(&self.repo_root, &self.config, &raw_id, &paths)?
                };
                inner.ensure_journaled(&task)?;
                let paths = report.paths.clone();
                inner.journal(if method == "claim_paths" {
                    WalOp::Claim {
                        task: task.id.clone(),
                        paths,
                    }
                } else {
                    WalOp::Release {
                        task: task.id.clone(),
                        paths,
                    }
                })?;
                inner.touch(&task.id);
                if report.deferred.is_some() {
                    self.dirty.store(true, Ordering::SeqCst);
                }
                Ok(report.to_json())
            }
            "heartbeat" => {
                let id = task_id_param(params)?;
                let task = registry::read_task(&self.repo_root, &id)?;
                self.lock().touch(&task.id);
                Ok(json!({ "task_id": task.id, "status": task.status }))
            }
            "task_status" => match params.get("task_id").and_then(Value::as_str) {
                Some(_) => {
                    let id = task_id_param(params)?;
                    let task = registry::read_task(&self.repo_root, &id)?;
                    json_result(serde_json::to_value(&task))
                }
                None => json_result(serde_json::to_value(
                    &registry::list_tasks(&self.repo_root)?.tasks,
                )),
            },
            "sync" => {
                let _inner = self.lock();
                let (outcome, revoked) = claims::sync_in(&self.repo_root, &self.config)?;
                self.dirty
                    .store(outcome.deferred.is_some(), Ordering::SeqCst);
                Ok(json!({
                    "coord": claims::push_status(&outcome),
                    "leases": outcome.replay.leases.len(),
                    "revoked": revoked.iter().map(|r| json!({
                        "task_id": r.task, "path": r.path, "holder": r.holder,
                    })).collect::<Vec<_>>(),
                }))
            }
            "shutdown" => {
                self.shutdown.store(true, Ordering::SeqCst);
                Ok(json!({ "stopping": true }))
            }
            other => Err(CliError::new(USAGE, format!("unknown method '{other}'"))),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A panicking handler thread must not wedge every later request.
        self.ops
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// One supervisor pass: heartbeat live claims, abandon silent tasks.
    fn supervise(&self, now_ms: u64) {
        let mut inner = self.lock();
        let tasks = match registry::list_tasks(&self.repo_root) {
            Ok(listing) => listing.tasks,
            Err(err) => {
                note!("supervisor: {}", err.message);
                return;
            }
        };
        let plan = plan(
            &tasks,
            &inner.last_seen,
            self.started_ms,
            now_ms,
            self.config.lease.ttl_secs,
        );
        for task in tasks.iter().filter(|t| plan.heartbeat.contains(&t.id)) {
            match claims::heartbeat(&self.repo_root, &self.config, &task.id, &task.claims) {
                Ok(outcome) if outcome.deferred.is_some() => {
                    self.dirty.store(true, Ordering::SeqCst)
                }
                Ok(_) => {}
                Err(err) => note!("heartbeat {}: {}", task.id, err.message),
            }
        }
        for task in tasks.iter().filter(|t| plan.abandon.contains(&t.id)) {
            if let Err(err) = self.abandon(&mut inner, task.clone()) {
                note!("sweep {}: {}", task.id, err.message);
            }
        }
    }

    /// `active → abandoned` for a task unseen for `ttl_secs × 4` (§8).
    fn abandon(&self, inner: &mut Inner, mut task: Task) -> Result<(), CliError> {
        inner.ensure_journaled(&task)?;
        if !task.claims.is_empty() {
            claims::release(&self.repo_root, &self.config, &task.id, &task.claims)?;
            inner.journal(WalOp::Release {
                task: task.id.clone(),
                paths: std::mem::take(&mut task.claims),
            })?;
        }
        let from = task.status;
        task.transition(TaskStatus::Abandoned)
            .map_err(|err| CliError::new(TASK, err.to_string()))?;
        registry::write_task(&self.repo_root, &task)?;
        inner.journal(WalOp::Transition {
            task: task.id.clone(),
            from,
            to: TaskStatus::Abandoned,
        })?;
        note!("{} abandoned (no activity for ttl_secs x 4)", task.id);
        Ok(())
    }

    /// Retries a deferred coordination push.
    fn flush(&self) {
        let _inner = self.lock();
        match claims::sync_in(&self.repo_root, &self.config) {
            Ok((outcome, _)) => self
                .dirty
                .store(outcome.deferred.is_some(), Ordering::SeqCst),
            Err(err) => note!("flush: {}", err.message),
        }
    }
}

fn json_result(value: serde_json::Result<Value>) -> Result<Value, CliError> {
    value.map_err(|err| CliError::new(PLATFORM, err.to_string()))
}

fn str_param(params: &Value, key: &str) -> Result<String, CliError> {
    params
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| CliError::new(USAGE, format!("missing string parameter '{key}'")))
}

fn task_id_param(params: &Value) -> Result<TaskId, CliError> {
    let raw = str_param(params, "task_id")?;
    TaskId::parse(&raw).ok_or_else(|| CliError::new(USAGE, format!("'{raw}' is not a task id")))
}

fn paths_param(params: &Value) -> Result<Vec<String>, CliError> {
    let bad = || CliError::new(USAGE, "'paths' must be a non-empty array of strings");
    let paths = params
        .get("paths")
        .and_then(Value::as_array)
        .ok_or_else(bad)?
        .iter()
        .map(|p| p.as_str().map(str::to_string).ok_or_else(bad))
        .collect::<Result<Vec<_>, _>>()?;
    if paths.is_empty() {
        return Err(bad());
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use meshtree_core::{TaskIdAllocator, TaskSpec};

    fn task(n: u128, status: TaskStatus, claims: &[&str]) -> Task {
        let mut t = Task::new(
            TaskIdAllocator::new().allocate(1, n),
            TaskSpec {
                name: format!("t{n}"),
                agent: None,
                machine: "mc-test".into(),
                base: "main".into(),
                created_at: "2026-11-03T14:02:11Z".into(),
                claims: claims.iter().map(|c| c.to_string()).collect(),
            },
        );
        t.status = status;
        t
    }

    #[test]
    fn supervisor_plan_heartbeats_live_and_sweeps_after_ttl_x4() {
        let ttl = 900;
        let start = 1_000_000;
        let quiet = task(1, TaskStatus::Active, &["src/a/**"]);
        let busy = task(2, TaskStatus::Active, &["src/b/**"]);
        let unclaimed = task(3, TaskStatus::Active, &[]);
        let merged = task(4, TaskStatus::Merged, &[]);
        let tasks = [quiet.clone(), busy.clone(), unclaimed.clone(), merged];
        let mut seen = BTreeMap::new();
        seen.insert(busy.id.clone(), start + 3_000_000);

        // Just under ttl × 4 since daemon start: nothing abandoned.
        let edge = start + ttl * 4 * 1000 - 1;
        let p = plan(&tasks, &seen, start, edge, ttl);
        assert!(p.abandon.is_empty());
        assert_eq!(p.heartbeat, vec![quiet.id.clone(), busy.id.clone()]);

        // At ttl × 4: silent tasks (claimed or not) are swept; the busy one
        // (seen later) keeps heartbeating; terminal tasks are ignored.
        let p = plan(&tasks, &seen, start, edge + 1, ttl);
        assert_eq!(p.abandon, vec![quiet.id.clone(), unclaimed.id.clone()]);
        assert_eq!(p.heartbeat, vec![busy.id.clone()]);
    }

    #[test]
    fn due_is_inclusive_and_saturating() {
        assert!(due(0, 5_000, 5));
        assert!(!due(0, 4_999, 5));
        assert!(!due(10_000, 5_000, 5), "clock going backwards is not due");
        assert!(due(0, 0, 0));
    }

    #[test]
    fn error_codes_roundtrip_to_exit_codes() {
        let cases = [
            (
                CliError::new(TASK, "[MT-001] claim conflict: x"),
                "MT-001",
                TASK,
            ),
            (CliError::new(TASK, "[MT-004] a -> b"), "MT-004", TASK),
            (CliError::new(USAGE, "'x' is not a task id"), "usage", USAGE),
            (CliError::new(USAGE, "[MT-050] bad config"), "MT-050", USAGE),
            (CliError::new(TASK, "unknown task t-1"), "task", TASK),
            (CliError::new(PLATFORM, "disk full"), "platform", PLATFORM),
            (CliError::new(DAEMON, "[MT-011] stale"), "MT-011", DAEMON),
            (
                CliError::new(PLATFORM, "odd [MT-0 text"),
                "platform",
                PLATFORM,
            ),
        ];
        for (err, code, exit) in cases {
            assert_eq!(error_code(&err), code, "{err:?}");
            let back = cli_error(&ErrorBody {
                code: code.to_string(),
                message: err.message.clone(),
            });
            assert_eq!(back, CliError::new(exit, err.message.clone()));
        }
    }
}
