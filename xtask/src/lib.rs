//! MeshTree benchmark suite (`cargo run --release -p xtask -- bench`).
//!
//! Authoritative specification: `SPEC.md` §18 "Benchmark suite" (six
//! scenarios), §20 (Weeks 7–8), §22, Appendix B (`xtask/`).
//!
//! Every scenario drives the real `meshtree` binary against scratch
//! repositories, the way agents do, and reports a measured value against the
//! §18 target:
//!
//! | # | Scenario | Measured |
//! |---|---|---|
//! | 1 | spawn with a shared `node_modules` | slowest `meshtree spawn` wall time |
//! | 2 | local claim | p95 of `claim_paths` round trips through the daemon |
//! | 3 | cross-machine claim CAS round | p95 overhead of `meshtree claim` on clone A over a paired plain-git fetch + `--force-with-lease` push (`ADR-0012`); network calls per claim; B's visibility |
//! | 4 | two agents, one repo | silent overwrites with MeshTree vs a shared-checkout baseline |
//! | 5 | `index.lock` collisions | concurrent git index writers, through a concurrency-1 queue vs directly |
//! | 6 | crash recovery | daemon restart after `SIGKILL` / `TerminateProcess` until it serves again |
//!
//! [`Options::full`] uses the §18 sizes (2 GB `node_modules`, long session);
//! [`Options::smoke`] runs the same code at small sizes for CI. Scenario 3
//! uses a local bare `file://` remote unless [`Options::remote`] names a real
//! one (a throwaway repository: the bench pushes `main` and
//! `refs/meshtree/coord` to it).

use meshtree_platform::ipc::{Endpoint, IpcClient};
use serde_json::{json, Value};
use std::fmt::Write as _;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Benchmark parameters.
#[derive(Debug, Clone)]
pub struct Options {
    /// The `meshtree` binary under test.
    pub meshtree: PathBuf,
    /// Size of the generated primary `node_modules` (scenario 1).
    pub node_modules_mb: u64,
    /// Spawns measured in scenario 1.
    pub spawn_samples: usize,
    /// Claims measured in scenario 2.
    pub claim_samples: usize,
    /// CAS rounds measured in scenario 3.
    pub cas_samples: usize,
    /// Remote for scenario 3 (`None`: a local bare `file://` repository).
    pub remote: Option<String>,
    /// Rounds of the scripted two-agent session (scenario 4).
    pub session_rounds: usize,
    /// Edits per agent per round (scenario 4).
    pub edits_per_round: usize,
    /// Concurrent index writers (scenario 5).
    pub lock_workers: usize,
    /// Index writes per worker (scenario 5).
    pub lock_ops: usize,
    /// Daemon kill/restart cycles (scenario 6).
    pub recovery_samples: usize,
    /// Tasks the killed daemon has to recover (scenario 6).
    pub recovery_tasks: usize,
    /// Seed of the scripted agent plan (scenario 4).
    pub seed: u64,
}

impl Options {
    /// The §18 scale.
    pub fn full(meshtree: PathBuf) -> Self {
        Self {
            meshtree,
            node_modules_mb: 2048,
            spawn_samples: 3,
            claim_samples: 200,
            cas_samples: 30,
            remote: None,
            session_rounds: 20,
            edits_per_round: 5,
            lock_workers: 8,
            lock_ops: 25,
            recovery_samples: 5,
            recovery_tasks: 20,
            seed: 0x5eed_f00d_3e57_7ee5,
        }
    }

    /// Small sizes for CI: same code paths, minutes instead of an hour.
    pub fn smoke(meshtree: PathBuf) -> Self {
        Self {
            node_modules_mb: 4,
            spawn_samples: 2,
            claim_samples: 20,
            cas_samples: 4,
            session_rounds: 2,
            edits_per_round: 3,
            lock_workers: 4,
            lock_ops: 6,
            recovery_samples: 2,
            recovery_tasks: 3,
            ..Self::full(meshtree)
        }
    }
}

/// One scenario's result.
#[derive(Debug, Clone)]
pub struct Row {
    /// §18 scenario name.
    pub scenario: &'static str,
    /// §18 target, as text.
    pub target: &'static str,
    /// Measured value, as text.
    pub measured: String,
    /// `true` when the measured value meets the target.
    pub pass: bool,
    /// How it was measured.
    pub notes: String,
    /// Named numbers behind `measured`, for tests (empty for most rows).
    pub metrics: Vec<(&'static str, f64)>,
}

impl Row {
    /// The metric named `name`, if the row records it.
    pub fn metric(&self, name: &str) -> Option<f64> {
        self.metrics
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| *v)
    }
}

/// All six rows, in §18 order.
#[derive(Debug, Clone)]
pub struct Report {
    pub rows: Vec<Row>,
    pub options: Options,
    /// Total wall time of the run.
    pub elapsed: Duration,
}

impl Report {
    /// The report as Markdown (`docs/benchmarks.md`).
    pub fn to_markdown(&self, scale: &str) -> String {
        let o = &self.options;
        let mut md = String::new();
        let _ = writeln!(md, "# MeshTree Benchmarks\n");
        let _ = writeln!(
            md,
            "Generated by `cargo run --release -p xtask -- bench` (`SPEC.md` §18). \
             Scale: **{scale}**. Host: `{}-{}`. Total run time: {:.1} s.\n",
            std::env::consts::OS,
            std::env::consts::ARCH,
            self.elapsed.as_secs_f64()
        );
        let _ = writeln!(md, "| Scenario | Target (§18) | Measured | Met | How |");
        let _ = writeln!(md, "|---|---|---|---|---|");
        for row in &self.rows {
            let _ = writeln!(
                md,
                "| {} | {} | {} | {} | {} |",
                row.scenario,
                row.target,
                row.measured,
                if row.pass { "yes" } else { "**no**" },
                row.notes
            );
        }
        let _ = writeln!(
            md,
            "\nParameters: `node_modules` {} MB, {} spawns, {} local claims, {} CAS rounds \
             (remote: {}), {} session rounds × {} edits × 2 agents, {} index writers × {} \
             writes, {} recoveries of {} tasks, seed {:#x}.",
            o.node_modules_mb,
            o.spawn_samples,
            o.claim_samples,
            o.cas_samples,
            o.remote
                .as_deref()
                .unwrap_or("local bare file:// repository"),
            o.session_rounds,
            o.edits_per_round,
            o.lock_workers,
            o.lock_ops,
            o.recovery_samples,
            o.recovery_tasks,
            o.seed
        );
        md
    }
}

/// Runs all six scenarios.
pub fn run(opts: &Options) -> Result<Report, String> {
    let start = Instant::now();
    let rows = vec![
        spawn_shared(opts)?,
        claim_local(opts)?,
        claim_cas(opts)?,
        two_agents(opts)?,
        index_lock(opts)?,
        crash_recovery(opts)?,
    ];
    Ok(Report {
        rows,
        options: opts.clone(),
        elapsed: start.elapsed(),
    })
}

// ---------------------------------------------------------------------------
// Scenarios

const CONFIG: &str = r#"[project]
base_branch = "main"
integration_branch = "meshtree/integration"
validate = ["git --version"]
promote_validate = ["git --version"]

[[share]]
source = "node_modules"
strategy = "auto"

[queue.index]
concurrency = 1
command = "git add -A"
"#;

/// 1. Spawn on a repository with a large `node_modules` (link-based sharing).
fn spawn_shared(opts: &Options) -> Result<Row, String> {
    let repo = Fixture::new(opts)?;
    let modules = repo.root().join("node_modules");
    let files = (opts.node_modules_mb * 16).max(1);
    let chunk = vec![b'x'; 64 * 1024];
    for i in 0..files {
        let dir = modules
            .join(format!("pkg-{}", i / 64))
            .join(format!("m{i}"));
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        fs::write(dir.join("index.js"), &chunk).map_err(|e| e.to_string())?;
    }
    let mut times = Vec::new();
    let mut linked = true;
    for i in 0..opts.spawn_samples {
        let begin = Instant::now();
        let out = repo.meshtree(&["spawn", &format!("spawn-{i}")])?;
        times.push(begin.elapsed());
        let id = task_id(&out)?;
        let link = repo
            .root()
            .join(".meshtree/worktrees")
            .join(&id)
            .join("node_modules");
        linked &= fs::symlink_metadata(&link).is_ok_and(|m| m.file_type().is_symlink());
    }
    let worst = times.iter().max().copied().unwrap_or_default();
    Ok(Row {
        scenario: "Spawn with shared `node_modules`",
        target: "< 5 s",
        measured: format!("{} ms (max of {})", worst.as_millis(), times.len()),
        pass: worst < Duration::from_secs(5) && linked,
        notes: format!(
            "{} MB in {files} files; `meshtree spawn` wall time incl. process start; \
             node_modules linked: {linked}",
            opts.node_modules_mb
        ),
        metrics: Vec::new(),
    })
}

/// 2. Local claim acquire through the daemon.
fn claim_local(opts: &Options) -> Result<Row, String> {
    let mut parts = Vec::new();
    let mut worst = Duration::ZERO;
    for &logged in &LOG_SIZES {
        let repo = Fixture::new(opts)?;
        let id = task_id(&repo.meshtree(&["spawn", "claims"])?)?;
        seed_log(repo.root(), logged)?;
        let daemon = Daemon::start(opts, repo.root())?;
        let mut samples = Vec::new();
        for i in 0..opts.claim_samples {
            let path = format!("bench/claim-{i}/**");
            let begin = Instant::now();
            daemon.call("claim_paths", json!({ "task_id": id, "paths": [path] }))?;
            samples.push(begin.elapsed());
            daemon.call("release_paths", json!({ "task_id": id, "paths": [path] }))?;
        }
        daemon.stop()?;
        let p95 = percentile(&mut samples, 95);
        worst = worst.max(p95);
        parts.push((logged, p95, samples[samples.len() / 2]));
    }
    Ok(Row {
        scenario: "Claim acquire (local)",
        target: "p95 < 50 ms",
        measured: parts
            .iter()
            .map(|(n, p95, _)| format!("p95 {:.1} ms ({n} lifetime events)", ms(*p95)))
            .collect::<Vec<_>>()
            .join("; "),
        pass: worst < Duration::from_millis(50),
        notes: format!(
            "{} `claim_paths` IPC round trips per history size (the log is pre-seeded \
             with acquire/release pairs older than the compaction horizon, as a \
             long-lived repository accrues them; the daemon compacts them, ADR-0010; \
             no remote); medians {}",
            opts.claim_samples,
            parts
                .iter()
                .map(|(n, _, median)| format!("{:.1} ms at {n}", ms(*median)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        metrics: Vec::new(),
    })
}

/// Lifetime-event counts the local claim is measured at (`ADR-0009`,
/// `ADR-0010`: claim cost must not grow with history; §18 "≥ 10,000 lifetime
/// events").
const LOG_SIZES: [usize; 3] = [100, 2_000, 10_000];

/// Pre-seeds `refs/meshtree/coord` with `n` events of finished history:
/// ACQUIRE/RELEASE pairs on distinct paths by an unrelated task, spread over
/// the 30 days ending two days ago (older than the compaction horizon), in
/// one sync round without compaction.
fn seed_log(root: &Path, n: usize) -> Result<(), String> {
    use meshtree_core::lease::{EventType, LeaseEvent, ReplayConfig};
    use meshtree_core::{TaskId, TaskIdAllocator, Timestamp};
    const DAY_MS: u64 = 24 * 60 * 60 * 1000;
    let holder = TaskId::from_ulid(1);
    let mut alloc = TaskIdAllocator::new();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_millis() as u64;
    let start = now - 32 * DAY_MS;
    let step = (30 * DAY_MS / n.max(1) as u64).max(1);
    let events: Vec<LeaseEvent> = (0..n)
        .map(|i| {
            let pair = (i / 2) as u64;
            let ms = start + pair * 2 * step + (i % 2) as u64;
            let kind = if i % 2 == 0 {
                EventType::Acquire
            } else {
                EventType::Release
            };
            LeaseEvent {
                id: alloc.allocate(ms, i as u128 + 1).as_str()[2..].to_string(),
                kind,
                path: format!("seed/{pair}/**"),
                task: holder.clone(),
                machine: "mc-bench-seed".to_string(),
                ts: Timestamp::from_unix_ms(ms),
                counter: i as u64,
                ttl_secs: (kind == EventType::Acquire).then_some(900),
            }
        })
        .collect();
    let store = meshtree_platform::coord::CoordStore::new(
        meshtree_platform::gitops::CliGitOps::new(root),
        None,
        ReplayConfig {
            default_ttl_secs: 900,
            skew_grace_secs: 600,
        },
    );
    store.sync(&events).map(drop).map_err(|e| e.to_string())
}

/// Ref the plain-git floor rounds push to: same remote, same kind of
/// update, without touching `refs/meshtree/coord`.
const FLOOR_REF: &str = "refs/meshtree-bench/floor";

/// 3. Cross-machine claim (`ADR-0012`): MeshTree's overhead over the git CAS.
///
/// Each sample pairs a plain-git floor round (fetch [`FLOOR_REF`], then a
/// `--force-with-lease` push of a new commit, from a separate clone) with a
/// `meshtree claim` round on clone A, alternating which runs first so network
/// drift hits both alike. Overhead = claim − floor, per pair. Every claim runs
/// under `GIT_TRACE` to count its fetches and pushes. Clone B's `sync` until
/// it sees the lease is timed separately as cross-machine visibility.
fn claim_cas(opts: &Options) -> Result<Row, String> {
    let scratch = Scratch::new()?;
    let (url, label) = match &opts.remote {
        Some(url) => (url.clone(), "configured remote"),
        None => {
            let bare = scratch.path().join("remote.git");
            git(scratch.path(), &["init", "-q", "--bare", &path_str(&bare)])?;
            (
                file_url(&bare),
                "local bare file:// remote (no network RTT)",
            )
        }
    };
    let seed = Fixture::new(opts)?;
    git(seed.root(), &["push", "-q", &url, "main"])?;
    let clone = |name: &str| -> Result<PathBuf, String> {
        let dir = scratch.path().join(name);
        git(
            scratch.path(),
            &["clone", "-q", "-b", "main", &url, &path_str(&dir)],
        )?;
        Ok(dir)
    };
    let (a, b, floor) = (clone("a")?, clone("b")?, clone("floor")?);
    for dir in [&a, &b] {
        mt(&opts.meshtree, dir, &["init"])?;
    }
    let id = task_id(&mt(&opts.meshtree, &a, &["spawn", "cas"])?)?;
    // The floor ref exists before timing starts, like the coordination ref
    // after the first claim.
    let mut floor_tip = floor_commit(&floor, None)?;
    git(
        &floor,
        &["push", "-q", "origin", &format!("{floor_tip}:{FLOOR_REF}")],
    )?;

    let trace = scratch.path().join("trace.txt");
    let (mut floors, mut claims, mut visibility) = (Vec::new(), Vec::new(), Vec::new());
    let mut overheads = Vec::new();
    let (mut max_fetches, mut max_pushes) = (0usize, 0usize);
    for i in 0..opts.cas_samples {
        let mut floor_round = || -> Result<Duration, String> {
            let begin = Instant::now();
            floor_tip = floor_cas(&floor, &floor_tip)?;
            Ok(begin.elapsed())
        };
        let claim_round = || -> Result<(Duration, usize, usize), String> {
            fs::write(&trace, "").map_err(|e| e.to_string())?;
            let begin = Instant::now();
            mt_env(
                &opts.meshtree,
                &a,
                &["claim", &id, &format!("cas/{i}/**")],
                &[("GIT_TRACE", &path_str(&trace))],
            )?;
            let took = begin.elapsed();
            let (fetches, pushes) = network_calls(&fs::read_to_string(&trace).unwrap_or_default());
            Ok((took, fetches, pushes))
        };
        let (floor_took, (claim_took, fetches, pushes)) = if i % 2 == 0 {
            let f = floor_round()?;
            (f, claim_round()?)
        } else {
            let c = claim_round()?;
            (floor_round()?, c)
        };
        let begin = Instant::now();
        let out = mt(&opts.meshtree, &b, &["sync"])?;
        visibility.push(begin.elapsed());
        if !String::from_utf8_lossy(&out.stdout).contains(&format!("leases: {}", i + 1)) {
            return Err(format!("clone B did not see claim {i}: {out:?}"));
        }
        overheads.push(ms(claim_took) - ms(floor_took));
        floors.push(floor_took);
        claims.push(claim_took);
        max_fetches = max_fetches.max(fetches);
        max_pushes = max_pushes.max(pushes);
    }
    let floor_p50 = ms(percentile(&mut floors, 50));
    let floor_p95 = ms(percentile(&mut floors, 95));
    let claim_p50 = ms(percentile(&mut claims, 50));
    let claim_p95 = ms(percentile(&mut claims, 95));
    let visibility_p95 = ms(percentile(&mut visibility, 95));
    let overhead_p50 = percentile_f64(&mut overheads, 50);
    let overhead_p95 = percentile_f64(&mut overheads, 95);
    let pass = overhead_p95 < 500.0 && max_fetches <= 1 && max_pushes <= 1;
    Ok(Row {
        scenario: "Cross-machine claim CAS round",
        target: "overhead over the git floor p95 < 500 ms; ≤ 1 fetch + 1 push per claim",
        measured: format!(
            "overhead p95 {overhead_p95:.0} ms, median {overhead_p50:.0} ms (claim p95 \
             {claim_p95:.0} / median {claim_p50:.0} ms vs git floor p95 {floor_p95:.0} / median \
             {floor_p50:.0} ms); at most {max_fetches} fetch + {max_pushes} push per claim; B \
             sees the lease: p95 {visibility_p95:.0} ms"
        ),
        pass,
        notes: format!(
            "{} pairs, alternating order: plain-git fetch + `--force-with-lease` push of \
             `{FLOOR_REF}` from a separate clone, and `meshtree claim` on clone A (fetch, \
             replay, CAS push; network calls counted with `GIT_TRACE`); then `meshtree sync` \
             on clone B until it sees the lease; {label}",
            claims.len()
        ),
        metrics: vec![
            ("floor_p50_ms", floor_p50),
            ("floor_p95_ms", floor_p95),
            ("claim_p50_ms", claim_p50),
            ("claim_p95_ms", claim_p95),
            ("overhead_p50_ms", overhead_p50),
            ("overhead_p95_ms", overhead_p95),
            ("visibility_p95_ms", visibility_p95),
            ("max_fetches", max_fetches as f64),
            ("max_pushes", max_pushes as f64),
        ],
    })
}

/// Git's built-in empty tree (SHA-1), always present in every repository.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// A new commit on top of `parent` (an empty tree): the floor's payload.
fn floor_commit(dir: &Path, parent: Option<&str>) -> Result<String, String> {
    let mut args = vec![
        "-c",
        "user.name=MeshTree Bench",
        "-c",
        "user.email=bench@meshtree.invalid",
        "commit-tree",
        EMPTY_TREE,
        "-m",
        "floor",
    ];
    if let Some(parent) = parent {
        args.extend(["-p", parent]);
    }
    Ok(git(dir, &args)?.trim().to_string())
}

/// One plain-git CAS round: fetch [`FLOOR_REF`], push a child commit with
/// `--force-with-lease` on the fetched tip. Returns the new tip.
fn floor_cas(dir: &Path, expected: &str) -> Result<String, String> {
    git(
        dir,
        &[
            "fetch",
            "--quiet",
            "--no-tags",
            "origin",
            &format!("+{FLOOR_REF}:refs/remotes/floor"),
        ],
    )?;
    let fetched = git(dir, &["rev-parse", "refs/remotes/floor"])?;
    let fetched = fetched.trim();
    if fetched != expected {
        return Err(format!(
            "floor ref moved unexpectedly: {fetched} != {expected}"
        ));
    }
    let next = floor_commit(dir, Some(fetched))?;
    git(
        dir,
        &[
            "push",
            "-q",
            &format!("--force-with-lease={FLOOR_REF}:{fetched}"),
            "origin",
            &format!("{next}:{FLOOR_REF}"),
        ],
    )?;
    Ok(next)
}

/// Network calls in a `GIT_TRACE` log: (`fetch` + `ls-remote`, `push`).
fn network_calls(trace: &str) -> (usize, usize) {
    let count = |cmd: &str| {
        trace
            .lines()
            .filter(|l| l.contains(&format!("built-in: git {cmd}")))
            .count()
    };
    (count("fetch ") + count("ls-remote "), count("push "))
}

/// Shared files both agents edit in scenario 4.
const POOL: usize = 6;

/// 4. Two agents editing one repository: MeshTree vs a shared checkout.
fn two_agents(opts: &Options) -> Result<Row, String> {
    let with = session_meshtree(opts)?;
    let without = session_baseline(opts)?;
    Ok(Row {
        scenario: "Two agents, one repo",
        target: "0 silent overwrites (baseline N > 0)",
        measured: format!(
            "{} with MeshTree; baseline {} of {}",
            with.silent, without.silent, without.acknowledged
        ),
        pass: with.silent == 0,
        notes: format!(
            "scripted session: {} rounds × {} edits × 2 concurrent agents over {POOL} shared \
             files (seed-driven); MeshTree: {} edits acknowledged, {} claim conflicts and {} \
             merge-gate refusals reported to the agent, none silent; baseline: both agents \
             read-modify-write one shared checkout",
            opts.session_rounds,
            opts.edits_per_round,
            with.acknowledged,
            with.conflicts,
            with.refused
        ),
        metrics: Vec::new(),
    })
}

/// Outcome of one scripted session.
#[derive(Debug, Default)]
struct Session {
    acknowledged: usize,
    silent: usize,
    conflicts: usize,
    refused: usize,
}

/// One agent's plan: which pool file each edit targets.
fn plan(seed: u64, agent: u64, rounds: usize, edits: usize) -> Vec<Vec<usize>> {
    let mut rng = Rng(seed ^ (agent.wrapping_add(1)).wrapping_mul(0x9e37_79b9_7f4a_7c15));
    (0..rounds)
        .map(|_| {
            (0..edits)
                .map(|_| rng.below(POOL as u64) as usize)
                .collect()
        })
        .collect()
}

fn pool_file(i: usize) -> String {
    format!("src/f{i}.txt")
}

fn session_meshtree(opts: &Options) -> Result<Session, String> {
    let repo = Fixture::new(opts)?;
    let root = repo.root().to_path_buf();
    for i in 0..POOL {
        let path = root.join(pool_file(i));
        fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
        fs::write(&path, format!("file {i}\n")).map_err(|e| e.to_string())?;
    }
    git(&root, &["add", "src"])?;
    commit(&root, "pool")?;
    let daemon = Daemon::start(opts, &root)?;
    let plans = [
        plan(opts.seed, 0, opts.session_rounds, opts.edits_per_round),
        plan(opts.seed, 1, opts.session_rounds, opts.edits_per_round),
    ];
    let mut total = Session::default();
    // (file, line, merged?) for every acknowledged edit.
    let mut ledger: Vec<(String, String, bool)> = Vec::new();
    for (round, edits) in plans[0].iter().zip(&plans[1]).enumerate() {
        let edits = [edits.0, edits.1];
        let base = if git(
            &root,
            &["rev-parse", "--verify", "-q", "meshtree/integration"],
        )
        .is_ok()
        {
            "meshtree/integration"
        } else {
            "main"
        };
        let results: Vec<Result<AgentRound, String>> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..2)
                .map(|agent| {
                    let (root, plan) = (&root, edits[agent]);
                    s.spawn(move || agent_round(opts, root, base, agent, round, plan))
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        for result in results {
            let r = result?;
            total.acknowledged += r.lines.len();
            total.conflicts += r.conflicts;
            total.refused += usize::from(!r.merged);
            ledger.extend(r.lines.into_iter().map(|(f, l)| (f, l, r.merged)));
        }
    }
    daemon.stop()?;
    for (file, line, merged) in &ledger {
        let text =
            git(&root, &["show", &format!("meshtree/integration:{file}")]).unwrap_or_default();
        if *merged && !text.lines().any(|l| l == line) {
            total.silent += 1;
        }
    }
    Ok(total)
}

struct AgentRound {
    /// `(file, line)` of each acknowledged edit.
    lines: Vec<(String, String)>,
    conflicts: usize,
    merged: bool,
}

fn agent_round(
    opts: &Options,
    root: &Path,
    base: &str,
    agent: usize,
    round: usize,
    plan: &[usize],
) -> Result<AgentRound, String> {
    let name = format!("agent{agent}-r{round}");
    let id = task_id(&mt(
        &opts.meshtree,
        root,
        &["spawn", &name, "--base", base],
    )?)?;
    let wt = root.join(".meshtree/worktrees").join(&id);
    let mut out = AgentRound {
        lines: Vec::new(),
        conflicts: 0,
        merged: false,
    };
    let mut held: Vec<usize> = Vec::new();
    for (edit, &want) in plan.iter().enumerate() {
        // Claim before editing; on MT-001 try the next file (the agent is
        // told, so this is never silent).
        let mut target = None;
        for k in 0..POOL {
            let file = (want + k) % POOL;
            if held.contains(&file) {
                target = Some(file);
                break;
            }
            let claim = Command::new(&opts.meshtree)
                .args(["claim", &id, &pool_file(file)])
                .current_dir(root)
                .output()
                .map_err(|e| e.to_string())?;
            match claim.status.code() {
                Some(0) => {
                    held.push(file);
                    target = Some(file);
                    break;
                }
                Some(20) => out.conflicts += 1,
                _ => return Err(format!("claim failed: {claim:?}")),
            }
        }
        let Some(file) = target else { continue };
        let rel = pool_file(file);
        let line = format!("{name} edit {edit}");
        let path = wt.join(&rel);
        let mut text = fs::read_to_string(&path).map_err(|e| e.to_string())?;
        text.push_str(&line);
        text.push('\n');
        fs::write(&path, text).map_err(|e| e.to_string())?;
        git(&wt, &["add", &rel])?;
        commit(&wt, &line)?;
        out.lines.push((rel, line));
    }
    let collect = Command::new(&opts.meshtree)
        .args(["collect", &id])
        .current_dir(root)
        .output()
        .map_err(|e| e.to_string())?;
    match collect.status.code() {
        Some(0) => out.merged = true,
        // MT-031 / MT-032: reported to the agent, never silent.
        Some(20) => {}
        _ => return Err(format!("collect failed: {collect:?}")),
    }
    Ok(out)
}

fn session_baseline(opts: &Options) -> Result<Session, String> {
    let scratch = Scratch::new()?;
    let root = scratch.path().to_path_buf();
    for i in 0..POOL {
        let path = root.join(pool_file(i));
        fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
        fs::write(&path, format!("file {i}\n")).map_err(|e| e.to_string())?;
    }
    let lines: Vec<Vec<(String, String)>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..2u64)
            .map(|agent| {
                let root = &root;
                let plan = plan(opts.seed, agent, opts.session_rounds, opts.edits_per_round);
                s.spawn(move || {
                    let mut rng = Rng(opts.seed ^ agent);
                    let mut done = Vec::new();
                    for (round, edits) in plan.iter().enumerate() {
                        for (edit, &file) in edits.iter().enumerate() {
                            // An editor buffer: read, think, write the whole file.
                            let rel = pool_file(file);
                            let path = root.join(&rel);
                            let text = fs::read_to_string(&path).unwrap_or_default();
                            std::thread::sleep(Duration::from_millis(rng.below(4)));
                            let line = format!("agent{agent}-r{round} edit {edit}");
                            let _ = fs::write(&path, format!("{text}{line}\n"));
                            done.push((rel, line));
                        }
                    }
                    done
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut session = Session::default();
    for (file, line) in lines.into_iter().flatten() {
        session.acknowledged += 1;
        let text = fs::read_to_string(root.join(&file)).unwrap_or_default();
        if !text.lines().any(|l| l == line) {
            session.silent += 1;
        }
    }
    Ok(session)
}

/// 5. Concurrent writers of one git index: through the queue vs directly.
fn index_lock(opts: &Options) -> Result<Row, String> {
    let repo = Fixture::new(opts)?;
    let id = task_id(&repo.meshtree(&["spawn", "index"])?)?;
    let wt = repo.root().join(".meshtree/worktrees").join(&id);
    let baseline = writers(opts, &wt, "direct", |w, i| {
        Command::new("git")
            .args(["add", "-A"])
            .current_dir(&wt)
            .output()
            .map(|o| (o, format!("{w}-{i}")))
    })?;
    let daemon = Daemon::start(opts, repo.root())?;
    let queued = writers(opts, &wt, "queued", |w, i| {
        Command::new(&opts.meshtree)
            .args(["queue", "index", "request", "--task", &id])
            .current_dir(repo.root())
            .stdin(Stdio::null())
            .output()
            .map(|o| (o, format!("{w}-{i}")))
    })?;
    daemon.stop()?;
    Ok(Row {
        scenario: "`index.lock` collisions",
        target: "0 with queue (vs baseline)",
        measured: format!(
            "{} with queue; baseline {} of {}",
            queued.collisions, baseline.collisions, baseline.ops
        ),
        pass: queued.collisions == 0 && queued.failures == 0,
        notes: format!(
            "{} concurrent workers × {} `git add -A` in one task worktree, each after writing \
             a new file; queue `index` with concurrency 1 (`meshtree queue index request`); \
             other queued failures: {}",
            opts.lock_workers, opts.lock_ops, queued.failures
        ),
        metrics: Vec::new(),
    })
}

#[derive(Debug, Default)]
struct Writers {
    ops: usize,
    collisions: usize,
    failures: usize,
}

fn writers(
    opts: &Options,
    wt: &Path,
    tag: &str,
    op: impl Fn(usize, usize) -> std::io::Result<(Output, String)> + Sync,
) -> Result<Writers, String> {
    let results: Vec<Vec<Output>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..opts.lock_workers)
            .map(|w| {
                let op = &op;
                s.spawn(move || {
                    (0..opts.lock_ops)
                        .filter_map(|i| {
                            let _ = fs::write(wt.join(format!("{tag}-{w}-{i}.txt")), "x");
                            op(w, i).ok().map(|(o, _)| o)
                        })
                        .collect()
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut out = Writers::default();
    for o in results.into_iter().flatten() {
        out.ops += 1;
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        );
        if text.contains("index.lock") {
            out.collisions += 1;
        } else if !o.status.success() {
            out.failures += 1;
        }
    }
    Ok(out)
}

/// 6. Crash recovery: kill the daemon, restart it, time until it serves.
fn crash_recovery(opts: &Options) -> Result<Row, String> {
    let repo = Fixture::new(opts)?;
    let mut ids = Vec::new();
    for i in 0..opts.recovery_tasks {
        ids.push(task_id(&repo.meshtree(&["spawn", &format!("r{i}")])?)?);
    }
    let mut restarts = Vec::new();
    let mut reported = Vec::new();
    for round in 0..opts.recovery_samples {
        let daemon = Daemon::start(opts, repo.root())?;
        for (i, id) in ids.iter().enumerate() {
            let path = format!("crash/{round}/{i}/**");
            daemon.call("claim_paths", json!({ "task_id": id, "paths": [path] }))?;
        }
        daemon.kill();
        let begin = Instant::now();
        let daemon = Daemon::start(opts, repo.root())?;
        let ping = daemon.call("ping", json!({}))?;
        restarts.push(begin.elapsed());
        reported.push(ping["recovery_ms"].as_u64().unwrap_or(u64::MAX));
        daemon.stop()?;
    }
    let worst = restarts.iter().max().copied().unwrap_or_default();
    Ok(Row {
        scenario: "Crash recovery",
        target: "< 2 s",
        measured: format!(
            "{} ms to serving (max of {}); WAL recovery {} ms",
            worst.as_millis(),
            restarts.len(),
            reported.iter().max().copied().unwrap_or(0)
        ),
        pass: worst < Duration::from_secs(2),
        notes: format!(
            "daemon killed without warning after journaling claims for {} tasks, then \
             restarted; time from process start to its first answered `ping` (mid-merge \
             kills: chaos test `chaos_sigkill_daemon_mid_merge_recovers_under_2s`)",
            opts.recovery_tasks
        ),
        metrics: Vec::new(),
    })
}

// ---------------------------------------------------------------------------
// Fixtures and process helpers

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A scratch directory, removed on drop (links unlinked first).
pub struct Scratch(PathBuf);

impl Scratch {
    pub fn new() -> Result<Self, String> {
        // Short: macOS temp paths are long and the daemon socket must fit.
        let path = std::env::temp_dir().join(format!(
            "mtb-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).map_err(|e| e.to_string())?;
        Ok(Self(path))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Never let a recursive delete reach a shared directory via a link.
        for root in [self.0.clone(), self.0.join("repo")] {
            let worktrees = root.join(".meshtree/worktrees");
            for entry in fs::read_dir(worktrees).into_iter().flatten().flatten() {
                let _ = meshtree_platform::share::unlink(&entry.path().join("node_modules"));
            }
        }
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// An initialized MeshTree repository in a scratch directory.
struct Fixture {
    _scratch: Scratch,
    root: PathBuf,
    meshtree: PathBuf,
}

impl Fixture {
    fn new(opts: &Options) -> Result<Self, String> {
        let scratch = Scratch::new()?;
        let root = scratch.path().join("repo");
        fs::create_dir_all(&root).map_err(|e| e.to_string())?;
        git(&root, &["init", "-q", "-b", "main"])?;
        fs::write(root.join("README.md"), "bench\n").map_err(|e| e.to_string())?;
        fs::write(root.join(".gitignore"), "node_modules/\n").map_err(|e| e.to_string())?;
        git(&root, &["add", "README.md", ".gitignore"])?;
        commit(&root, "initial")?;
        mt(&opts.meshtree, &root, &["init"])?;
        fs::write(root.join(".meshtree/config.toml"), CONFIG).map_err(|e| e.to_string())?;
        Ok(Self {
            _scratch: scratch,
            root,
            meshtree: opts.meshtree.clone(),
        })
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn meshtree(&self, args: &[&str]) -> Result<Output, String> {
        mt(&self.meshtree, &self.root, args)
    }
}

/// A running `meshtree daemon`.
struct Daemon {
    child: Child,
    _stdout: BufReader<ChildStdout>,
    root: PathBuf,
}

impl Daemon {
    fn start(opts: &Options, root: &Path) -> Result<Self, String> {
        let mut child = Command::new(&opts.meshtree)
            .arg("daemon")
            .current_dir(root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("start daemon: {e}"))?;
        let mut stdout = BufReader::new(child.stdout.take().expect("piped"));
        let mut line = String::new();
        stdout.read_line(&mut line).map_err(|e| e.to_string())?;
        if !line.contains("listening on") {
            let _ = child.kill();
            return Err(format!("daemon did not start: {line:?}"));
        }
        Ok(Self {
            child,
            _stdout: stdout,
            root: root.to_path_buf(),
        })
    }

    fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let endpoint = Endpoint::for_repo(&self.root).map_err(|e| e.to_string())?;
        let resp = IpcClient::connect(&endpoint)
            .map_err(|e| e.to_string())?
            .call(method, params)
            .map_err(|e| e.to_string())?;
        match (resp.ok, resp.result) {
            (true, Some(result)) => Ok(result),
            _ => Err(format!("{method}: {:?}", resp.error)),
        }
    }

    fn stop(mut self) -> Result<(), String> {
        self.call("shutdown", json!({}))?;
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Err("daemon did not stop".into())
    }

    fn kill(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// Runs `meshtree <args>` in `cwd`, requiring success.
fn mt(bin: &Path, cwd: &Path, args: &[&str]) -> Result<Output, String> {
    let out = Command::new(bin)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("run {}: {e}", bin.display()))?;
    if out.status.success() {
        Ok(out)
    } else {
        Err(format!(
            "meshtree {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}

/// [`mt`] with extra environment variables.
fn mt_env(bin: &Path, cwd: &Path, args: &[&str], envs: &[(&str, &str)]) -> Result<Output, String> {
    let out = Command::new(bin)
        .args(args)
        .current_dir(cwd)
        .envs(envs.iter().copied())
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("run {}: {e}", bin.display()))?;
    if out.status.success() {
        Ok(out)
    } else {
        Err(format!(
            "meshtree {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}

/// Runs `git <args>` in `cwd`, requiring success; returns stdout.
fn git(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("run git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}

fn commit(cwd: &Path, message: &str) -> Result<(), String> {
    git(
        cwd,
        &[
            "-c",
            "user.name=MeshTree Bench",
            "-c",
            "user.email=bench@meshtree.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            message,
        ],
    )
    .map(drop)
}

fn task_id(out: &Output) -> Result<String, String> {
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("task_id: "))
        .map(str::to_string)
        .ok_or_else(|| format!("no task_id in {out:?}"))
}

fn path_str(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn file_url(path: &Path) -> String {
    let p = path.to_string_lossy().replace('\\', "/");
    if p.starts_with('/') {
        format!("file://{p}")
    } else {
        format!("file:///{p}")
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Nearest-rank percentile of signed values (`p` in 1..=100); sorts `samples`.
pub fn percentile_f64(samples: &mut [f64], p: usize) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    samples.sort_by(f64::total_cmp);
    let rank = (p * samples.len()).div_ceil(100).max(1);
    samples[rank - 1]
}

/// Nearest-rank percentile (`p` in 1..=100); sorts `samples`.
pub fn percentile(samples: &mut [Duration], p: usize) -> Duration {
    if samples.is_empty() {
        return Duration::ZERO;
    }
    samples.sort();
    let rank = (p * samples.len()).div_ceil(100).max(1);
    samples[rank - 1]
}

/// xorshift64*: a tiny deterministic generator for the agent plans.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0.max(1);
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_is_nearest_rank() {
        let mut v: Vec<Duration> = (1..=100).rev().map(Duration::from_millis).collect();
        assert_eq!(percentile(&mut v, 95), Duration::from_millis(95));
        assert_eq!(percentile(&mut v, 50), Duration::from_millis(50));
        let mut one = vec![Duration::from_millis(7)];
        assert_eq!(percentile(&mut one, 95), Duration::from_millis(7));
        assert_eq!(percentile(&mut [], 95), Duration::ZERO);
    }

    #[test]
    fn percentile_f64_handles_negative_overheads() {
        let mut v = vec![-40.0, 120.0, 5.0, -3.0, 80.0];
        assert_eq!(percentile_f64(&mut v, 95), 120.0);
        assert_eq!(percentile_f64(&mut v, 20), -40.0);
        assert_eq!(percentile_f64(&mut [], 95), 0.0);
    }

    #[test]
    fn network_calls_counts_fetches_and_pushes_in_a_trace() {
        let trace = "\
12:00:00.1 git.c:463 trace: built-in: git fetch --quiet --no-tags --prune origin +a*:b*
12:00:00.2 run-command.c:673 trace: run_command: git-upload-pack '/r.git'
12:00:00.3 git.c:463 trace: built-in: git rev-parse HEAD
12:00:00.4 git.c:463 trace: built-in: git push --force-with-lease=x:y origin z:x
12:00:00.5 git.c:463 trace: built-in: git ls-remote origin x
";
        assert_eq!(network_calls(trace), (2, 1));
        assert_eq!(network_calls(""), (0, 0));
    }

    #[test]
    fn agent_plans_are_deterministic_and_differ() {
        let a = plan(42, 0, 3, 4);
        assert_eq!(a, plan(42, 0, 3, 4));
        assert_ne!(a, plan(42, 1, 3, 4));
        assert!(a.iter().flatten().all(|&f| f < POOL));
    }
}
