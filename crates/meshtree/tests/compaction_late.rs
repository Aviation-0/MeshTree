//! A claim the daemon acknowledged, which reaches the ref only after another
//! clone's compaction passed it, is re-issued above the watermark (`SPEC.md`
//! §9 Compaction, §10; `ADR-0010`).

mod common;

use common::{assert_exit, git, meshtree, spawn_task, DaemonProc, TempDir};
use meshtree_core::lease::{EventType, LeaseEvent, ReplayConfig, COMPACT_THRESHOLD};
use meshtree_core::task::TaskId;
use meshtree_core::time::Timestamp;
use meshtree_platform::coord::CoordStore;
use meshtree_platform::gitops::CliGitOps;
use meshtree_platform::ipc::{Endpoint, IpcClient, Response};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CONFIG: ReplayConfig = ReplayConfig {
    default_ttl_secs: 900,
    skew_grace_secs: 600,
};

fn clone(remote: &TempDir) -> TempDir {
    let dir = TempDir::new();
    let out = Command::new("git")
        .args(["clone", "-q", "-b", "main", &remote.file_url()])
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_exit(&meshtree(dir.path(), &["init"]), 0);
    dir
}

fn rpc(root: &Path, method: &str, params: Value) -> Response {
    IpcClient::connect(&Endpoint::for_repo(root).unwrap())
        .expect("daemon reachable")
        .call(method, params)
        .unwrap()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// The local coordination state of `dir`: snapshot watermark ts and events.
fn state(dir: &Path) -> (Option<u64>, Vec<LeaseEvent>) {
    let tip = git(dir, &["rev-parse", "refs/meshtree/coord"]);
    let reader = CoordStore::new(CliGitOps::new(dir), None, CONFIG);
    let (snapshot, log) = reader.read_state(tip.trim()).unwrap();
    (snapshot.map(|s| s.watermark.ts.unix_ms()), log)
}

#[test]
fn late_event_below_watermark_reissued() {
    let seed = TempDir::repo_with_commit();
    let remote = TempDir::bare();
    git(seed.path(), &["push", "-q", &remote.file_url(), "main"]);
    let mac = clone(&remote);
    let holder = spawn_task(mac.path(), "holder");
    let rival = spawn_task(mac.path(), "rival");

    // The daemon acknowledges a claim, then dies before committing it.
    let mut child = Command::new(env!("CARGO_BIN_EXE_meshtree"))
        .arg("daemon")
        .current_dir(mac.path())
        .env("MESHTREE_TEST_FLUSH_DELAY_MS", "60000")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    assert!(line.contains("listening on"), "{line:?}");
    let resp = rpc(
        mac.path(),
        "claim_paths",
        json!({ "task_id": holder, "paths": ["late/**"] }),
    );
    assert!(resp.ok, "{resp:?}");
    let acknowledged_at = now_ms();
    child.kill().unwrap();
    child.wait().unwrap();
    drop(stdout);

    // Meanwhile another clone writes history whose compaction folds past
    // the acknowledged claim: events just after it, and one two days later
    // that puts them beyond the 24 h horizon.
    let win = clone(&remote);
    let base = acknowledged_at + 1_000;
    let mut events: Vec<LeaseEvent> = (0..(COMPACT_THRESHOLD as u64 / 2 + 60))
        .flat_map(|i| {
            [EventType::Acquire, EventType::Release]
                .into_iter()
                .enumerate()
                .map(move |(k, kind)| LeaseEvent {
                    id: format!("01SEED{i:07}{k}"),
                    kind,
                    path: format!("seed/{i}/**"),
                    task: TaskId::from_ulid(99),
                    machine: "mc-win".into(),
                    ts: Timestamp::from_unix_ms(base + 2 * i + k as u64),
                    counter: 0,
                    ttl_secs: (kind == EventType::Acquire).then_some(900),
                })
        })
        .collect();
    events.push(LeaseEvent {
        id: "01SEEDNEWEST".into(),
        kind: EventType::Heartbeat,
        path: "seed/0/**".into(),
        task: TaskId::from_ulid(99),
        machine: "mc-win".into(),
        ts: Timestamp::from_unix_ms(base + 2 * 24 * 60 * 60 * 1000),
        counter: 0,
        ttl_secs: None,
    });
    let store = CoordStore::new(CliGitOps::new(win.path()), Some("origin".into()), CONFIG)
        .with_compaction(COMPACT_THRESHOLD);
    let outcome = store.sync(&events).unwrap();
    assert!(outcome.compacted && outcome.pushed, "{outcome:?}");
    let watermark = outcome.watermark.unwrap().ts.unix_ms();
    assert!(watermark > acknowledged_at);

    // Restart: recovery re-queues the journaled claim, the flush finds it
    // below the remote watermark, and the daemon re-issues it above.
    let daemon = DaemonProc::start(mac.path());
    let deadline = Instant::now() + Duration::from_secs(15);
    let reissued = loop {
        let (mark, log) = state(mac.path());
        let found = log.into_iter().find(|e| {
            e.kind == EventType::Acquire && e.path == "late/**" && e.task.as_str() == holder
        });
        if let (Some(mark), Some(e)) = (mark, found) {
            break (mark, e);
        }
        assert!(Instant::now() < deadline, "claim was not re-issued");
        std::thread::sleep(Duration::from_millis(50));
    };
    let (mark, event) = reissued;
    assert_eq!(mark, watermark, "the remote snapshot won");
    assert!(
        event.ts.unix_ms() > watermark,
        "re-issued above the watermark"
    );

    // The claim holds: a rival is refused, and the lease is in the ref.
    let resp = rpc(
        mac.path(),
        "claim_paths",
        json!({ "task_id": rival, "paths": ["late/x.rs"] }),
    );
    assert!(!resp.ok, "{resp:?}");
    assert_eq!(resp.error.unwrap().code, "MT-001");
    let out = meshtree(mac.path(), &["sync"]);
    assert_exit(&out, 0);
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("leases: 1"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    daemon.stop(mac.path());
}
