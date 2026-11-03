//! The daemon's in-memory lease state (`SPEC.md` §9 "Daemon claim path",
//! `ADR-0009` Part 2).
//!
//! `claim_paths` / `release_paths` are decided here, against the replay of
//! the coordination log as of the local ref plus the events this daemon has
//! acknowledged but not yet committed (`pending`). The background writer in
//! [`super`] commits `pending` and reports the new log back
//! ([`Leases::flushed`]).
//!
//! Other writers (a CLI `spawn --claim`, the daemon's own heartbeats and
//! merge-gate releases, other processes) update the ref directly. Before
//! every decision the local ref's tip is compared with the one the log was
//! read at, by reading the ref file (no git process); the log is reloaded
//! only when it moved.
//!
//! A full replay scans every live lease for every `ACQUIRE`, so its cost
//! grows with the square of the log. The replay of the committed log is
//! therefore cached (`base`) and new events are folded onto it with
//! [`apply`], the same step [`replay`] performs per event. That is exact
//! whenever the new events sort after every logged event, which holds for
//! events this daemon creates now; otherwise the full replay is used.

use crate::exit::{CliError, PLATFORM};
use meshtree_core::lease::{
    replay, EventType, Lease, LeaseEvent, Replay, ReplayConfig, ERROR_CODE_CLAIM_CONFLICT,
};
use meshtree_core::pattern::conflicts_raw;
use meshtree_core::Rejection;
use meshtree_platform::coord::{CoordStore, COORD_REF};
use meshtree_platform::gitops::{CliGitOps, GitOps};
use std::cmp::Ordering;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

pub struct Leases {
    repo_root: PathBuf,
    config: ReplayConfig,
    /// The log as of `tip`.
    log: Vec<LeaseEvent>,
    ids: HashSet<String>,
    /// `replay(log)`.
    base: Replay,
    /// The log's last event in replay order.
    last: Option<LeaseEvent>,
    /// Local coordination ref the log was read at (`None`: not read yet;
    /// `Some(None)`: the ref did not exist).
    tip: Option<Option<String>>,
    /// Acknowledged events not yet in the ref, in acknowledgement order.
    pending: Vec<LeaseEvent>,
    /// When the oldest pending event was acknowledged.
    since: Option<Instant>,
    /// This daemon's writer is committing: the ref may move before
    /// [`Leases::flushed`] delivers the new log, so [`Leases::refresh`] waits
    /// for it instead of reloading the whole log itself.
    flushing: bool,
}

impl Leases {
    pub fn new(repo_root: &Path, config: ReplayConfig) -> Self {
        Self {
            repo_root: repo_root.to_path_buf(),
            config,
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

    /// Reloads the log if the local ref moved since it was read.
    pub fn refresh(&mut self, store: &CoordStore<CliGitOps>) -> Result<(), CliError> {
        if self.flushing {
            // The writer's flush unions every event in the ref and hands the
            // result to `flushed` shortly; until then the cached log plus
            // `pending` is what this daemon committed (§9 revocation covers a
            // concurrent foreign writer).
            return Ok(());
        }
        let tip = current_tip(&self.repo_root, store.git())?;
        if self.tip.as_ref() == Some(&tip) {
            return Ok(());
        }
        let log = match &tip {
            Some(oid) => {
                store
                    .read_log(oid)
                    .map_err(|err| CliError::new(PLATFORM, err.to_string()))?
                    .0
            }
            None => Vec::new(),
        };
        self.set_log(tip, log);
        Ok(())
    }

    /// Replaces the log. When the new log only adds events that sort after
    /// the old one, the cached replay is extended instead of recomputed.
    fn set_log(&mut self, tip: Option<String>, log: Vec<LeaseEvent>) {
        let mut added: Vec<&LeaseEvent> =
            log.iter().filter(|e| !self.ids.contains(&e.id)).collect();
        added.sort_by(|a, b| a.total_cmp(b));
        let extends = log.len() == self.ids.len() + added.len()
            && match (&self.last, added.first()) {
                (Some(last), Some(first)) => first.total_cmp(last) == Ordering::Greater,
                _ => true,
            };
        if extends {
            for ev in &added {
                apply(&mut self.base, ev, &self.config);
            }
            if let Some(newest) = added.last() {
                self.last = Some((*newest).clone());
            }
        } else {
            self.base = replay(&log, &self.config, conflicts_raw);
            self.last = log.iter().max_by(|a, b| a.total_cmp(b)).cloned();
        }
        self.ids = log.iter().map(|e| e.id.clone()).collect();
        self.log = log;
        self.tip = Some(tip);
        let ids = &self.ids;
        self.pending.retain(|e| !ids.contains(&e.id));
        if self.pending.is_empty() {
            self.since = None;
        }
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
            (Some(last), Some(first)) => first.total_cmp(last) == Ordering::Greater,
            _ => true,
        };
        let state = if incremental {
            let mut state = self.base.clone();
            for ev in new {
                apply(&mut state, ev, &self.config);
            }
            state
        } else {
            let mut all = self.log.clone();
            all.extend(self.pending.iter().cloned());
            all.extend(candidates.iter().cloned());
            replay(&all, &self.config, conflicts_raw)
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

    /// A commit of the ref to `tip` holding `log` succeeded.
    pub fn flushed(&mut self, tip: String, log: Vec<LeaseEvent>) {
        self.flushing = false;
        self.set_log(Some(tip), log);
    }

    /// The writer starts (`true`) or abandons (`false`) a commit.
    pub fn set_flushing(&mut self, flushing: bool) {
        self.flushing = flushing;
    }

    /// Re-adds journaled events missing from the ref (crash recovery,
    /// §10). Call after [`Leases::refresh`]. Returns how many were missing.
    pub fn recover(&mut self, journaled: Vec<LeaseEvent>) -> usize {
        let missing: Vec<LeaseEvent> = journaled
            .into_iter()
            .filter(|e| !self.ids.contains(&e.id))
            .filter(|e| !self.pending.iter().any(|p| p.id == e.id))
            .collect();
        let n = missing.len();
        self.accept(missing);
        n
    }
}

/// One step of [`replay`]: applies `ev` to `state`. Must stay identical to
/// the loop body of `meshtree_core::lease::replay` (checked by
/// `fold_matches_core_replay`).
fn apply(state: &mut Replay, ev: &LeaseEvent, config: &ReplayConfig) {
    let grace = config.skew_grace_secs;
    match ev.kind {
        EventType::Acquire => {
            let mut expired = Vec::new();
            let mut holder: Option<&Lease> = None;
            for lease in state.leases.values() {
                if lease.task == ev.task
                    || !(lease.path == ev.path || conflicts_raw(&lease.path, &ev.path))
                {
                    continue;
                }
                if lease.is_expired_at(ev.ts, grace) {
                    expired.push(lease.path.clone());
                } else if holder.is_none() {
                    holder = Some(lease);
                }
            }
            if let Some(lease) = holder {
                let rejection = Rejection {
                    code: ERROR_CODE_CLAIM_CONFLICT.to_string(),
                    event: ev.id.clone(),
                    path: ev.path.clone(),
                    task: ev.task.clone(),
                    machine: ev.machine.clone(),
                    holder: lease.task.clone(),
                    holder_path: lease.path.clone(),
                };
                state.rejected.push(rejection);
                return;
            }
            for path in expired {
                state.leases.remove(&path);
            }
            let ttl_secs = ev.ttl_secs.unwrap_or(config.default_ttl_secs);
            let acquired_at = match state.leases.get(&ev.path) {
                Some(own) if !own.is_expired_at(ev.ts, grace) => own.acquired_at,
                _ => ev.ts,
            };
            state.leases.insert(
                ev.path.clone(),
                Lease {
                    path: ev.path.clone(),
                    task: ev.task.clone(),
                    machine: ev.machine.clone(),
                    acquired_at,
                    ttl_secs,
                    last_heartbeat: ev.ts,
                },
            );
        }
        EventType::Release => {
            if state
                .leases
                .get(&ev.path)
                .is_some_and(|l| l.task == ev.task)
            {
                state.leases.remove(&ev.path);
            }
        }
        EventType::Heartbeat => {
            if let Some(lease) = state.leases.get_mut(&ev.path) {
                if lease.task == ev.task && !lease.is_expired_at(ev.ts, grace) {
                    lease.last_heartbeat = lease.last_heartbeat.max(ev.ts);
                }
            }
        }
        EventType::ExpireSweep => {
            if state
                .leases
                .get(&ev.path)
                .is_some_and(|l| l.task == ev.task && l.is_expired_at(ev.ts, grace))
            {
                state.leases.remove(&ev.path);
            }
        }
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
    use meshtree_core::{TaskId, Timestamp};

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
        l.set_log(
            Some("a".repeat(40)),
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
        l.flushed(
            "b".repeat(40),
            vec![ev("01A", EventType::Acquire, "src/**", 1, 1_000), docs],
        );
        assert!(l.pending().is_empty() && l.waiting().is_none());
        // Recovery re-adds only events the ref lacks.
        let n = l.recover(vec![
            ev("01C", EventType::Acquire, "docs/**", 2, 3_000),
            ev("01E", EventType::Release, "docs/**", 2, 5_000),
        ]);
        assert_eq!(n, 1);
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
                ms += [1, 1_000, 300_000, 1_600_000][(next() % 4) as usize];
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
    fn fold_matches_core_replay() {
        for seed in 1..200 {
            let events = stream(seed, 60);
            let full = replay(&events, &CONFIG, conflicts_raw);
            for split in [0, 1, 17, 30, 59, 60] {
                let mut state = replay(&events[..split], &CONFIG, conflicts_raw);
                for e in &events[split..] {
                    apply(&mut state, e, &CONFIG);
                }
                assert_eq!(state, full, "seed {seed}, split {split}");
            }
            // Through the cache: extending the log, then deciding.
            let mut l = leases();
            l.set_log(Some("a".repeat(40)), events[..20].to_vec());
            l.set_log(Some("b".repeat(40)), events[..40].to_vec());
            let candidates = &events[40..];
            let expected: Vec<Rejection> = full
                .rejected
                .iter()
                .filter(|r| candidates.iter().any(|c| c.id == r.event))
                .cloned()
                .collect();
            assert_eq!(l.decide(candidates), expected, "seed {seed}");
            assert_eq!(l.base, replay(&events[..40], &CONFIG, conflicts_raw));
        }
    }

    #[test]
    fn out_of_order_events_fall_back_to_full_replay() {
        let mut l = leases();
        l.set_log(
            Some("a".repeat(40)),
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
        l.set_log(Some("b".repeat(40)), vec![early.clone()]);
        assert_eq!(l.base, replay(&[early], &CONFIG, conflicts_raw));
    }

    #[test]
    fn oid_shape() {
        assert!(is_oid(&"0123456789abcdef".repeat(4)[..40]));
        assert!(is_oid(&"f".repeat(64)));
        assert!(!is_oid("ref: refs/heads/main"));
        assert!(!is_oid(&"g".repeat(40)));
    }
}
