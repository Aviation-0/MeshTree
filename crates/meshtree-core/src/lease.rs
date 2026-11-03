//! Lease event log model, ordering key, and deterministic replay.
//!
//! Authoritative specification: `SPEC.md` §7 (Lease), §9 (event log,
//! ordering key, deterministic replay), §16 (`MT-001`), §18.
//!
//! The event log is the source of truth; the lease table is derived by
//! [`replay`]. Replay is a pure function of the *set* of events: input order
//! and duplicates never change the output. Path-pattern overlap is supplied by
//! the caller (the §7 conflict rule lives in `pattern`), so replay itself has
//! no opinion on glob semantics.

pub mod coord;

use crate::task::TaskId;
use crate::time::Timestamp;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

/// Error code for a claim conflict per `SPEC.md` §16.
pub const ERROR_CODE_CLAIM_CONFLICT: &str = "MT-001";

/// Lease event type (`SPEC.md` §9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EventType {
    /// Claim a path pattern for a task.
    Acquire,
    /// Give up a held path pattern.
    Release,
    /// Refresh a held lease's `last_heartbeat`.
    Heartbeat,
    /// Remove a lease whose holder has expired; emitted by any machine, idempotent.
    ExpireSweep,
}

/// One append-only event in `refs/meshtree/coord:log/<ULID>.json` (`SPEC.md` §9).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseEvent {
    /// Event ULID (also the log filename).
    pub id: String,
    /// Event type.
    #[serde(rename = "type")]
    pub kind: EventType,
    /// Normalized path pattern.
    pub path: String,
    /// Holder task (for `EXPIRE_SWEEP`, the task being swept).
    pub task: TaskId,
    /// Emitting machine.
    pub machine: String,
    /// Emitter's wall-clock time.
    pub ts: Timestamp,
    /// Emitter's monotonic counter (tie-break within one millisecond).
    pub counter: u64,
    /// Lease TTL for `ACQUIRE`; falls back to the configured default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_secs: Option<u64>,
}

impl LeaseEvent {
    /// The §9 ordering key `(ts, counter, machine, id)`.
    pub fn ordering_key(&self) -> (Timestamp, u64, &str, &str) {
        (self.ts, self.counter, &self.machine, &self.id)
    }

    /// Total order: the §9 ordering key, then the remaining fields, so even
    /// malformed logs (two different events sharing one key) sort the same
    /// way regardless of input order.
    pub fn total_cmp(&self, other: &Self) -> Ordering {
        self.ordering_key()
            .cmp(&other.ordering_key())
            .then_with(|| self.kind.cmp(&other.kind))
            .then_with(|| self.path.cmp(&other.path))
            .then_with(|| self.task.cmp(&other.task))
            .then_with(|| self.ttl_secs.cmp(&other.ttl_secs))
    }
}

/// A held lease (`SPEC.md` §7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    /// Normalized path pattern.
    pub path: String,
    /// Holder task.
    pub task: TaskId,
    /// Holder machine.
    pub machine: String,
    /// Time of the winning `ACQUIRE`.
    pub acquired_at: Timestamp,
    /// Lease TTL.
    pub ttl_secs: u64,
    /// Holder's latest `ACQUIRE`/`HEARTBEAT` time.
    pub last_heartbeat: Timestamp,
}

impl Lease {
    /// `true` once `now` is past `last_heartbeat + ttl + grace`.
    ///
    /// Evaluated against the *holder's* timestamps, so a skewed observer clock
    /// only shifts the verdict by at most the skew, which `grace` absorbs (§9).
    pub fn is_expired_at(&self, now: Timestamp, skew_grace_secs: u64) -> bool {
        now > self.expires_at(skew_grace_secs)
    }

    /// Last instant (holder clock) at which the lease is still live:
    /// `last_heartbeat + ttl + grace`.
    pub fn expires_at(&self, skew_grace_secs: u64) -> Timestamp {
        let window_ms = self
            .ttl_secs
            .saturating_add(skew_grace_secs)
            .saturating_mul(1000);
        Timestamp::from_unix_ms(self.last_heartbeat.unix_ms().saturating_add(window_ms))
    }
}

/// Derived lease table, keyed by normalized path (deterministic order).
pub type LeaseTable = BTreeMap<String, Lease>;

/// Leases an observer may sweep at `now` (the observer's own clock): those
/// past `last_heartbeat + ttl + grace` by the holder's timestamps (`SPEC.md`
/// §9), in path order. Emitting an `EXPIRE_SWEEP` for each is always safe:
/// replay re-checks expiry at the sweep's own timestamp, so a sweep from a
/// clock running fast by up to `skew_grace_secs` is a no-op, and repeated
/// sweeps are idempotent.
pub fn sweep_candidates(table: &LeaseTable, now: Timestamp, skew_grace_secs: u64) -> Vec<&Lease> {
    table
        .values()
        .filter(|lease| lease.is_expired_at(now, skew_grace_secs))
        .collect()
}

/// An `ACQUIRE` that lost to a live conflicting lease (kept for audit, §9).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rejection {
    /// Error code (`MT-001`).
    pub code: String,
    /// Rejected event ID.
    pub event: String,
    /// Requested path pattern.
    pub path: String,
    /// Requesting task.
    pub task: TaskId,
    /// Requesting machine.
    pub machine: String,
    /// Task holding the conflicting lease.
    pub holder: TaskId,
    /// Path pattern of the conflicting lease.
    pub holder_path: String,
}

/// Replay parameters (from `[lease]` config, `SPEC.md` §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplayConfig {
    /// TTL for `ACQUIRE` events without `ttl_secs`.
    pub default_ttl_secs: u64,
    /// Cross-machine clock-skew allowance.
    pub skew_grace_secs: u64,
}

/// Output of [`replay`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Replay {
    /// Live and not-yet-swept leases.
    pub leases: LeaseTable,
    /// Rejected `ACQUIRE`s in replay order.
    pub rejected: Vec<Rejection>,
}

/// Derives the lease table from an event log (`SPEC.md` §9 deterministic replay).
///
/// 1. Union: events are de-duplicated by `id`.
/// 2. Sort by the ordering key (see [`LeaseEvent::total_cmp`]).
/// 3. Replay each event with [`apply`].
pub fn replay<F>(events: &[LeaseEvent], config: &ReplayConfig, conflicts: F) -> Replay
where
    F: Fn(&str, &str) -> bool,
{
    replay_from(None, events, config, conflicts)
}

/// [`replay`] starting from a compaction snapshot (`SPEC.md` §9, `ADR-0010`):
/// the state at the watermark `W`, then every event whose ordering key is
/// above `W`, in order. Events at or below `W` are already folded into the
/// snapshot and are ignored.
pub fn replay_from<F>(
    snapshot: Option<&Snapshot>,
    events: &[LeaseEvent],
    config: &ReplayConfig,
    conflicts: F,
) -> Replay
where
    F: Fn(&str, &str) -> bool,
{
    let mut out = snapshot.map(Snapshot::state).unwrap_or_default();
    for ev in ordered_above(snapshot.map(|s| &s.watermark), events) {
        apply(&mut out, ev, config, &conflicts);
    }
    out
}

/// `events` de-duplicated by id, sorted by [`LeaseEvent::total_cmp`], without
/// those a watermark covers.
fn ordered_above<'a>(
    watermark: Option<&Watermark>,
    events: &'a [LeaseEvent],
) -> Vec<&'a LeaseEvent> {
    let mut sorted: Vec<&LeaseEvent> = events
        .iter()
        .filter(|ev| !watermark.is_some_and(|w| w.covers(ev)))
        .collect();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let mut seen = BTreeSet::new();
    sorted.retain(|ev| seen.insert(ev.id.as_str()));
    sorted
}

/// Applies one event to the replay state (§9):
///
/// - `ACQUIRE` wins if no *other* task holds a live lease on a conflicting
///   path (`conflicts`, or identical path); expired conflicting leases are
///   dropped. A re-`ACQUIRE` by the holder refreshes the lease. A loss is
///   recorded as a [`Rejection`] (`MT-001`).
/// - `RELEASE` by the holder removes the lease; otherwise ignored.
/// - `HEARTBEAT` by the holder refreshes a live lease; a heartbeat after
///   expiry does not revive it (the holder must re-acquire, `MT-002`).
/// - `EXPIRE_SWEEP` removes the named holder's lease only if it is expired
///   at the sweep's timestamp; idempotent.
///
/// Replay is a fold of `apply` over the ordered events, so a caller holding
/// the state of a prefix may extend it with later events.
pub fn apply<F>(state: &mut Replay, ev: &LeaseEvent, config: &ReplayConfig, conflicts: &F)
where
    F: Fn(&str, &str) -> bool,
{
    let grace = config.skew_grace_secs;
    match ev.kind {
        EventType::Acquire => {
            let mut expired = Vec::new();
            let mut holder = None;
            for lease in state.leases.values() {
                if lease.task == ev.task
                    || !(lease.path == ev.path || conflicts(&lease.path, &ev.path))
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

/// The §9 ordering key `(ts, counter, machine, id)` of one event, owned: a
/// compaction watermark (`ADR-0010`). Field order makes the derived `Ord`
/// the ordering key's.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Watermark {
    pub ts: Timestamp,
    pub counter: u64,
    pub machine: String,
    pub id: String,
}

impl Watermark {
    /// The key of `event`.
    pub fn of(event: &LeaseEvent) -> Self {
        Self {
            ts: event.ts,
            counter: event.counter,
            machine: event.machine.clone(),
            id: event.id.clone(),
        }
    }

    /// `true` if `event`'s key is at or below this watermark (folded).
    pub fn covers(&self, event: &LeaseEvent) -> bool {
        event.ordering_key()
            <= (
                self.ts,
                self.counter,
                self.machine.as_str(),
                self.id.as_str(),
            )
    }
}

/// `snapshot/<W>.json` (`SPEC.md` §5, §9, `ADR-0010`; format in
/// `docs/protocol.md` §3.5): the lease table after folding every event up
/// to the watermark, so `replay_from(snapshot, events above W)` derives the
/// same table as `replay(all events)`, byte for byte. The table is kept
/// whole, in path order (expired entries nothing has removed yet included:
/// expiry is decided at the next event's timestamp). Rejections of folded
/// events are kept as ids only, for audit; replay reports full
/// [`Rejection`]s for the events above `W`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    /// Ordering key of the newest folded event.
    pub watermark: Watermark,
    /// The lease table at `W`, in path order.
    pub leases: Vec<Lease>,
    /// Ids of folded `ACQUIRE`s that were rejected, in replay order.
    pub rejected: Vec<String>,
}

impl Snapshot {
    /// The replay state it starts from: its table, no rejections.
    pub fn state(&self) -> Replay {
        Replay {
            leases: self
                .leases
                .iter()
                .map(|l| (l.path.clone(), l.clone()))
                .collect(),
            rejected: Vec::new(),
        }
    }
}

/// A compaction runs once more than this many events are foldable
/// (`ADR-0010`).
pub const COMPACT_THRESHOLD: usize = 1_000;

/// How far behind the log's newest event an event must be to be folded:
/// `max(24 h, 4 × (ttl_secs + 2 × skew_grace_secs))` (`ADR-0010`).
pub fn compact_horizon_secs(config: &ReplayConfig) -> u64 {
    let lease = config
        .default_ttl_secs
        .saturating_add(config.skew_grace_secs.saturating_mul(2));
    lease.saturating_mul(4).max(24 * 60 * 60)
}

/// Compaction (`SPEC.md` §9, `ADR-0010`): folds every event with
/// `ts ≤ newest.ts − horizon` (`newest` taken from the events themselves,
/// never a clock) onto `snapshot`, when more than `threshold` such events
/// exist. Returns the new snapshot (watermark = key of the newest folded
/// event) and the events above it, or `None` when there is not enough to
/// fold. Every machine holding the same events derives the same result, and
/// `replay_from(new, rest)` derives the same table as
/// `replay_from(snapshot, events)`; rejections of the folded events move into
/// the snapshot's `rejected` ids.
pub fn compact<F>(
    snapshot: Option<&Snapshot>,
    events: &[LeaseEvent],
    config: &ReplayConfig,
    conflicts: F,
    threshold: usize,
) -> Option<(Snapshot, Vec<LeaseEvent>)>
where
    F: Fn(&str, &str) -> bool,
{
    let ordered = ordered_above(snapshot.map(|s| &s.watermark), events);
    let newest = ordered.iter().map(|ev| ev.ts).max()?;
    let cutoff = newest
        .unix_ms()
        .checked_sub(compact_horizon_secs(config).saturating_mul(1000))?;
    // Sorted by key, whose first component is `ts`: the foldable events are
    // a prefix.
    let fold = ordered
        .iter()
        .take_while(|ev| ev.ts.unix_ms() <= cutoff)
        .count();
    if fold <= threshold {
        return None;
    }
    let mut state = snapshot.map(Snapshot::state).unwrap_or_default();
    for ev in &ordered[..fold] {
        apply(&mut state, ev, config, &conflicts);
    }
    let mut rejected = snapshot.map(|s| s.rejected.clone()).unwrap_or_default();
    rejected.extend(state.rejected.into_iter().map(|r| r.event));
    let compacted = Snapshot {
        watermark: Watermark::of(ordered[fold - 1]),
        leases: state.leases.into_values().collect(),
        rejected,
    };
    let rest = ordered[fold..].iter().map(|ev| (*ev).clone()).collect();
    Some((compacted, rest))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::TaskIdAllocator;

    const CONFIG: ReplayConfig = ReplayConfig {
        default_ttl_secs: 900,
        skew_grace_secs: 600,
    };

    fn exact(a: &str, b: &str) -> bool {
        a == b
    }

    fn tasks() -> (TaskId, TaskId) {
        let mut alloc = TaskIdAllocator::new();
        (alloc.allocate(1_000, 1), alloc.allocate(1_000, 2))
    }

    fn ev(
        id: &str,
        kind: EventType,
        path: &str,
        task: &TaskId,
        machine: &str,
        secs: u64,
    ) -> LeaseEvent {
        LeaseEvent {
            id: id.to_string(),
            kind,
            path: path.to_string(),
            task: task.clone(),
            machine: machine.to_string(),
            ts: Timestamp::from_unix_ms(secs * 1000),
            counter: 0,
            ttl_secs: None,
        }
    }

    #[test]
    fn sweep_candidates_are_expired_leases_in_path_order() {
        use EventType::*;
        let (a, b) = tasks();
        let log = vec![
            ev("01", Acquire, "z/**", &a, "mc-a", 1_000),
            ev("02", Acquire, "m/**", &b, "mc-b", 1_000),
            ev("03", Acquire, "a/**", &a, "mc-a", 1_000),
            ev("04", Heartbeat, "m/**", &b, "mc-b", 2_000),
        ];
        let table = replay(&log, &CONFIG, exact).leases;
        let deadline = 1_000 + CONFIG.default_ttl_secs + CONFIG.skew_grace_secs;
        let at = |secs: u64| Timestamp::from_unix_ms(secs * 1000);
        assert!(sweep_candidates(&table, at(deadline), CONFIG.skew_grace_secs).is_empty());
        let paths: Vec<&str> = sweep_candidates(&table, at(deadline + 1), CONFIG.skew_grace_secs)
            .iter()
            .map(|l| l.path.as_str())
            .collect();
        assert_eq!(paths, ["a/**", "z/**"], "heartbeaten m/** is still live");
    }

    /// Holder and observer clocks are offset from real time by `dh` / `dobs`
    /// seconds. Within a relative skew of `skew_grace_secs`, no observer ever
    /// sees a lease expire before its true TTL deadline (§9 holder-timestamp
    /// expiry + grace), and every observer agrees it has expired by
    /// `ttl + 2 × grace` after the real heartbeat.
    #[test]
    fn lease_ttl_expiry_with_injected_clock_skew() {
        use EventType::*;
        let (a, b) = tasks();
        let ttl = CONFIG.default_ttl_secs as i64;
        let grace = CONFIG.skew_grace_secs as i64;
        let real_hb = 100_000i64;
        let stamp = |real: i64, skew: i64| Timestamp::from_unix_ms(((real + skew) * 1000) as u64);
        let skews = [-grace / 2, -60, -1, 0, 1, 60, grace / 2];

        for &dh in &skews {
            for &dobs in &skews {
                let lease = Lease {
                    path: "p".to_string(),
                    task: a.clone(),
                    machine: "mc-holder".to_string(),
                    acquired_at: stamp(real_hb, dh),
                    ttl_secs: ttl as u64,
                    last_heartbeat: stamp(real_hb, dh),
                };
                for real_now in [real_hb, real_hb + ttl / 2, real_hb + ttl] {
                    assert!(
                        !lease.is_expired_at(stamp(real_now, dobs), grace as u64),
                        "premature expiry: dh={dh} dobs={dobs} real_now={real_now}"
                    );
                }
                assert!(
                    lease.is_expired_at(stamp(real_hb + ttl + 2 * grace + 1, dobs), grace as u64),
                    "never expired: dh={dh} dobs={dobs}"
                );

                // Replay: a skewed observer's sweep and rival ACQUIRE at the
                // true deadline are a no-op and a rejection.
                let log = vec![
                    LeaseEvent {
                        ts: stamp(real_hb, dh),
                        ..ev("01", Acquire, "p", &a, "mc-holder", 0)
                    },
                    LeaseEvent {
                        ts: stamp(real_hb + ttl, dobs),
                        ..ev("02", ExpireSweep, "p", &a, "mc-observer", 0)
                    },
                    LeaseEvent {
                        ts: stamp(real_hb + ttl, dobs),
                        ..ev("03", Acquire, "p", &b, "mc-observer", 0)
                    },
                ];
                let out = replay(&log, &CONFIG, exact);
                assert_eq!(out.leases["p"].task, a, "dh={dh} dobs={dobs}");
                assert_eq!(out.rejected.len(), 1, "dh={dh} dobs={dobs}");
                assert_eq!(
                    out.leases["p"].expires_at(grace as u64),
                    Timestamp::from_unix_ms((((real_hb + dh) + ttl + grace) * 1000) as u64)
                );
            }
        }

        // The bound is real: relative skew beyond the grace window (holder
        // slow by `grace`, observer fast by `grace`) can expire a lease at its
        // true deadline. Larger grace is the documented mitigation (§21).
        let lease = Lease {
            path: "p".to_string(),
            task: a,
            machine: "mc-holder".to_string(),
            acquired_at: stamp(real_hb, -grace),
            ttl_secs: ttl as u64,
            last_heartbeat: stamp(real_hb, -grace),
        };
        assert!(lease.is_expired_at(stamp(real_hb + ttl + 1, grace), grace as u64));
    }

    #[test]
    fn lease_event_serde_and_ordering_key() {
        // SPEC §9 example (task ID made canonical).
        let json = r#"{
          "id": "01JAY7K2N8AAAAAAAAAAAAAAAA",
          "type": "ACQUIRE",
          "path": "src/auth/**",
          "task": "t-01JAXGZK7Q4D3RT2WFH8MB3VQY",
          "machine": "mc-lucas-mac",
          "ts": "2026-11-03T14:05:00.500Z",
          "counter": 4821,
          "ttl_secs": 900
        }"#;
        let event: LeaseEvent = serde_json::from_str(json).expect("SPEC §9 event parses");
        assert_eq!(event.kind, EventType::Acquire);
        assert_eq!(event.ts.unix_ms(), 1_793_714_700_500);
        assert_eq!(event.ttl_secs, Some(900));
        let back = serde_json::to_string(&event).unwrap();
        assert_eq!(serde_json::from_str::<LeaseEvent>(&back).unwrap(), event);
        let value: serde_json::Value = serde_json::from_str(&back).unwrap();
        assert_eq!(value["type"], "ACQUIRE");
        assert_eq!(value["ts"], "2026-11-03T14:05:00.500Z");

        for (kind, wire) in [
            (EventType::Acquire, "\"ACQUIRE\""),
            (EventType::Release, "\"RELEASE\""),
            (EventType::Heartbeat, "\"HEARTBEAT\""),
            (EventType::ExpireSweep, "\"EXPIRE_SWEEP\""),
        ] {
            assert_eq!(serde_json::to_string(&kind).unwrap(), wire);
        }
        // ttl_secs is omitted when absent; unknown types and bad ts are rejected.
        let mut no_ttl = event.clone();
        no_ttl.ttl_secs = None;
        assert!(!serde_json::to_string(&no_ttl).unwrap().contains("ttl_secs"));
        assert!(serde_json::from_str::<LeaseEvent>(&json.replace("ACQUIRE", "GRAB")).is_err());
        assert!(serde_json::from_str::<LeaseEvent>(&json.replace(".500Z", ".500")).is_err());

        // Ordering key: ts, then counter, then machine, then id.
        let (a, _) = tasks();
        let base = ev("01B", EventType::Acquire, "p", &a, "mc-b", 10);
        let mut earlier = base.clone();
        earlier.ts = Timestamp::from_unix_ms(9_999);
        let mut lower_counter = base.clone();
        lower_counter.counter = 0;
        let mut higher_counter = base.clone();
        higher_counter.counter = 1;
        let mut lower_machine = higher_counter.clone();
        lower_machine.machine = "mc-a".to_string();
        let mut lower_id = higher_counter.clone();
        lower_id.id = "01A".to_string();
        assert!(earlier.ordering_key() < base.ordering_key());
        assert!(lower_counter.ordering_key() < higher_counter.ordering_key());
        assert!(lower_machine.ordering_key() < higher_counter.ordering_key());
        assert!(lower_id.ordering_key() < higher_counter.ordering_key());
        assert_eq!(base.total_cmp(&base), Ordering::Equal);
    }

    #[test]
    fn replay_handcrafted_event_log() {
        use EventType::*;
        let (a, b) = tasks();
        let log = vec![
            ev("01", Acquire, "src/auth/**", &a, "mc-mac", 100),
            // B loses: A holds a live lease.
            ev("02", Acquire, "src/auth/**", &b, "mc-win", 101),
            // A's heartbeat extends; a non-holder RELEASE and early sweep are no-ops.
            ev("03", Heartbeat, "src/auth/**", &a, "mc-mac", 400),
            ev("04", Release, "src/auth/**", &b, "mc-win", 401),
            ev("05", ExpireSweep, "src/auth/**", &a, "mc-win", 402),
            // B takes an unrelated path.
            ev("06", Acquire, "docs/**", &b, "mc-win", 403),
            // A releases; B now wins.
            ev("07", Release, "src/auth/**", &a, "mc-mac", 500),
            ev("08", Acquire, "src/auth/**", &b, "mc-win", 501),
        ];
        let out = replay(&log, &CONFIG, exact);
        assert_eq!(out.rejected.len(), 1);
        let r = &out.rejected[0];
        assert_eq!(
            (r.code.as_str(), r.event.as_str()),
            (ERROR_CODE_CLAIM_CONFLICT, "02")
        );
        assert_eq!((&r.task, &r.holder), (&b, &a));
        assert_eq!(out.leases.len(), 2);
        assert_eq!(out.leases["src/auth/**"].task, b);
        assert_eq!(
            out.leases["src/auth/**"].acquired_at,
            Timestamp::from_unix_ms(501_000)
        );
        assert_eq!(out.leases["docs/**"].task, b);

        // Input order and duplicates do not matter.
        let mut shuffled: Vec<LeaseEvent> = log.iter().rev().cloned().collect();
        shuffled.extend(log.iter().take(3).cloned());
        assert_eq!(replay(&shuffled, &CONFIG, exact), out);

        // Expiry: ttl 900 + grace 600 = 1500 s after the last heartbeat.
        let expiring = vec![
            ev("01", Acquire, "p", &a, "mc-mac", 0),
            ev("02", Heartbeat, "p", &a, "mc-mac", 100),
            // Exactly at the boundary the lease is still live.
            ev("03", Acquire, "p", &b, "mc-win", 1_600),
            // One second later it has expired: B wins, A's lease is dropped.
            ev("04", Acquire, "p", &b, "mc-win", 1_601),
            // A's late heartbeat cannot revive anything it no longer holds.
            ev("05", Heartbeat, "p", &a, "mc-mac", 1_602),
        ];
        let out = replay(&expiring, &CONFIG, exact);
        assert_eq!(out.rejected.len(), 1);
        assert_eq!(out.rejected[0].event, "03");
        assert_eq!(out.leases["p"].task, b);
        assert_eq!(
            out.leases["p"].last_heartbeat,
            Timestamp::from_unix_ms(1_601_000)
        );

        // Sweeps remove only expired leases, idempotently; holder re-acquire refreshes.
        let sweeping = vec![
            ev("01", Acquire, "p", &a, "mc-mac", 0),
            ev("02", Acquire, "p", &a, "mc-mac", 50),
            ev("03", ExpireSweep, "p", &a, "mc-win", 1_550),
            ev("04", ExpireSweep, "p", &a, "mc-win", 1_551),
            ev("05", ExpireSweep, "p", &a, "mc-lin", 1_552),
        ];
        let out = replay(&sweeping[..3], &CONFIG, exact);
        assert_eq!(out.leases["p"].acquired_at, Timestamp::from_unix_ms(0));
        assert_eq!(
            out.leases["p"].last_heartbeat,
            Timestamp::from_unix_ms(50_000)
        );
        assert!(replay(&sweeping, &CONFIG, exact).leases.is_empty());

        // Overlap is delegated to the caller's conflict rule.
        let overlap = vec![
            ev("01", Acquire, "src/**", &a, "mc-mac", 0),
            ev("02", Acquire, "src/auth/**", &b, "mc-win", 1),
        ];
        assert_eq!(replay(&overlap, &CONFIG, exact).rejected.len(), 0);
        let prefix = |x: &str, y: &str| x.starts_with(&y[..3]) || y.starts_with(&x[..3]);
        let out = replay(&overlap, &CONFIG, prefix);
        assert_eq!(out.rejected.len(), 1);
        assert_eq!(out.rejected[0].holder_path, "src/**");
    }
}
