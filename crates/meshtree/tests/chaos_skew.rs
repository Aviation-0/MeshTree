//! §18 chaos tests: clock-skew injection (±30 min) and four-clone CAS
//! contention on `refs/meshtree/coord` (`SPEC.md` §9, §18, §21).
//!
//! Every "machine" is a real clone syncing through a bare `file://` remote
//! with `CoordStore`. Machine clocks are injected: each machine stamps its
//! events with `real time + its offset`, so skew is exact and nothing sleeps.
//! Observers decide what to sweep with `meshtree_core::lease::sweep_candidates`
//! and emit `EXPIRE_SWEEP` events exactly as a daemon would.

mod common;

use common::{git, TempDir};
use meshtree_core::lease::{
    sweep_candidates, EventType, LeaseEvent, Replay, ReplayConfig, ERROR_CODE_CLAIM_CONFLICT,
};
use meshtree_core::{conflicts_raw, TaskId, TaskIdAllocator, Timestamp};
use meshtree_platform::coord::{CoordError, CoordStore, SyncOutcome, COORD_REF, MAX_ATTEMPTS};
use meshtree_platform::gitops::{CliGitOps, GitError, GitOps, TreeEntry, WorktreeInfo};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// ±30 minutes, in seconds.
const SKEW: i64 = 30 * 60;
const TTL: u64 = 900;

static EVENT_SEQ: AtomicU64 = AtomicU64::new(0);

/// A machine: one clone with an injected clock offset.
struct Machine {
    name: &'static str,
    offset_secs: i64,
    store: CoordStore<CliGitOps>,
    _dir: TempDir,
}

impl Machine {
    fn new(remote: &TempDir, name: &'static str, offset_secs: i64, config: ReplayConfig) -> Self {
        let dir = TempDir::repo();
        git(dir.path(), &["remote", "add", "origin", &remote.file_url()]);
        let store = CoordStore::new(CliGitOps::new(dir.path()), Some("origin".into()), config);
        Self {
            name,
            offset_secs,
            store,
            _dir: dir,
        }
    }

    /// This machine's clock reading at real time `real_secs`.
    fn clock(&self, real_secs: i64) -> Timestamp {
        Timestamp::from_unix_ms(((real_secs + self.offset_secs) * 1000) as u64)
    }

    fn event(&self, kind: EventType, path: &str, task: &TaskId, real_secs: i64) -> LeaseEvent {
        event(kind, path, task, self.name, self.clock(real_secs))
    }

    fn sync(&self, events: &[LeaseEvent]) -> SyncOutcome {
        let out = self.store.sync(events).expect("sync converges");
        assert!(
            out.pushed && out.deferred.is_none(),
            "{}: {out:?}",
            self.name
        );
        out
    }

    /// What this machine's daemon would sweep at real time `real_secs`: an
    /// `EXPIRE_SWEEP` per lease its replay shows expired on its own clock.
    fn sweeps(&self, replay: &Replay, real_secs: i64, grace: u64) -> Vec<LeaseEvent> {
        let now = self.clock(real_secs);
        sweep_candidates(&replay.leases, now, grace)
            .into_iter()
            .map(|lease| {
                event(
                    EventType::ExpireSweep,
                    &lease.path,
                    &lease.task,
                    self.name,
                    now,
                )
            })
            .collect()
    }
}

fn event(kind: EventType, path: &str, task: &TaskId, machine: &str, ts: Timestamp) -> LeaseEvent {
    let n = EVENT_SEQ.fetch_add(1, Ordering::Relaxed);
    LeaseEvent {
        // ULID-shaped (26 Crockford chars), unique per event.
        id: format!("01JCHAQS{n:018}"),
        kind,
        path: path.to_string(),
        task: task.clone(),
        machine: machine.to_string(),
        ts,
        counter: n,
        ttl_secs: (kind == EventType::Acquire).then_some(TTL),
    }
}

fn replay_bytes(replay: &Replay) -> Vec<u8> {
    serde_json::to_vec(replay).unwrap()
}

fn log_kinds(store: &CoordStore<CliGitOps>, oid: &str, kind: EventType) -> usize {
    let (log, issues) = store.read_log(oid).unwrap();
    assert!(issues.is_empty(), "{issues:?}");
    log.iter().filter(|e| e.kind == kind).count()
}

/// A holder and two observers whose clocks run 30 min fast and 30 min slow
/// relative to it share `refs/meshtree/coord`, with `skew_grace_secs`
/// covering that 30 min holder↔observer skew. (Expiry compares an observer's
/// clock with the holder's timestamps, so only that relative skew matters.)
///
/// - While the holder heartbeats, and right up to its true deadline, no
///   observer ever finds a sweep candidate (no premature sweep), and a sweep
///   a fast observer forces anyway is a no-op in every replay.
/// - After the holder vanishes, observers sweep it once it is expired on
///   their own clocks; the sweeps are audited in the log, idempotent, and
///   every clone derives byte-identical tables.
/// - With the default 10 min grace, the same 30 min skew does sweep early:
///   grace must cover the fleet's skew (`SPEC.md` §21).
#[test]
fn chaos_clock_skew_plus_minus_30min_no_premature_sweep() {
    let grace = SKEW as u64;
    let config = ReplayConfig {
        default_ttl_secs: TTL,
        skew_grace_secs: grace,
    };
    let remote = TempDir::bare();
    let holder = Machine::new(&remote, "mc-holder", 0, config);
    let fast = Machine::new(&remote, "mc-observer-fast", SKEW, config);
    let slow = Machine::new(&remote, "mc-observer-slow", -SKEW, config);
    let mut alloc = TaskIdAllocator::new();
    let task = alloc.allocate(1, 1);
    let path = "src/auth/**";
    let t0: i64 = 1_800_000_000;

    // The holder acquires, then heartbeats every 5 min for an hour.
    holder.sync(&[holder.event(EventType::Acquire, path, &task, t0)]);
    let mut last_beat = t0;
    for beat in 1..=12 {
        let real = t0 + beat * 300;
        holder.sync(&[holder.event(EventType::Heartbeat, path, &task, real)]);
        last_beat = real;
        // Each observer checks just before the next beat is due.
        for observer in [&fast, &slow] {
            let seen = observer.sync(&[]);
            assert_eq!(seen.replay.leases[path].task, task);
            let sweeps = observer.sweeps(&seen.replay, real + 299, grace);
            assert!(
                sweeps.is_empty(),
                "{} swept a live lease: {sweeps:?}",
                observer.name
            );
        }
    }

    // The holder vanishes after its last heartbeat. Up to the true deadline
    // (last beat + ttl, real time) no observer may sweep.
    let deadline = last_beat + TTL as i64;
    for real in [last_beat + 1, last_beat + TTL as i64 / 2, deadline] {
        for observer in [&fast, &slow] {
            let seen = observer.sync(&[]);
            assert!(
                observer.sweeps(&seen.replay, real, grace).is_empty(),
                "{} swept prematurely at real {}s past the last beat",
                observer.name,
                real - last_beat
            );
        }
    }

    // A misbehaving fast observer sweeps at the true deadline anyway: replay
    // re-checks expiry at the sweep's timestamp, so the lease survives, and a
    // rival ACQUIRE at that moment is rejected (MT-001) on every clone.
    let rival = alloc.allocate(1, 2);
    let forced = fast.sync(&[
        fast.event(EventType::ExpireSweep, path, &task, deadline),
        fast.event(EventType::Acquire, path, &rival, deadline),
    ]);
    assert_eq!(
        forced.replay.leases[path].task, task,
        "forced early sweep must be a no-op"
    );
    assert_eq!(forced.replay.rejected.len(), 1);
    assert_eq!(forced.replay.rejected[0].code, ERROR_CODE_CLAIM_CONFLICT);
    assert_eq!(forced.replay.rejected[0].holder, task);

    // Past every clock's expiry, both observers sweep it (each from its own
    // replay, so the sweeps race); the lease is gone everywhere.
    let late = last_beat + TTL as i64 + 2 * grace as i64 + 1;
    let fast_view = fast.sync(&[]);
    let slow_view = slow.sync(&[]);
    let fast_sweeps = fast.sweeps(&fast_view.replay, late, grace);
    let slow_sweeps = slow.sweeps(&slow_view.replay, late, grace);
    assert_eq!(fast_sweeps.len(), 1);
    assert_eq!(slow_sweeps.len(), 1);
    std::thread::scope(|s| {
        let a = s.spawn(|| fast.sync(&fast_sweeps));
        let b = s.spawn(|| slow.sync(&slow_sweeps));
        (a.join().unwrap(), b.join().unwrap())
    });
    let finals: Vec<SyncOutcome> = [&holder, &fast, &slow]
        .iter()
        .map(|m| m.sync(&[]))
        .collect();
    for out in &finals {
        assert_eq!(out.oid, finals[0].oid);
        assert_eq!(replay_bytes(&out.replay), replay_bytes(&finals[0].replay));
        assert!(
            !out.replay.leases.contains_key(path),
            "expired lease was swept"
        );
    }
    // Audit: the forced sweep and both real sweeps stay in the log.
    assert_eq!(
        log_kinds(&holder.store, &finals[0].oid, EventType::ExpireSweep),
        3
    );

    // Idempotent: another sweep for the same lease changes nothing but the
    // audit log.
    let again = slow.sync(&[slow.event(EventType::ExpireSweep, path, &task, late + 60)]);
    assert_eq!(again.replay.leases, finals[0].replay.leases);
    assert_eq!(again.replay.rejected, finals[0].replay.rejected);
    assert_eq!(
        log_kinds(&slow.store, &again.oid, EventType::ExpireSweep),
        4
    );

    // The bound is real: with the default 10 min grace, the observer 30 min
    // fast sees the live lease as sweepable halfway through its TTL.
    let default_grace = 600;
    let narrow = Replay {
        leases: forced.replay.leases.clone(),
        rejected: Vec::new(),
    };
    let mid_ttl = last_beat + TTL as i64 / 2;
    let early = fast.sweeps(&narrow, mid_ttl, default_grace);
    assert_eq!(
        early.len(),
        1,
        "skew beyond grace sweeps early; grace must cover fleet skew"
    );
}

/// A barrier that panics instead of hanging when a party never arrives (a
/// clone that failed before its first push must fail the test, not wedge CI).
struct Gate {
    parties: usize,
    arrived: Mutex<usize>,
    all_in: Condvar,
}

impl Gate {
    fn new(parties: usize) -> Self {
        Self {
            parties,
            arrived: Mutex::new(0),
            all_in: Condvar::new(),
        }
    }

    fn wait(&self) {
        let mut arrived = self.arrived.lock().unwrap();
        *arrived += 1;
        if *arrived >= self.parties {
            self.all_in.notify_all();
            return;
        }
        let (arrived, timeout) = self
            .all_in
            .wait_timeout_while(arrived, Duration::from_secs(60), |n| *n < self.parties)
            .unwrap();
        assert!(
            !timeout.timed_out(),
            "only {} of {} parties reached the gate",
            *arrived,
            self.parties
        );
    }
}

/// `CliGitOps` whose first push waits at a gate shared by all clones, so
/// every first push expects the same (absent) remote tip.
struct Gated {
    inner: CliGitOps,
    gate: Arc<Gate>,
    first_push: AtomicBool,
}

impl GitOps for Gated {
    fn worktree_add(&self, path: &Path, branch: &str, base: &str) -> Result<(), GitError> {
        self.inner.worktree_add(path, branch, base)
    }
    fn worktree_remove(&self, path: &Path) -> Result<(), GitError> {
        self.inner.worktree_remove(path)
    }
    fn worktree_list(&self) -> Result<Vec<WorktreeInfo>, GitError> {
        self.inner.worktree_list()
    }
    fn rev_parse(&self, rev: &str) -> Result<String, GitError> {
        self.inner.rev_parse(rev)
    }
    fn ref_read(&self, reference: &str) -> Result<Option<String>, GitError> {
        self.inner.ref_read(reference)
    }
    fn update_ref(&self, r: &str, new: &str, old: Option<&str>) -> Result<(), GitError> {
        self.inner.update_ref(r, new, old)
    }
    fn remote_url(&self, name: &str) -> Result<Option<String>, GitError> {
        self.inner.remote_url(name)
    }
    fn hash_object(&self, contents: &[u8]) -> Result<String, GitError> {
        self.inner.hash_object(contents)
    }
    fn hash_objects(&self, contents: &[&[u8]]) -> Result<Vec<String>, GitError> {
        self.inner.hash_objects(contents)
    }
    fn mktree(&self, entries: &[TreeEntry]) -> Result<String, GitError> {
        self.inner.mktree(entries)
    }
    fn commit_tree(&self, tree: &str, parents: &[&str], msg: &str) -> Result<String, GitError> {
        self.inner.commit_tree(tree, parents, msg)
    }
    fn ls_tree(&self, treeish: &str) -> Result<Vec<TreeEntry>, GitError> {
        self.inner.ls_tree(treeish)
    }
    fn cat_blob(&self, oid: &str) -> Result<Vec<u8>, GitError> {
        self.inner.cat_blob(oid)
    }
    fn cat_blobs(&self, oids: &[String]) -> Result<Vec<Vec<u8>>, GitError> {
        self.inner.cat_blobs(oids)
    }
    fn fetch_ref(&self, remote: &str, src: &str, dst: &str) -> Result<Option<String>, GitError> {
        self.inner.fetch_ref(remote, src, dst)
    }
    fn push_ref_cas(
        &self,
        remote: &str,
        reference: &str,
        new_oid: &str,
        expected_old: Option<&str>,
    ) -> Result<(), GitError> {
        if self.first_push.swap(false, Ordering::SeqCst) {
            self.gate.wait();
        }
        self.inner
            .push_ref_cas(remote, reference, new_oid, expected_old)
    }
}

/// Syncs, retrying a whole `MT-003` contention error the way the daemon's
/// flush loop does. Returns the outcome and how many syncs it took.
fn sync_retrying(store: &CoordStore<Gated>, events: &[LeaseEvent]) -> (SyncOutcome, u32) {
    for call in 1..=3 {
        match store.sync(events) {
            Ok(out) => {
                assert!(out.attempts >= 1 && out.attempts <= MAX_ATTEMPTS);
                return (out, call);
            }
            Err(err @ CoordError::Contention { .. }) => {
                assert_eq!(err.code(), Some("MT-003"));
            }
            Err(err) => panic!("unexpected sync failure: {err}"),
        }
    }
    panic!("four writers did not converge within 3 bounded syncs");
}

/// Four clones race their first push against the same absent remote tip
/// (all four ACQUIRE a contested path plus a private one), then race again
/// with heartbeats and releases. Every sync converges within the bounded
/// retry, the contested path goes to the earliest ordering key no matter
/// whose push landed first, and all clones end on one commit with
/// byte-identical lease tables.
#[test]
fn chaos_four_clone_cas_contention_converges() {
    let config = ReplayConfig {
        default_ttl_secs: TTL,
        skew_grace_secs: 600,
    };
    let remote = TempDir::bare();
    let gate = Arc::new(Gate::new(4));
    let names = ["mc-c0", "mc-c1", "mc-c2", "mc-c3"];
    let dirs: Vec<TempDir> = (0..4).map(|_| TempDir::repo()).collect();
    let stores: Vec<CoordStore<Gated>> = dirs
        .iter()
        .map(|dir| {
            git(dir.path(), &["remote", "add", "origin", &remote.file_url()]);
            let gated = Gated {
                inner: CliGitOps::new(dir.path()),
                gate: Arc::clone(&gate),
                first_push: AtomicBool::new(true),
            };
            CoordStore::new(gated, Some("origin".into()), config)
        })
        .collect();
    let mut alloc = TaskIdAllocator::new();
    let tasks: Vec<TaskId> = (0..4).map(|i| alloc.allocate(1, i + 1)).collect();
    let t0 = 1_800_000_000_000u64;
    let at = |ms: u64| Timestamp::from_unix_ms(t0 + ms);
    let contested = "src/shared/**";

    // Round 1: clone i acquires at t0 + i ms, so clone 0 holds the earliest key.
    let round1: Vec<Vec<LeaseEvent>> = (0..4)
        .map(|i| {
            let own = format!("own/{i}/**");
            vec![
                event(
                    EventType::Acquire,
                    contested,
                    &tasks[i],
                    names[i],
                    at(i as u64),
                ),
                event(EventType::Acquire, &own, &tasks[i], names[i], at(i as u64)),
            ]
        })
        .collect();
    let results: Vec<(SyncOutcome, u32)> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..4)
            .map(|i| {
                let (store, events) = (&stores[i], &round1[i]);
                s.spawn(move || sync_retrying(store, events))
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let rounds: u32 = results
        .iter()
        .map(|(out, calls)| out.attempts + (calls - 1) * MAX_ATTEMPTS)
        .sum();
    assert!(
        rounds >= 4 + 3,
        "all four first pushes expected an absent ref, so at least three lost: {rounds} rounds"
    );

    // Round 2: everyone races again: losers release their lost claim on the
    // contested path, everyone heartbeats its private path.
    let gate2 = Gate::new(4);
    std::thread::scope(|s| {
        for i in 0..4 {
            let (store, gate2, task, name) = (&stores[i], &gate2, &tasks[i], names[i]);
            s.spawn(move || {
                let own = format!("own/{i}/**");
                let mut events = vec![event(EventType::Heartbeat, &own, task, name, at(60_000))];
                if i != 0 {
                    events.push(event(EventType::Release, contested, task, name, at(60_000)));
                }
                gate2.wait();
                sync_retrying(store, &events)
            });
        }
    });

    // Convergence: every clone and the remote on one commit, same tables.
    let finals: Vec<SyncOutcome> = stores.iter().map(|s| sync_retrying(s, &[]).0).collect();
    for out in &finals {
        assert_eq!(out.oid, finals[0].oid);
        assert_eq!(replay_bytes(&out.replay), replay_bytes(&finals[0].replay));
    }
    let remote_tip = git(remote.path(), &["rev-parse", COORD_REF]);
    assert_eq!(remote_tip.trim(), finals[0].oid);
    let replay = &finals[0].replay;
    assert_eq!(
        replay.leases[contested].task, tasks[0],
        "earliest ordering key wins"
    );
    for (i, task) in tasks.iter().enumerate() {
        assert_eq!(replay.leases[&format!("own/{i}/**")].task, *task);
    }
    assert_eq!(replay.leases.len(), 5);
    let mut losers: Vec<&TaskId> = replay.rejected.iter().map(|r| &r.task).collect();
    losers.sort();
    let mut expected: Vec<&TaskId> = tasks[1..].iter().collect();
    expected.sort();
    assert_eq!(
        losers, expected,
        "each later ACQUIRE of the contested path is rejected once"
    );
    assert!(replay
        .rejected
        .iter()
        .all(|r| r.holder == tasks[0] && r.code == ERROR_CODE_CLAIM_CONFLICT));
    assert!(conflicts_raw(contested, "src/shared/x.rs"));
}
