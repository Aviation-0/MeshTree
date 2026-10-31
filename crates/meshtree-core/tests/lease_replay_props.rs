//! Property tests: lease replay is a deterministic function of the event *set*.
//!
//! Authority: `SPEC.md` §9 (deterministic replay: "any two machines replaying
//! the same log derive byte-identical lease tables"), §16 (`MT-001`), §18.

use meshtree_core::lease::{
    replay, EventType, LeaseEvent, ReplayConfig, ERROR_CODE_CLAIM_CONFLICT,
};
use meshtree_core::task::{TaskId, TaskIdAllocator};
use meshtree_core::time::Timestamp;
use proptest::prelude::*;
use std::collections::BTreeSet;

const CONFIG: ReplayConfig = ReplayConfig {
    default_ttl_secs: 900,
    skew_grace_secs: 600,
};

const PATHS: [&str; 5] = ["src", "src/auth", "src/auth/x.rs", "docs", "tests"];
const MACHINES: [&str; 3] = ["mc-mac", "mc-win", "mc-lin"];

/// Directory-prefix overlap: a conservative stand-in for the §7 rule.
fn prefix_conflict(a: &str, b: &str) -> bool {
    a == b || a.starts_with(&format!("{b}/")) || b.starts_with(&format!("{a}/"))
}

fn task_ids() -> Vec<TaskId> {
    let mut alloc = TaskIdAllocator::new();
    (0..3u128).map(|i| alloc.allocate(1_000, i)).collect()
}

fn any_kind() -> impl Strategy<Value = EventType> {
    prop::sample::select(vec![
        EventType::Acquire,
        EventType::Acquire,
        EventType::Release,
        EventType::Heartbeat,
        EventType::ExpireSweep,
    ])
}

/// Event fields without the id; ids are assigned by position so they are unique.
fn any_event_body() -> impl Strategy<Value = (EventType, usize, usize, usize, u64, u64, Option<u64>)>
{
    (
        any_kind(),
        0..PATHS.len(),
        0..3usize,
        0..MACHINES.len(),
        // Up to ~67 min of event time: several TTL + grace windows.
        0u64..4_000_000,
        0u64..3,
        prop::option::of(0u64..2_000),
    )
}

fn build(bodies: Vec<(EventType, usize, usize, usize, u64, u64, Option<u64>)>) -> Vec<LeaseEvent> {
    let tasks = task_ids();
    bodies
        .into_iter()
        .enumerate()
        .map(
            |(i, (kind, path, task, machine, ms, counter, ttl))| LeaseEvent {
                id: format!("01EVENT{i:019}"),
                kind,
                path: PATHS[path].to_string(),
                task: tasks[task].clone(),
                machine: MACHINES[machine].to_string(),
                ts: Timestamp::from_unix_ms(ms),
                counter,
                ttl_secs: ttl,
            },
        )
        .collect()
}

/// A random event log, plus a shuffled copy with some events duplicated
/// (as when two machines' logs are unioned).
fn log_and_permutation() -> impl Strategy<Value = (Vec<LeaseEvent>, Vec<LeaseEvent>)> {
    prop::collection::vec(any_event_body(), 0..48)
        .prop_map(build)
        .prop_flat_map(|events| {
            let n = events.len();
            let dups = prop::collection::vec(0..n.max(1), 0..=n.min(8));
            (Just(events.clone()), Just(events).prop_shuffle(), dups)
        })
        .prop_map(|(events, mut shuffled, dups)| {
            if !events.is_empty() {
                for i in dups {
                    shuffled.push(events[i].clone());
                }
            }
            (events, shuffled)
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn replay_determinism_200_random_streams((events, permuted) in log_and_permutation()) {
        let a = replay(&events, &CONFIG, prefix_conflict);
        let b = replay(&permuted, &CONFIG, prefix_conflict);

        // Byte-identical derived lease table and REJECTED audit output.
        let a_bytes = serde_json::to_vec(&a).unwrap();
        let b_bytes = serde_json::to_vec(&b).unwrap();
        prop_assert_eq!(&a_bytes, &b_bytes);
        prop_assert_eq!(
            serde_json::to_vec(&a.leases).unwrap(),
            serde_json::to_vec(&b.leases).unwrap()
        );

        // Splitting the log across two "machines" and unioning is the same.
        let (left, right) = permuted.split_at(permuted.len() / 2);
        let mut union = right.to_vec();
        union.extend_from_slice(left);
        prop_assert_eq!(&serde_json::to_vec(&replay(&union, &CONFIG, prefix_conflict)).unwrap(), &a_bytes);

        // No two different tasks hold conflicting leases in the derived table.
        let leases: Vec<_> = a.leases.values().collect();
        for (i, x) in leases.iter().enumerate() {
            prop_assert_eq!(&x.path, a.leases.keys().nth(i).unwrap());
            for y in &leases[i + 1..] {
                prop_assert!(
                    x.task == y.task || !prefix_conflict(&x.path, &y.path),
                    "{} ({}) vs {} ({})", x.path, x.task, y.path, y.task
                );
            }
        }

        // Every rejection is an MT-001 audit of a distinct ACQUIRE from the log.
        let acquires: BTreeSet<&str> = events
            .iter()
            .filter(|e| e.kind == EventType::Acquire)
            .map(|e| e.id.as_str())
            .collect();
        let mut rejected_ids = BTreeSet::new();
        for r in &a.rejected {
            prop_assert_eq!(r.code.as_str(), ERROR_CODE_CLAIM_CONFLICT);
            prop_assert!(acquires.contains(r.event.as_str()));
            prop_assert!(rejected_ids.insert(r.event.as_str()), "duplicate rejection {}", r.event);
            prop_assert!(r.task != r.holder);
            prop_assert!(prefix_conflict(&r.path, &r.holder_path));
        }
    }

    /// Replay never panics and always yields a table keyed by its own paths,
    /// whatever the (possibly adversarial) TTL values.
    #[test]
    fn replay_tolerates_extreme_ttls(
        (events, _) in log_and_permutation(),
        huge in any::<u64>(),
    ) {
        let mut events = events;
        for e in events.iter_mut().step_by(3) {
            e.ttl_secs = Some(huge);
        }
        let config = ReplayConfig { default_ttl_secs: huge, skew_grace_secs: huge };
        let out = replay(&events, &config, prefix_conflict);
        for (path, lease) in &out.leases {
            prop_assert_eq!(path, &lease.path);
        }
    }
}
