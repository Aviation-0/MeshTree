//! Compaction through the coordination store: concurrent compactions on two
//! clones converge (`SPEC.md` §9 Compaction, `ADR-0010`).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use meshtree_core::lease::{
    replay, replay_from, EventType, LeaseEvent, ReplayConfig, COMPACT_THRESHOLD,
};
use meshtree_core::pattern::conflicts_raw;
use meshtree_core::task::TaskId;
use meshtree_core::time::Timestamp;
use meshtree_platform::coord::{CoordStore, COORD_REF};
use meshtree_platform::gitops::{CliGitOps, GitOps};

const CONFIG: ReplayConfig = ReplayConfig {
    default_ttl_secs: 900,
    skew_grace_secs: 600,
};
const DAY_MS: u64 = 24 * 60 * 60 * 1000;
const T0: u64 = 1_700_000_000_000;

struct TempRepo(PathBuf);

static COUNTER: AtomicU64 = AtomicU64::new(0);

impl TempRepo {
    fn new(bare: bool) -> Self {
        let path = std::env::temp_dir().join(format!(
            "meshtree-compact-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        let repo = Self(path);
        repo.git(&[
            "init",
            "-q",
            if bare {
                "--bare"
            } else {
                "--initial-branch=main"
            },
        ]);
        repo
    }

    fn git(&self, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.0)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn file_url(&self) -> String {
        let path = self.0.to_string_lossy().replace('\\', "/");
        if path.starts_with('/') {
            format!("file://{path}")
        } else {
            format!("file:///{path}")
        }
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempRepo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn event(id: String, kind: EventType, path: String, task: u128, ms: u64) -> LeaseEvent {
    LeaseEvent {
        id,
        kind,
        path,
        task: TaskId::from_ulid(task),
        machine: "mc-test".into(),
        ts: Timestamp::from_unix_ms(ms),
        counter: 0,
        ttl_secs: (kind == EventType::Acquire).then_some(900),
    }
}

/// `pairs` ACQUIRE/RELEASE pairs on distinct paths starting at `T0`.
fn history(pairs: usize) -> Vec<LeaseEvent> {
    (0..pairs as u64)
        .flat_map(|i| {
            [
                event(
                    format!("01HIST{i:06}A"),
                    EventType::Acquire,
                    format!("seed/{i}/**"),
                    1,
                    T0 + 2 * i,
                ),
                event(
                    format!("01HIST{i:06}R"),
                    EventType::Release,
                    format!("seed/{i}/**"),
                    1,
                    T0 + 2 * i + 1,
                ),
            ]
        })
        .collect()
}

fn store(repo: &TempRepo, compact: bool) -> CoordStore<CliGitOps> {
    let store = CoordStore::new(CliGitOps::new(repo.path()), Some("origin".into()), CONFIG);
    if compact {
        store.with_compaction(COMPACT_THRESHOLD)
    } else {
        store
    }
}

fn tree_paths(repo: &TempRepo, oid: &str) -> Vec<String> {
    CliGitOps::new(repo.path())
        .ls_tree(oid)
        .unwrap()
        .into_iter()
        .map(|e| e.path)
        .collect()
}

#[test]
fn concurrent_compaction_two_clones_converges() {
    let remote = TempRepo::new(true);
    let (a, b) = (TempRepo::new(false), TempRepo::new(false));
    for clone in [&a, &b] {
        clone.git(&["remote", "add", "origin", &remote.file_url()]);
    }

    // Shared history, older than the horizon, written without compaction.
    let old = history(600); // 1,200 events: more than COMPACT_THRESHOLD
    store(&a, false).sync(&old).unwrap();
    store(&b, false).sync(&[]).unwrap();

    // Each clone adds its own events, then both compact at once. B also has
    // an old event A never saw, so their watermarks differ.
    let a_new = vec![event(
        "01ANEW0000001".into(),
        EventType::Acquire,
        "a/**".into(),
        2,
        T0 + 3 * DAY_MS,
    )];
    let b_new = vec![
        event(
            "01BMID0000001".into(),
            EventType::Acquire,
            "b/**".into(),
            3,
            T0 + 1_300,
        ),
        event(
            "01BNEW0000001".into(),
            EventType::Heartbeat,
            "b/**".into(),
            3,
            T0 + 5 * DAY_MS,
        ),
    ];
    let (ra, rb) = std::thread::scope(|s| {
        let ha = s.spawn(|| store(&a, true).sync(&a_new).unwrap());
        let hb = s.spawn(|| store(&b, true).sync(&b_new).unwrap());
        (ha.join().unwrap(), hb.join().unwrap())
    });
    assert!(ra.compacted || rb.compacted, "someone compacted");

    // Both converge on the same tree after a further round each.
    let fa = store(&a, true).sync(&[]).unwrap();
    let fb = store(&b, true).sync(&[]).unwrap();
    let fa2 = store(&a, true).sync(&[]).unwrap();
    assert_eq!(fb.oid, fa2.oid, "identical coordination refs");
    assert_eq!(fa.watermark, fb.watermark);

    // Exactly one snapshot, and the replayed state equals the full log's.
    let paths = tree_paths(&a, &fa2.oid);
    let snapshots: Vec<_> = paths
        .iter()
        .filter(|p| p.starts_with("snapshot/"))
        .collect();
    assert_eq!(snapshots.len(), 1, "{snapshots:?}");
    let mut all = old.clone();
    all.extend(a_new.iter().cloned());
    all.extend(b_new.iter().cloned());
    let full = replay(&all, &CONFIG, conflicts_raw);
    let reader = CoordStore::new(CliGitOps::new(a.path()), None, CONFIG);
    let (snapshot, log) = reader.read_state(&fa2.oid).unwrap();
    let snapshot = snapshot.expect("compacted");
    assert_eq!(
        replay_from(Some(&snapshot), &log, &CONFIG, conflicts_raw).leases,
        full.leases
    );
    assert_eq!(fa2.replay.leases, full.leases);

    // The log holds only events above the watermark; B's old event was not
    // lost (it is above A's watermark or folded into the winning snapshot).
    assert!(log.iter().all(|e| !snapshot.watermark.covers(e)));
    assert!(
        log.len() < all.len() / 10,
        "{} events kept of {}",
        log.len(),
        all.len()
    );
    assert!(full.leases.contains_key("b/**") && full.leases.contains_key("a/**"));

    // The local ref advanced on both sides.
    for clone in [&a, &b] {
        let tip = CliGitOps::new(clone.path()).ref_read(COORD_REF).unwrap();
        assert_eq!(tip.as_deref(), Some(fa2.oid.as_str()));
    }
}
