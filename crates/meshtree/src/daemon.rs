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
//! On startup, before binding, [`recovery`] replays the WAL against the task
//! registry and settles collects a crash interrupted (§10 Crash safety).
//!
//! Every status change the daemon makes (the `collect` edges, recovery, the
//! sweeper) is also published to the coordination-ref task registry
//! (`tasks/<id>.json`, `T-047`), best-effort like `spawn` and `abort`;
//! recovery's publishes wait until the endpoint is serving.
//!
//! `claim` and `release` reach the daemon through [`call`] when it is
//! running; when no daemon is reachable the CLI performs the same operation
//! in-process, arbitrated by the coordination-ref CAS (`T-014`).
//!
//! The methods are the §15 MCP tools (`meshtree mcp` proxies each one here),
//! plus `ping`, `heartbeat`, `ticket_status`, `release_shared_op`,
//! `queue_status` and `shutdown`.

use crate::exit::{CliError, DAEMON, PLATFORM, TASK, USAGE};
use crate::repo::{load_config, machine_id, require_repo_root};
use crate::{abort, agents, claims, clock, handoff, merge_gate, registry, spawn};
use meshtree_core::queue::{check_overrides, Holder, QueueTicket, SharedQueue, TICKET_ID_PREFIX};
use meshtree_core::{Config, Task, TaskId, TaskIdAllocator, TaskStatus, Timestamp};
use meshtree_platform::ipc::{
    Endpoint, ErrorBody, IpcClient, IpcError, IpcListener, Request, Response,
    ERROR_CODE_DAEMON_UNREACHABLE,
};
use meshtree_platform::lock::{boot_id, pid_alive, same_boot, DaemonLock, LockError};
use meshtree_platform::teardown::gc_quarantine;
use meshtree_platform::wal::{replay, Wal, WalOp};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Writes a line to stdout, ignoring failure: a daemon must outlive the
/// terminal or pipe that started it (`println!` panics on a closed pipe).
macro_rules! say {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, $($arg)*);
        let _ = out.flush();
    }};
}

/// [`say!`] for stderr, prefixed `meshtree: `.
macro_rules! note {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr().lock(), "meshtree: {}", format_args!($($arg)*));
    }};
}

mod recovery;

/// How often a deferred coordination push is retried.
const FLUSH_EVERY_SECS: u64 = 5;

/// Interval between checks while the daemon waits for its own queue slot.
const SLOT_POLL: Duration = Duration::from_millis(50);

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

    let started_ms = clock::now_unix_ms();
    let ticket_ttl_ms = config.lease.ttl_secs.saturating_mul(1000);
    let daemon = Arc::new(Daemon {
        repo_root,
        config,
        started_ms,
        recovery_ms: AtomicU64::new(0),
        ops: Mutex::new(Inner {
            wal: opened.wal,
            journaled: state.tasks.keys().cloned().collect(),
            last_seen: BTreeMap::new(),
        }),
        merge: Mutex::new(()),
        queues: Mutex::new(Queues {
            by_name: BTreeMap::new(),
            ids: TaskIdAllocator::new(),
            boot: boot_id().ok(),
            ttl_ms: ticket_ttl_ms,
        }),
        dirty: AtomicBool::new(false),
        shutdown: AtomicBool::new(false),
        recovering: AtomicBool::new(true),
        unpublished: Mutex::new(Vec::new()),
    });

    // Recover before accepting clients (§10 Crash safety).
    let recovery_start = std::time::Instant::now();
    let recovered = daemon.recover(&state);
    daemon.recovery_ms.store(
        u64::try_from(recovery_start.elapsed().as_millis()).unwrap_or(u64::MAX),
        Ordering::SeqCst,
    );
    match recovered {
        Ok(report) => {
            if let Some(summary) = report.summary() {
                note!("{summary}");
            }
        }
        Err(err) => note!("recovery incomplete: {}", err.message),
    }

    let listener = IpcListener::bind(&endpoint).map_err(|err| {
        let code = if err.kind() == io::ErrorKind::AddrInUse {
            DAEMON
        } else {
            PLATFORM
        };
        CliError::new(code, format!("bind {}: {err}", endpoint.path().display()))
    })?;
    let handler = Arc::clone(&daemon);
    listener.serve(move |req| handler.handle(req));

    say!(
        "meshtree daemon {} listening on {}",
        std::process::id(),
        endpoint.path().display()
    );
    daemon.publish_recovered();

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

/// `MT-010` for a command that needs the daemon (`what` says why).
pub fn unreachable(what: &str) -> CliError {
    CliError::new(
        DAEMON,
        format!(
            "[{ERROR_CODE_DAEMON_UNREACHABLE}] daemon unreachable; start `meshtree daemon` ({what})"
        ),
    )
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
    /// Duration of startup recovery (§10 target: < 2 s), reported by `ping`.
    recovery_ms: AtomicU64,
    /// Held for every state-changing operation (serializes clients).
    ops: Mutex<Inner>,
    /// The per-machine merge mutex (§8 `collect` step 3).
    merge: Mutex<()>,
    /// Shared-operation queues (§12); separate from `ops` so polling a
    /// ticket never waits behind a coordination sync.
    queues: Mutex<Queues>,
    /// A coordination push was deferred and should be retried.
    dirty: AtomicBool,
    shutdown: AtomicBool,
    /// Startup recovery is running: registry publishes wait in
    /// `unpublished` so pushes cannot delay recovery (§10: < 2 s).
    recovering: AtomicBool,
    unpublished: Mutex<Vec<Task>>,
}

/// Local FIFO queues, created on first use from `[queue.<name>]`.
struct Queues {
    by_name: BTreeMap<String, SharedQueue>,
    ids: TaskIdAllocator,
    /// This boot, for holder liveness (`None` if it could not be read: then
    /// only the pid is checked).
    boot: Option<String>,
    /// Ticket TTL (§12): a granted slot lapses this long after its grant
    /// even if the holder is alive. `lease.ttl_secs`; §6 has no queue key.
    ttl_ms: u64,
}

impl Queues {
    /// Evicts tickets of dead holders (pid + boot id, §12) and slots whose
    /// TTL lapsed, and grants the freed slots.
    fn maintain(&mut self, now: Timestamp) {
        let boot = self.boot.clone();
        let dead = |h: &Holder| {
            boot.as_deref().is_some_and(|b| !same_boot(&h.boot_id, b)) || !pid_alive(h.pid)
        };
        for queue in self.by_name.values_mut() {
            for ticket in queue.evict(dead, now) {
                note!(
                    "queue {}: evicted {} (holder pid {} is gone)",
                    queue.name,
                    ticket.id,
                    ticket.holder.pid
                );
            }
            for ticket in queue.expire(self.ttl_ms, now) {
                note!(
                    "queue {}: {} released (slot TTL of {} s lapsed; holder pid {})",
                    queue.name,
                    ticket.id,
                    self.ttl_ms / 1000,
                    ticket.holder.pid
                );
            }
        }
    }

    fn find(&self, id: &str) -> Option<&QueueTicket> {
        self.by_name.values().find_map(|q| q.get(id))
    }
}

/// The §8 path from `spawning` to `status` used to adopt a task.
fn adoption_path(status: TaskStatus) -> &'static [TaskStatus] {
    use TaskStatus::*;
    match status {
        Spawning => &[Spawning],
        Active => &[Spawning, Active],
        Collecting => &[Spawning, Active, Collecting],
        Merging => &[Spawning, Active, Collecting, Merging],
        Merged => &[Spawning, Active, Collecting, Merging, Merged],
        Failed => &[Spawning, Failed],
        Abandoned => &[Spawning, Active, Abandoned],
    }
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

    /// Adopts a task the WAL has not seen (spawned outside the daemon, or
    /// before the WAL existed): `spawn` plus the §8 edges leading to its
    /// current status, so later transitions replay cleanly.
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
        for pair in adoption_path(task.status).windows(2) {
            self.journal(WalOp::Transition {
                task: task.id.clone(),
                from: pair[0],
                to: pair[1],
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
            "ping" => Ok(json!({
                "pid": std::process::id(),
                "recovery_ms": self.recovery_ms.load(Ordering::SeqCst),
            })),
            "claim_paths" | "release_paths" => {
                let raw_id = str_param(params, "task_id")?;
                let paths = paths_param(params)?;
                let ttl_secs = match params.get("ttl_secs") {
                    None | Some(Value::Null) => None,
                    Some(v) => Some(v.as_u64().filter(|&t| t > 0).ok_or_else(|| {
                        CliError::new(USAGE, "'ttl_secs' must be a positive integer")
                    })?),
                };
                let mut inner = self.lock();
                let (task, report) = if method == "claim_paths" {
                    claims::claim_in(&self.repo_root, &self.config, &raw_id, &paths, ttl_secs)?
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
            "list_tasks" => {
                let active_only = params
                    .get("active_only")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let tasks: Vec<Task> = registry::list_tasks(&self.repo_root)?
                    .tasks
                    .into_iter()
                    .filter(|t| !active_only || is_live(t.status))
                    .collect();
                json_result(serde_json::to_value(&tasks))
            }
            "spawn_task" => self.spawn_task(params),
            "abort_task" => self.abort_task(params),
            "commit_handoff" => {
                let raw_id = str_param(params, "task_id")?;
                let mut fields = params.as_object().cloned().unwrap_or_default();
                fields.remove("task_id");
                let mut inner = self.lock();
                let report =
                    handoff::run(&self.repo_root, &raw_id, handoff::Input::Fields(fields))?;
                inner.touch(&report.task.id);
                let handoff_md = self
                    .repo_root
                    .join(&report.task.worktree)
                    .join(agents::handoff_path(&report.task, "HANDOFF.md"));
                Ok(json!({
                    "task_id": report.task.id,
                    "branch": report.task.branch,
                    "commit": report.commit,
                    "handoff_md": handoff_md,
                    "agents_md": rendered_json(&report.agents),
                }))
            }
            "render_agents_block" => {
                let _inner = self.lock();
                Ok(rendered_json(&agents::render(&self.repo_root)?))
            }
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
            "request_shared_op" => self.request_shared_op(params),
            "collect_task" => {
                let raw_id = str_param(params, "task_id")?;
                let message = params.get("message").and_then(Value::as_str);
                merge_gate::collect(self, &raw_id, message)
            }
            "ticket_status" => {
                let id = str_param(params, "ticket_id")?;
                let mut queues = self.queues();
                queues.maintain(now());
                let ticket = queues.find(&id).ok_or_else(|| unknown_ticket(&id))?;
                json_result(serde_json::to_value(ticket))
            }
            "release_shared_op" => {
                let id = str_param(params, "ticket_id")?;
                let mut queues = self.queues();
                let released = queues
                    .by_name
                    .values_mut()
                    .find_map(|q| q.release(&id, now()))
                    .is_some();
                queues.maintain(now());
                Ok(json!({ "ticket_id": id, "released": released }))
            }
            "queue_status" => {
                let mut queues = self.queues();
                queues.maintain(now());
                let name = params.get("queue").and_then(Value::as_str);
                let tickets: Vec<&QueueTicket> = queues
                    .by_name
                    .values()
                    .filter(|q| name.map_or(true, |n| q.name == n))
                    .flat_map(|q| q.tickets())
                    .collect();
                json_result(serde_json::to_value(tickets))
            }
            "shutdown" => {
                self.shutdown.store(true, Ordering::SeqCst);
                Ok(json!({ "stopping": true }))
            }
            other => Err(CliError::new(USAGE, format!("unknown method '{other}'"))),
        }
    }

    /// `spawn_task` (§8 `spawn`, §15), journaled like an adopted task.
    fn spawn_task(&self, params: &Value) -> Result<Value, CliError> {
        let args = spawn::SpawnArgs {
            name: str_param(params, "name")?,
            base: opt_str_param(params, "base")?,
            agent: opt_str_param(params, "agent")?,
            claims: match params.get("claims") {
                None | Some(Value::Null) => Vec::new(),
                Some(Value::Array(a)) if a.is_empty() => Vec::new(),
                Some(_) => strings_param(params, "claims")?,
            },
        };
        let mut inner = self.lock();
        let report = spawn::run(&self.repo_root, args)?;
        let task = &report.task;
        inner.ensure_journaled(task)?;
        if !task.claims.is_empty() {
            inner.journal(WalOp::Claim {
                task: task.id.clone(),
                paths: task.claims.clone(),
            })?;
        }
        inner.touch(&task.id);
        Ok(json!({
            "task_id": task.id,
            "worktree_path": report.worktree_path,
            "branch": task.branch,
            "claims": task.claims,
            "shares": report.shares.iter().map(ToString::to_string).collect::<Vec<_>>(),
        }))
    }

    /// `abort_task` (§8 `abort`, §15): `active → abandoned`, journaled.
    fn abort_task(&self, params: &Value) -> Result<Value, CliError> {
        let id = task_id_param(params)?;
        let mut inner = self.lock();
        let before = registry::read_task(&self.repo_root, &id)?;
        inner.ensure_journaled(&before)?;
        let (task, note) = abort::run(&self.repo_root, id.as_str())?;
        if !before.claims.is_empty() {
            inner.journal(WalOp::Release {
                task: task.id.clone(),
                paths: before.claims.clone(),
            })?;
        }
        inner.journal(WalOp::Transition {
            task: task.id.clone(),
            from: before.status,
            to: task.status,
        })?;
        Ok(json!({ "task_id": task.id, "status": task.status, "note": note }))
    }

    /// `request_shared_op` (§12, §15): validates the queue, task and
    /// overrides (§17, `ADR-0005`), then issues a FIFO ticket for the
    /// calling process (`pid`, `boot_id`).
    fn request_shared_op(&self, params: &Value) -> Result<Value, CliError> {
        let name = str_param(params, "queue")?;
        let config = self
            .config
            .queue
            .get(&name)
            .ok_or_else(|| CliError::new(USAGE, format!("unknown queue '{name}'")))?;
        let id = task_id_param(params)?;
        let task = registry::read_task(&self.repo_root, &id)?;
        if task.status != TaskStatus::Active {
            return Err(CliError::new(
                TASK,
                format!("task {id} is {}; only active tasks use queues", task.status),
            ));
        }
        let args: Vec<String> = match params.get("command_override") {
            None | Some(Value::Null) => Vec::new(),
            Some(_) => strings_param(params, "command_override")?,
        };
        check_overrides(&name, &config.allow_args, &args)
            .map_err(|err| CliError::new(USAGE, err.to_string()))?;
        let pid = params
            .get("pid")
            .and_then(Value::as_u64)
            .and_then(|p| u32::try_from(p).ok())
            .ok_or_else(|| CliError::new(USAGE, "missing integer parameter 'pid'"))?;
        let holder = Holder {
            pid,
            boot_id: str_param(params, "boot_id")?,
        };
        let mut command = config.command.clone();
        for arg in &args {
            command.push(' ');
            command.push_str(arg);
        }

        let ticket = self.issue_ticket(&name, &task.id, holder, args);
        let mut result = json_result(serde_json::to_value(ticket))?;
        result["command"] = json!(command);
        result["worktree"] = json!(self.repo_root.join(&task.worktree));
        self.lock().touch(&task.id);
        Ok(result)
    }

    /// Issues a FIFO ticket in queue `name` (created on first use with its
    /// configured concurrency, or 1 for an internal queue such as
    /// `validate`).
    fn issue_ticket(
        &self,
        name: &str,
        task: &TaskId,
        holder: Holder,
        args: Vec<String>,
    ) -> QueueTicket {
        let concurrency = self.config.queue.get(name).map_or(1, |q| q.concurrency);
        let mut queues = self.queues();
        let now = now();
        queues.maintain(now);
        let ulid = queues.ids.allocate(now.unix_ms(), clock::entropy());
        let ticket_id = format!("{TICKET_ID_PREFIX}{}", &ulid.as_str()[2..]);
        queues
            .by_name
            .entry(name.to_string())
            .or_insert_with(|| SharedQueue::new(name, concurrency))
            .request(ticket_id, task.clone(), holder, args, now)
            .clone()
    }

    fn queues(&self) -> std::sync::MutexGuard<'_, Queues> {
        self.queues
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A panicking handler thread must not wedge every later request.
        self.ops
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// One supervisor pass: heartbeat live claims, abandon silent tasks,
    /// retry quarantined trees (§10 step 4, idle GC).
    fn supervise(&self, now_ms: u64) {
        let gc = gc_quarantine(&crate::worktree::quarantine_dir(&self.repo_root));
        for path in &gc.removed {
            note!("quarantine GC removed {}", path.display());
        }
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
        self.publish(&task);
        Ok(())
    }

    /// Publishes `task` to the coordination-ref task registry (§5, §8,
    /// `T-047`), best-effort: a failure is logged and never fails the
    /// transition; a deferred push is retried by the flush loop. Callers
    /// hold the operation mutex, so registry writes are serialized with
    /// claims.
    fn publish(&self, task: &Task) {
        if self.recovering.load(Ordering::SeqCst) {
            self.unpublished
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(task.clone());
            return;
        }
        if let Some(note) = claims::publish_task(&self.repo_root, &self.config, task) {
            note!("{}: {note}", task.id);
            self.dirty.store(true, Ordering::SeqCst);
        }
    }

    /// Ends recovery and publishes the transitions it made.
    fn publish_recovered(&self) {
        self.recovering.store(false, Ordering::SeqCst);
        let tasks = std::mem::take(
            &mut *self
                .unpublished
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        if tasks.is_empty() {
            return;
        }
        let _inner = self.lock();
        for task in &tasks {
            self.publish(task);
        }
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

impl merge_gate::Host for Daemon {
    fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    fn config(&self) -> &Config {
        &self.config
    }

    fn transition(&self, task: &mut Task, to: TaskStatus) -> Result<(), CliError> {
        let mut inner = self.lock();
        inner.ensure_journaled(task)?;
        let from = task.status;
        task.transition(to)
            .map_err(|err| CliError::new(TASK, err.to_string()))?;
        registry::write_task(&self.repo_root, task)?;
        inner.journal(WalOp::Transition {
            task: task.id.clone(),
            from,
            to,
        })?;
        inner.touch(&task.id);
        self.publish(task);
        Ok(())
    }

    fn with_slot<T>(
        &self,
        queue: &str,
        task: &TaskId,
        f: impl FnOnce() -> T,
    ) -> Result<T, CliError> {
        let holder = Holder {
            pid: std::process::id(),
            boot_id: self.queues().boot.clone().unwrap_or_default(),
        };
        let id = self.issue_ticket(queue, task, holder, Vec::new()).id;
        loop {
            {
                let mut queues = self.queues();
                queues.maintain(now());
                match queues.find(&id) {
                    Some(ticket) if ticket.granted => break,
                    Some(_) => {}
                    None => return Err(unknown_ticket(&id)),
                }
            }
            std::thread::sleep(SLOT_POLL);
        }
        let out = f();
        let mut queues = self.queues();
        let now = now();
        let _ = queues
            .by_name
            .values_mut()
            .find_map(|q| q.release(&id, now));
        Ok(out)
    }

    fn release_claims(&self, task: &mut Task) -> Result<(), CliError> {
        if task.claims.is_empty() {
            return Ok(());
        }
        let mut inner = self.lock();
        inner.ensure_journaled(task)?;
        let outcome = claims::release(&self.repo_root, &self.config, &task.id, &task.claims)?;
        if outcome.deferred.is_some() {
            self.dirty.store(true, Ordering::SeqCst);
        }
        inner.journal(WalOp::Release {
            task: task.id.clone(),
            paths: std::mem::take(&mut task.claims),
        })?;
        registry::write_task(&self.repo_root, task)
    }

    fn merge_mutex(&self) -> std::sync::MutexGuard<'_, ()> {
        self.merge
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Statuses listed as active sessions (`AGENTS.md`, `list_tasks
/// active_only`): the task's worktree is still in play.
fn is_live(status: TaskStatus) -> bool {
    matches!(
        status,
        TaskStatus::Active | TaskStatus::Collecting | TaskStatus::Merging
    )
}

fn rendered_json(rendered: &agents::Rendered) -> Value {
    json!({
        "path": rendered.path,
        "changed": rendered.changed,
        "sessions": rendered.sessions,
        "block": rendered.block,
        "warnings": rendered.warnings,
    })
}

fn opt_str_param(params: &Value, key: &str) -> Result<Option<String>, CliError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(CliError::new(
            USAGE,
            format!("parameter '{key}' must be a string"),
        )),
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

fn now() -> Timestamp {
    Timestamp::from_unix_ms(clock::now_unix_ms())
}

fn unknown_ticket(id: &str) -> CliError {
    CliError::new(
        TASK,
        format!("unknown ticket {id} (released, or evicted after its holder exited)"),
    )
}

fn paths_param(params: &Value) -> Result<Vec<String>, CliError> {
    strings_param(params, "paths")
}

/// A non-empty array of strings under `key`.
fn strings_param(params: &Value, key: &str) -> Result<Vec<String>, CliError> {
    let bad = || {
        CliError::new(
            USAGE,
            format!("'{key}' must be a non-empty array of strings"),
        )
    };
    let paths = params
        .get(key)
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
