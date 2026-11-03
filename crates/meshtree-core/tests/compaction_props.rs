//! Property tests: compaction never changes the replayed state (`SPEC.md`
//! §9 Compaction, §18; `ADR-0010`: `replay(compact(L)) == replay(L)`).

use meshtree_core::lease::coord::{decode_snapshot, encode_compacted, snapshot_file};
use meshtree_core::lease::{
    compact, compact_horizon_secs, replay, replay_from, EventType, LeaseEvent, ReplayConfig,
    Watermark,
};
use meshtree_core::pattern::conflicts_raw;
use meshtree_core::task::{TaskId, TaskIdAllocator};
use meshtree_core::time::Timestamp;
use proptest::prelude::*;

const CONFIG: ReplayConfig = ReplayConfig {
    default_ttl_secs: 900,
    skew_grace_secs: 600,
};

const DAY_MS: u64 = 24 * 60 * 60 * 1000;
const PATHS: [&str; 6] = [
    "src/**",
    "src/auth",
    "src/auth/x.rs",
    "docs/**",
    "tests",
    "*",
];
const MACHINES: [&str; 3] = ["mc-mac", "mc-win", "mc-lin"];

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

/// (kind, path, task, machine, gap before this event in ms, counter, ttl).
type Body = (EventType, usize, usize, usize, u64, u64, Option<u64>);

fn any_body() -> impl Strategy<Value = Body> {
    (
        any_kind(),
        0..PATHS.len(),
        0..3usize,
        0..MACHINES.len(),
        // Mostly short gaps (TTL-scale), sometimes days: logs that span the
        // compaction horizon with dense and sparse stretches.
        prop_oneof![
            8 => 0u64..2_000_000,
            1 => DAY_MS / 2..3 * DAY_MS,
        ],
        0u64..3,
        prop::option::of(1u64..2_000),
    )
}

/// Events with increasing time (gaps accumulate), shuffled ids by position.
fn build(bodies: Vec<Body>) -> Vec<LeaseEvent> {
    let tasks = task_ids();
    let mut ms = 10 * DAY_MS;
    bodies
        .into_iter()
        .enumerate()
        .map(|(i, (kind, path, task, machine, gap, counter, ttl))| {
            ms += gap;
            LeaseEvent {
                id: format!("01EVENT{i:019}"),
                kind,
                path: PATHS[path].to_string(),
                task: tasks[task].clone(),
                machine: MACHINES[machine].to_string(),
                ts: Timestamp::from_unix_ms(ms),
                counter,
                ttl_secs: ttl,
            }
        })
        .collect()
}

fn bytes<T: serde::Serialize>(value: &T) -> Vec<u8> {
    serde_json::to_vec(value).unwrap()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn compaction_replay_equivalence_props(
        events in prop::collection::vec(any_body(), 0..80).prop_map(build),
        threshold in 0usize..20,
        later in prop::collection::vec(any_body(), 0..30),
    ) {
        let full = replay(&events, &CONFIG, conflicts_raw);
        let compacted = compact(None, &events, &CONFIG, conflicts_raw, threshold);
        let (snapshot, rest) = match compacted {
            Some(done) => done,
            None => {
                // Nothing to fold: every foldable count is at most the threshold.
                let newest = events.iter().map(|e| e.ts.unix_ms()).max().unwrap_or(0);
                let cutoff = newest.saturating_sub(compact_horizon_secs(&CONFIG) * 1000);
                let foldable = events.iter().filter(|e| e.ts.unix_ms() <= cutoff).count();
                prop_assert!(foldable <= threshold || newest < compact_horizon_secs(&CONFIG) * 1000);
                return Ok(());
            }
        };

        // replay(compact(L)) == replay(L): the table byte for byte; the
        // rejections of folded events live on as snapshot ids, the rest in
        // full and in order.
        let resumed = replay_from(Some(&snapshot), &rest, &CONFIG, conflicts_raw);
        prop_assert_eq!(bytes(&resumed.leases), bytes(&full.leases));
        let covered = |id: &str| events.iter().any(|e| e.id == id && snapshot.watermark.covers(e));
        let (folded, above): (Vec<_>, Vec<_>) = full.rejected.iter().partition(|r| covered(&r.event));
        prop_assert_eq!(&resumed.rejected.iter().collect::<Vec<_>>(), &above);
        prop_assert_eq!(&snapshot.rejected, &folded.iter().map(|r| r.event.clone()).collect::<Vec<_>>());
        prop_assert!(snapshot.leases.windows(2).all(|w| w[0].path < w[1].path));

        // Folded events are exactly those at or below the watermark, all
        // older than the horizon behind the newest event.
        let newest = events.iter().map(|e| e.ts).max().unwrap();
        let horizon = compact_horizon_secs(&CONFIG) * 1000;
        for e in &events {
            let folded = snapshot.watermark.covers(e);
            prop_assert_eq!(folded, !rest.iter().any(|r| r.id == e.id));
            if folded {
                prop_assert!(e.ts.unix_ms() + horizon <= newest.unix_ms());
            }
        }

        // Events covered by the watermark are ignored on replay (a late
        // event is dropped, never double-counted), and replaying the union
        // of the compacted tree with the full log changes nothing.
        let mut union = rest.clone();
        union.extend(events.iter().cloned());
        prop_assert_eq!(bytes(&replay_from(Some(&snapshot), &union, &CONFIG, conflicts_raw)), bytes(&resumed));

        // Deterministic: the same log, in any order, compacts identically.
        let mut reversed = events.clone();
        reversed.reverse();
        let again = compact(None, &reversed, &CONFIG, conflicts_raw, threshold).unwrap();
        prop_assert_eq!(bytes(&again.0), bytes(&snapshot));

        // Compacting again later (more events) still matches the full replay.
        let mut grown = events.clone();
        let tail = build(later);
        let offset = newest.unix_ms();
        grown.extend(tail.into_iter().enumerate().map(|(i, mut e)| {
            e.id = format!("01LATER{i:019}");
            e.ts = Timestamp::from_unix_ms(offset + e.ts.unix_ms() - 10 * DAY_MS + 1);
            e
        }));
        let mut kept = rest.clone();
        kept.extend(grown[events.len()..].iter().cloned());
        let full_grown = replay(&grown, &CONFIG, conflicts_raw);
        if let Some((second, rest2)) = compact(Some(&snapshot), &kept, &CONFIG, conflicts_raw, 0) {
            prop_assert!(second.watermark >= snapshot.watermark);
            prop_assert!(second.rejected.starts_with(&snapshot.rejected));
            prop_assert_eq!(bytes(&replay_from(Some(&second), &rest2, &CONFIG, conflicts_raw).leases), bytes(&full_grown.leases));
        }
        prop_assert_eq!(bytes(&replay_from(Some(&snapshot), &kept, &CONFIG, conflicts_raw).leases), bytes(&full_grown.leases));

        // The tree codec round-trips the snapshot byte for byte.
        let files = encode_compacted(Some(&snapshot), &rest, &full);
        let path = snapshot_file(&snapshot);
        prop_assert!(files.contains_key(&path));
        let (decoded, issues) = decode_snapshot(files.iter().map(|(p, b)| (p.as_str(), b.as_slice())));
        prop_assert!(issues.is_empty());
        prop_assert_eq!(bytes(&decoded.unwrap()), bytes(&snapshot));
    }
}

#[test]
fn horizon_and_watermark_order() {
    // max(24 h, 4 × (ttl + 2 × grace)).
    assert_eq!(compact_horizon_secs(&CONFIG), 24 * 60 * 60);
    let long = ReplayConfig {
        default_ttl_secs: 36_000,
        skew_grace_secs: 600,
    };
    assert_eq!(compact_horizon_secs(&long), 4 * (36_000 + 1_200));

    let task = task_ids().remove(0);
    let ev = |id: &str, ms: u64, counter: u64| LeaseEvent {
        id: id.into(),
        kind: EventType::Acquire,
        path: "src".into(),
        task: task.clone(),
        machine: "mc".into(),
        ts: Timestamp::from_unix_ms(ms),
        counter,
        ttl_secs: None,
    };
    let w = Watermark::of(&ev("01B", 5, 1));
    assert!(w.covers(&ev("01B", 5, 1)));
    assert!(w.covers(&ev("01A", 5, 1)));
    assert!(w.covers(&ev("01Z", 5, 0)));
    assert!(w.covers(&ev("01Z", 4, 9)));
    assert!(!w.covers(&ev("01C", 5, 1)));
    assert!(!w.covers(&ev("01A", 6, 0)));
    // A malformed snapshot name is reported, not trusted.
    let files = [("snapshot/other.json", b"{}".as_slice())];
    let (snapshot, issues) = decode_snapshot(files);
    assert!(snapshot.is_none());
    assert_eq!(issues.len(), 1);
}
