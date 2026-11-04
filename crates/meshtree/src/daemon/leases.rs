//! The daemon's in-memory lease state (`SPEC.md` §9 "Daemon claim path",
//! `ADR-0009` Part 2; compaction `ADR-0010`).
//!
//! `claim_paths` / `release_paths` are decided here, against the replay of
//! the coordination ref as of the local tip (its compaction snapshot plus the
//! `log/` events above it) plus the events this daemon has acknowledged but
//! not yet committed (`pending`). The background writer in [`super`] commits
//! `pending` and reports the new state back ([`Leases::flushed`]).
//!
//! Other writers (a CLI `spawn --claim`, the daemon's own heartbeats and
//! merge-gate releases, other processes) update the ref directly. Before
//! every decision the local ref's tip is compared with the one the state was
//! read at, by reading the ref file (no git process); the state is reloaded
//! only when it moved.
//!
//! The replay of the committed state is cached (`base`) and new events are
//! folded onto a copy with [`apply`], the same step replay performs per
//! event. That is exact whenever the new events sort after everything
//! committed, which holds for events this daemon creates now; otherwise the
//! full replay is used.
//!
//! Late events (`ADR-0010`): a pending event that a flush finds below the
//! snapshot watermark has been dropped from the union; [`Leases::flushed`]
//! hands it back so the daemon re-issues it as a fresh event.

use crate::exit::{CliError, PLATFORM};
use meshtree_core::lease::{
    apply, replay_from, sweep_candidates, EventType, Lease, LeaseEvent, Replay, ReplayConfig,
    Snapshot, Watermark,
};
use meshtree_core::pattern::conflicts_raw;
use meshtree_core::Timestamp;
use meshtree_core::{Rejection, TaskId};
use meshtree_platform::coord::{CoordStore, COORD_REF};
use meshtree_platform::gitops::{CliGitOps, GitOps};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

pub struct Leases {
    repo_root: PathBuf,
    config: ReplayConfig,
    /// The compaction snapshot as of `tip`.
    snapshot: Option<Snapshot>,
    /// The `log/` events (above the snapshot) as of `tip`.
    log: Vec<LeaseEvent>,
    ids: HashSet<String>,
    /// `replay_from(snapshot, log)`.
    base: Replay,
    /// The newest committed key: the last log event, else the watermark.
    last: Option<Watermark>,
    /// Local coordination ref the state was read at (`None`: not read yet;
    /// `Some(None)`: the ref did not exist).
    tip: Option<Option<String>>,
    /// Acknowledged events not yet in the ref, in acknowledgement order.
    pending: Vec<LeaseEvent>,
    /// When the oldest pending event was acknowledged.
    since: Option<Instant>,
    /// This daemon's writer is committing: the ref may move before
    /// [`Leases::flushed`] delivers the new state, so [`Leases::refresh`]
    /// waits for it instead of reloading everything itself.
    flushing: bool,
}

impl Leases {
    pub fn new(repo_root: &Path, config: ReplayConfig) -> Self {
        Self {
            repo_root: repo_root.to_path_buf(),
            config,
            snapshot: None,
            log: Vec::new(),
            ids: HashSet::new(),
            base: Replay::default(),
            last: None,
            tip: None,
            pending: Vec::new(),
            since: None,
            flushing: false,
        }
    }

    /// Reloads the state if the local ref moved since it was read.
    pub fn refresh(&mut self, store: &CoordStore<CliGitOps>) -> Result<(), CliError> {
        if self.flushing {
            // The writer's flush unions everything in the ref and hands the
            // result to `flushed` shortly; until then the cached state plus
            // `pending` is what this daemon committed (§9 revocation covers a
            // concurrent foreign writer).
            return Ok(());
        }
        let tip = current_tip(&self.repo_root, store.git())?;
        if self.tip.as_ref() == Some(&tip) {
            return Ok(());
        }
        let (snapshot, log) = match &tip {
            Some(oid) => store
                .read_state(oid)
                .map_err(|err| CliError::new(PLATFORM, err.to_string()))?,
            None => (None, Vec::new()),
        };
        self.set_state(tip, snapshot, log);
        Ok(())
    }

    /// Replaces the committed state. When only later log events were added
    /// (same snapshot), the cached replay is extended instead of recomputed.
    fn set_state(&mut self, tip: Option<String>, snapshot: Option<Snapshot>, log: Vec<LeaseEvent>) {
        let same_snapshot =
            snapshot.as_ref().map(|s| &s.watermark) == self.snapshot.as_ref().map(|s| &s.watermark);
        let mut added: Vec<&LeaseEvent> =
            log.iter().filter(|e| !self.ids.contains(&e.id)).collect();
        added.sort_by(|a, b| a.total_cmp(b));
        let extends = same_snapshot
            && log.len() == self.ids.len() + added.len()
            && match (&self.last, added.first()) {
                (Some(last), Some(first)) => !last.covers(first),
                _ => true,
            };
        if extends {
            for ev in &added {
                apply(&mut self.base, ev, &self.config, &conflicts_raw);
            }
            if let Some(newest) = added.last() {
                self.last = Some(Watermark::of(newest));
            }
        } else {
            self.base = replay_from(snapshot.as_ref(), &log, &self.config, conflicts_raw);
            self.last = log
                .iter()
                .max_by(|a, b| a.total_cmp(b))
                .map(Watermark::of)
                .or_else(|| snapshot.as_ref().map(|s| s.watermark.clone()));
        }
        self.ids = log.iter().map(|e| e.id.clone()).collect();
        self.log = log;
        self.snapshot = snapshot;
        self.tip = Some(tip);
        let ids = &self.ids;
        self.pending.retain(|e| !ids.contains(&e.id));
        if self.pending.is_empty() {
            self.since = None;
        }
    }

    /// `true` if the committed snapshot covers `event` (folded, or dropped
    /// if it arrives now).
    fn covered(&self, event: &LeaseEvent) -> bool {
        self.snapshot
            .as_ref()
            .is_some_and(|s| s.watermark.covers(event))
    }

    /// The rejections `candidates` would get if added now (empty: all
    /// granted). Same replay and conflict rule as the coordination store.
    pub fn decide(&self, candidates: &[LeaseEvent]) -> Vec<Rejection> {
        let ids: HashSet<&str> = candidates.iter().map(|e| e.id.as_str()).collect();
        let mut new: Vec<&LeaseEvent> = self
            .pending
            .iter()
            .chain(candidates)
            .filter(|e| !self.ids.contains(&e.id))
            .collect();
        new.sort_by(|a, b| a.total_cmp(b));
        let mut seen = HashSet::new();
        new.retain(|e| seen.insert(e.id.as_str()));
        let incremental = match (&self.last, new.first()) {
            (Some(last), Some(first)) => !last.covers(first),
            _ => true,
        };
        let state = if incremental {
            let mut state = self.base.clone();
            for ev in new {
                apply(&mut state, ev, &self.config, &conflicts_raw);
            }
            state
        } else {
            let mut all = self.log.clone();
            all.extend(self.pending.iter().cloned());
            all.extend(candidates.iter().cloned());
            replay_from(self.snapshot.as_ref(), &all, &self.config, conflicts_raw)
        };
        state
            .rejected
            .into_iter()
            .filter(|r| ids.contains(r.event.as_str()))
            .collect()
    }

    /// Records acknowledged events for the background writer.
    pub fn accept(&mut self, events: Vec<LeaseEvent>) {
        if events.is_empty() {
            return;
        }
        self.since.get_or_insert_with(Instant::now);
        self.pending.extend(events);
    }

    /// Events to commit next.
    pub fn pending(&self) -> Vec<LeaseEvent> {
        self.pending.clone()
    }

    /// How long the oldest pending event has waited (`None`: nothing pending).
    pub fn waiting(&self) -> Option<std::time::Duration> {
        self.since.map(|t| t.elapsed())
    }

    /// Leases due for an `EXPIRE_SWEEP` at `now` (this daemon's clock):
    /// expired by holder timestamps plus `skew_grace_secs`
    /// (`sweep_candidates`, §9) in the committed state plus pending events,
    /// without those this daemon already has a pending sweep for (at most
    /// one per lease).
    pub fn sweeps_due(&self, now: Timestamp) -> Vec<Lease> {
        let mut state = self.base.clone();
        let mut pending: Vec<&LeaseEvent> = self.pending.iter().collect();
        pending.sort_by(|a, b| a.total_cmp(b));
        for ev in pending {
            apply(&mut state, ev, &self.config, &conflicts_raw);
        }
        sweep_candidates(&state.leases, now, self.config.skew_grace_secs)
            .into_iter()
            .filter(|lease| {
                !self.pending.iter().any(|p| {
                    p.kind == EventType::ExpireSweep && p.path == lease.path && p.task == lease.task
                })
            })
            .cloned()
            .collect()
    }

    /// The committed snapshot's watermark.
    pub fn watermark(&self) -> Option<&Watermark> {
        self.snapshot.as_ref().map(|s| &s.watermark)
    }

    /// Moves new events this daemon creates above the watermark when this
    /// machine's clock trails it (another machine's clock runs ahead):
    /// replay ignores events at or below `W`, so a claim stamped there would
    /// be dropped instead of decided.
    pub fn lift(&self, events: &mut [LeaseEvent]) {
        if let Some(w) = self.watermark() {
            for e in events.iter_mut().filter(|e| w.covers(e)) {
                e.ts = Timestamp::from_unix_ms(w.ts.unix_ms().saturating_add(1));
            }
        }
    }

    /// A commit of the ref to `tip` holding `snapshot` and `log` succeeded;
    /// `dropped` are ids of committed events the union dropped below the
    /// watermark. Returns those pending events (removed from `pending`) for
    /// re-issue.
    pub fn flushed(
        &mut self,
        tip: String,
        snapshot: Option<Snapshot>,
        log: Vec<LeaseEvent>,
        dropped: &[String],
    ) -> Vec<LeaseEvent> {
        self.flushing = false;
        let (late, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut self.pending)
            .into_iter()
            .partition(|e| dropped.contains(&e.id));
        self.pending = keep;
        self.set_state(Some(tip), snapshot, log);
        late
    }

    /// The writer starts (`true`) or abandons (`false`) a commit.
    pub fn set_flushing(&mut self, flushing: bool) {
        self.flushing = flushing;
    }

    /// Crash recovery (§10), after [`Leases::refresh`]. `journaled` are the
    /// WAL's lease events in order; `intents` the claims each non-terminal
    /// task holds per the WAL. Journaled events missing from the ref are
    /// re-queued; those the snapshot covers are either folded (their effect
    /// is in the snapshot) or were dropped as late, which only the table can
    /// tell: for the latest such `ACQUIRE`/`RELEASE` per task and path whose
    /// effect is missing while the WAL still intends it, a fresh event is
    /// returned for re-issue. Returns `(re-queued, to re-issue)`.
    pub fn recover(
        &mut self,
        journaled: Vec<LeaseEvent>,
        intents: &BTreeMap<TaskId, BTreeSet<String>>,
    ) -> (usize, Vec<LeaseEvent>) {
        let missing: Vec<LeaseEvent> = journaled
            .into_iter()
            .filter(|e| !self.ids.contains(&e.id))
            .filter(|e| !self.pending.iter().any(|p| p.id == e.id))
            .collect();
        let (covered, fresh): (Vec<_>, Vec<_>) = missing.into_iter().partition(|e| self.covered(e));
        let mut latest: BTreeMap<(TaskId, String), LeaseEvent> = BTreeMap::new();
        for e in covered {
            if matches!(e.kind, EventType::Acquire | EventType::Release) {
                latest.insert((e.task.clone(), e.path.clone()), e);
            }
        }
        let reissue: Vec<LeaseEvent> = latest
            .into_values()
            .filter(|e| {
                let intended = intents.get(&e.task).is_some_and(|c| c.contains(&e.path));
                let held = self
                    .base
                    .leases
                    .get(&e.path)
                    .is_some_and(|l| l.task == e.task);
                let lost = self.base.rejected.iter().any(|r| r.event == e.id)
                    || self
                        .snapshot
                        .as_ref()
                        .is_some_and(|s| s.rejected.contains(&e.id));
                match e.kind {
                    EventType::Acquire => intended && !held && !lost,
                    _ => !intended && held,
                }
            })
            .collect();
        let n = fresh.len();
        self.accept(fresh);
        (n, reissue)
    }
}

/// The local coordination ref's tip, read from the ref files when the
/// repository has a plain `.git` directory (loose ref, then `packed-refs`),
/// else through git.
fn current_tip(repo_root: &Path, git: &CliGitOps) -> Result<Option<String>, CliError> {
    let git_dir = repo_root.join(".git");
    if git_dir.is_dir() {
        if let Ok(text) = fs::read_to_string(git_dir.join(COORD_REF)) {
            let oid = text.trim();
            if is_oid(oid) {
                return Ok(Some(oid.to_string()));
            }
        } else {
            match fs::read_to_string(git_dir.join("packed-refs")) {
                Ok(text) => {
                    let packed = text.lines().find_map(|line| {
                        let (oid, name) = line.split_once(' ')?;
                        (name == COORD_REF && is_oid(oid)).then(|| oid.to_string())
                    });
                    if packed.is_some() || !text.contains(COORD_REF) {
                        return Ok(packed);
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(_) => {}
            }
        }
    }
    git.ref_read(COORD_REF)
        .map_err(|err| CliError::new(PLATFORM, err.to_string()))
}

/// A full hex object id (SHA-1 or SHA-256).
fn is_oid(text: &str) -> bool {
    matches!(text.len(), 40 | 64) && text.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use meshtree_core::lease::{compact, replay};

    const CONFIG: ReplayConfig = ReplayConfig {
        default_ttl_secs: 900,
        skew_grace_secs: 600,
    };

    fn ev(id: &str, kind: EventType, path: &str, task: u128, ms: u64) -> LeaseEvent {
        LeaseEvent {
            id: id.to_string(),
            kind,
            path: path.to_string(),
            task: TaskId::from_ulid(task),
            machine: "mc".into(),
            ts: Timestamp::from_unix_ms(ms),
            counter: 0,
            ttl_secs: (kind == EventType::Acquire).then_some(900),
        }
    }

    fn leases() -> Leases {
        Leases::new(Path::new("."), CONFIG)
    }

    #[test]
    fn decisions_see_log_and_pending_and_flush_prunes() {
        let mut l = leases();
        l.set_state(
            Some("a".repeat(40)),
            None,
            vec![ev("01A", EventType::Acquire, "src/**", 1, 1_000)],
        );
        // Conflicts with the committed log.
        let r = l.decide(&[ev("01B", EventType::Acquire, "src/x.rs", 2, 2_000)]);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].event, "01B");
        // Granted, then pending: a second claim on it conflicts in memory.
        let docs = ev("01C", EventType::Acquire, "docs/**", 2, 3_000);
        assert!(l.decide(std::slice::from_ref(&docs)).is_empty());
        l.accept(vec![docs.clone()]);
        assert!(l.waiting().is_some());
        let r = l.decide(&[ev("01D", EventType::Acquire, "docs/a.md", 3, 4_000)]);
        assert_eq!(r.len(), 1, "pending events are part of the decision");
        // The flush lands it in the log: pending empties.
        let late = l.flushed(
            "b".repeat(40),
            None,
            vec![ev("01A", EventType::Acquire, "src/**", 1, 1_000), docs],
            &[],
        );
        assert!(late.is_empty());
        assert!(l.pending().is_empty() && l.waiting().is_none());
        // Recovery re-adds only events the ref lacks.
        let (n, reissue) = l.recover(
            vec![
                ev("01C", EventType::Acquire, "docs/**", 2, 3_000),
                ev("01E", EventType::Release, "docs/**", 2, 5_000),
            ],
            &BTreeMap::new(),
        );
        assert_eq!((n, reissue.len()), (1, 0));
        assert_eq!(l.pending()[0].id, "01E");
    }

    /// Deterministic pseudo-random event stream over overlapping paths,
    /// three tasks, all four kinds, and time gaps that cross expiry.
    fn stream(seed: u64, n: usize) -> Vec<LeaseEvent> {
        const PATHS: [&str; 6] = [
            "src/**",
            "src/a.rs",
            "src/b/**",
            "docs/**",
            "docs/x.md",
            "*",
        ];
        const KINDS: [EventType; 4] = [
            EventType::Acquire,
            EventType::Release,
            EventType::Heartbeat,
            EventType::ExpireSweep,
        ];
        let mut x = seed.max(1);
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let mut ms = 1_000_000;
        (0..n)
            .map(|i| {
                ms += [1, 1_000, 300_000, 1_600_000, 40_000_000][(next() % 5) as usize];
                let kind = KINDS[(next() % 4) as usize];
                let mut e = ev(
                    &format!("{i:08}"),
                    kind,
                    PATHS[(next() % PATHS.len() as u64) as usize],
                    1 + u128::from(next() % 3),
                    ms,
                );
                e.ttl_secs = (kind == EventType::Acquire).then_some(600 + next() % 1200);
                e
            })
            .collect()
    }

    #[test]
    fn cached_decisions_match_full_replay_with_and_without_snapshot() {
        for seed in 1..120 {
            let events = stream(seed, 60);
            let full = replay(&events, &CONFIG, conflicts_raw);
            let candidates = &events[40..];
            let expected: Vec<Rejection> = full
                .rejected
                .iter()
                .filter(|r| candidates.iter().any(|c| c.id == r.event))
                .cloned()
                .collect();
            // Through the cache: extending the log, then deciding.
            let mut l = leases();
            l.set_state(Some("a".repeat(40)), None, events[..20].to_vec());
            l.set_state(Some("b".repeat(40)), None, events[..40].to_vec());
            assert_eq!(l.decide(candidates), expected, "seed {seed}");
            // The same from a compacted prefix.
            if let Some((snap, rest)) = compact(None, &events[..40], &CONFIG, conflicts_raw, 0) {
                let mut c = leases();
                c.set_state(Some("c".repeat(40)), Some(snap), rest);
                assert_eq!(c.decide(candidates), expected, "seed {seed} compacted");
                assert_eq!(
                    c.base.leases,
                    replay(&events[..40], &CONFIG, conflicts_raw).leases
                );
            }
        }
    }

    #[test]
    fn out_of_order_events_fall_back_to_full_replay() {
        let mut l = leases();
        l.set_state(
            Some("a".repeat(40)),
            None,
            vec![ev("02", EventType::Acquire, "src/**", 1, 5_000)],
        );
        // An event older than the log (a skewed remote clock) is decided by
        // the full replay: it sorts first, so it wins and the logged acquire
        // is the one rejected. Folding it onto the cache would reject it.
        let early = ev("01", EventType::Acquire, "src/a.rs", 2, 1_000);
        let full = replay(
            &[
                ev("02", EventType::Acquire, "src/**", 1, 5_000),
                early.clone(),
            ],
            &CONFIG,
            conflicts_raw,
        );
        assert_eq!(full.rejected.len(), 1);
        assert_eq!(full.rejected[0].event, "02");
        assert!(l.decide(std::slice::from_ref(&early)).is_empty());
        // A log that is not an extension is replayed from scratch.
        l.set_state(Some("b".repeat(40)), None, vec![early.clone()]);
        assert_eq!(l.base, replay(&[early], &CONFIG, conflicts_raw));
    }

    #[test]
    fn late_and_folded_events_after_compaction() {
        // A long history; compaction folds its old part.
        let day = 24 * 60 * 60 * 1000;
        let mut history = Vec::new();
        for i in 0..10u64 {
            history.push(ev(
                &format!("H{i:02}A"),
                EventType::Acquire,
                &format!("h/{i}/**"),
                1,
                i * 10,
            ));
            history.push(ev(
                &format!("H{i:02}R"),
                EventType::Release,
                &format!("h/{i}/**"),
                1,
                i * 10 + 1,
            ));
        }
        // Task 2's claim, folded into the snapshot and still held.
        let kept = ev("K", EventType::Acquire, "kept/**", 2, 500);
        // Task 2's claim that never reached the ref (dropped as late).
        let late = ev("L", EventType::Acquire, "late/**", 2, 450);
        history.push(kept.clone());
        history.push(ev("NEWEST", EventType::Heartbeat, "kept/**", 2, 3 * day));
        let (snap, rest) = compact(None, &history, &CONFIG, conflicts_raw, 0).unwrap();
        assert!(snap.watermark.covers(&late), "the late claim is below W");

        let mut l = leases();
        l.set_state(Some("a".repeat(40)), Some(snap.clone()), rest.clone());
        // A pending event the flush reports as dropped comes back for re-issue.
        l.accept(vec![late.clone()]);
        let back = l.flushed(
            "b".repeat(40),
            Some(snap),
            rest,
            std::slice::from_ref(&late.id),
        );
        assert_eq!(back, vec![late.clone()]);
        assert!(l.pending().is_empty());

        // Recovery: the folded, still-held claim needs nothing; the late one
        // (missing from the table, still intended) is re-issued.
        let mut intents = BTreeMap::new();
        intents.insert(
            TaskId::from_ulid(2),
            BTreeSet::from(["kept/**".to_string(), "late/**".to_string()]),
        );
        let (requeued, reissue) = l.recover(vec![kept, late.clone()], &intents);
        assert_eq!(requeued, 0);
        assert_eq!(reissue, vec![late]);
        // No longer intended (task finished): nothing to re-issue.
        let (_, none) = l.recover(
            vec![ev("L", EventType::Acquire, "late/**", 2, 450)],
            &BTreeMap::new(),
        );
        assert!(none.is_empty());
    }

    #[test]
    fn sweeps_due_once_per_expired_lease() {
        let mut l = leases();
        // ttl 900 s + grace 600 s: expired after 1,500 s by holder time.
        l.set_state(
            Some("a".repeat(40)),
            None,
            vec![
                ev("01A", EventType::Acquire, "gone/**", 1, 1_000),
                ev("01B", EventType::Acquire, "live/**", 2, 1_000_000),
            ],
        );
        let at = |s: u64| Timestamp::from_unix_ms(1_000 + s * 1000);
        assert!(
            l.sweeps_due(at(1_500)).is_empty(),
            "never before ttl + grace"
        );
        let due = l.sweeps_due(at(1_501));
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].path, "gone/**");
        // A pending sweep suppresses another one for the same lease.
        let mut sweep = ev(
            "01C",
            EventType::ExpireSweep,
            "gone/**",
            1,
            at(1_501).unix_ms(),
        );
        sweep.ttl_secs = None;
        l.accept(vec![sweep]);
        assert!(l.sweeps_due(at(1_600)).is_empty());
        // A pending heartbeat from the holder keeps a lease alive.
        let mut m = leases();
        m.set_state(
            Some("b".repeat(40)),
            None,
            vec![ev("01A", EventType::Acquire, "gone/**", 1, 1_000)],
        );
        let mut beat = ev(
            "01H",
            EventType::Heartbeat,
            "gone/**",
            1,
            at(1_000).unix_ms(),
        );
        beat.ttl_secs = None;
        m.accept(vec![beat]);
        assert!(m.sweeps_due(at(1_600)).is_empty());
        assert_eq!(m.sweeps_due(at(2_501)).len(), 1);
    }

    #[test]
    fn oid_shape() {
        assert!(is_oid(&"0123456789abcdef".repeat(4)[..40]));
        assert!(is_oid(&"f".repeat(64)));
        assert!(!is_oid("ref: refs/heads/main"));
        assert!(!is_oid(&"g".repeat(40)));
    }
}
