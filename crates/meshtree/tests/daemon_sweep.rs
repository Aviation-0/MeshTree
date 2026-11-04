//! A daemon reaps a vanished machine's expired lease with an audited
//! `EXPIRE_SWEEP` (`SPEC.md` §9, §18, §21; `ADR-0008`).

mod common;

use common::{assert_exit, git, meshtree, spawn_task, DaemonProc, TempDir};
use meshtree_core::lease::{EventType, LeaseEvent, ReplayConfig};
use meshtree_core::task::TaskIdAllocator;
use meshtree_core::time::Timestamp;
use meshtree_platform::coord::CoordStore;
use meshtree_platform::gitops::CliGitOps;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Short lease: expired `ttl + grace` = 3 s after the holder's last event.
const CONFIG: &str = r#"[project]
base_branch = "main"

[lease]
ttl_secs = 2
heartbeat_secs = 1
skew_grace_secs = 1
"#;

const REPLAY: ReplayConfig = ReplayConfig {
    default_ttl_secs: 2,
    skew_grace_secs: 1,
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
    fs::write(dir.path().join(".meshtree/config.toml"), CONFIG).unwrap();
    dir
}

/// The `log/` events in `dir`'s local coordination ref.
fn log(dir: &Path) -> Vec<LeaseEvent> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--verify", "-q", "refs/meshtree/coord"])
        .output()
        .unwrap();
    if !out.status.success() {
        return Vec::new();
    }
    let tip = String::from_utf8_lossy(&out.stdout).trim().to_string();
    CoordStore::new(CliGitOps::new(dir), None, REPLAY)
        .read_state(&tip)
        .unwrap()
        .1
}

fn sweeps(dir: &Path, path: &str) -> Vec<LeaseEvent> {
    log(dir)
        .into_iter()
        .filter(|e| e.kind == EventType::ExpireSweep && e.path == path)
        .collect()
}

fn leases_after_sync(dir: &Path) -> String {
    let out = meshtree(dir, &["sync"]);
    assert_exit(&out, 0);
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find(|l| l.starts_with("leases: "))
        .unwrap()
        .to_string()
}

#[test]
fn daemon_sweeps_vanished_machine_lease_with_audited_event() {
    let seed = TempDir::repo_with_commit();
    let remote = TempDir::bare();
    git(seed.path(), &["push", "-q", &remote.file_url(), "main"]);

    // Machine A claims, pushes, and vanishes: no daemon, no heartbeats.
    let a = clone(&remote);
    let task = spawn_task(a.path(), "vanishing");
    assert_exit(&meshtree(a.path(), &["claim", &task, "vanish/**"]), 0);
    let acquire = log(a.path())
        .into_iter()
        .find(|e| e.kind == EventType::Acquire && e.path == "vanish/**")
        .expect("claim committed and pushed");
    let expires_ms = acquire.ts.unix_ms() + 3_000;

    // Two observer machines run daemons.
    let b = clone(&remote);
    let c = clone(&remote);
    let b_daemon = DaemonProc::start(b.path());
    let c_daemon = DaemonProc::start(c.path());

    // Never before ttl + grace: until then neither observer sweeps.
    let now_ms = || {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    };
    while now_ms() + 300 < expires_ms {
        assert!(sweeps(b.path(), "vanish/**").is_empty(), "swept early");
        assert!(sweeps(c.path(), "vanish/**").is_empty(), "swept early");
        std::thread::sleep(Duration::from_millis(100));
    }

    // After it, an observer sweeps; the event is in log/ and audited.
    let deadline = Instant::now() + Duration::from_secs(20);
    let swept = loop {
        let mut found = sweeps(b.path(), "vanish/**");
        found.extend(sweeps(c.path(), "vanish/**"));
        if !found.is_empty() {
            break found;
        }
        assert!(
            Instant::now() < deadline,
            "the expired lease was never swept"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    for sweep in &swept {
        assert_eq!(sweep.task.as_str(), task, "names the holder task");
        assert!(
            sweep.ts.unix_ms() > expires_ms,
            "sweep at {} <= expiry {expires_ms}",
            sweep.ts.unix_ms()
        );
    }

    // The lease is gone from leases/ on every clone after a sync.
    for dir in [b.path(), c.path(), a.path()] {
        let deadline = Instant::now() + Duration::from_secs(10);
        while leases_after_sync(dir) != "leases: 0" {
            assert!(
                Instant::now() < deadline,
                "lease still present in {}",
                dir.display()
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    // A second observer's sweep of the same lease changes nothing.
    let store = CoordStore::new(CliGitOps::new(c.path()), Some("origin".into()), REPLAY);
    let before = store.sync(&[]).unwrap();
    let late = LeaseEvent {
        id: TaskIdAllocator::new().allocate(now_ms(), 7).as_str()[2..].to_string(),
        kind: EventType::ExpireSweep,
        path: "vanish/**".into(),
        task: acquire.task.clone(),
        machine: "mc-observer-2".into(),
        ts: Timestamp::from_unix_ms(now_ms()),
        counter: 0,
        ttl_secs: None,
    };
    let after = store.sync(std::slice::from_ref(&late)).unwrap();
    assert_eq!(after.replay, before.replay, "idempotent no-op");
    assert!(after.replay.leases.is_empty());
    assert!(
        log(c.path()).iter().any(|e| e.id == late.id),
        "still audited"
    );

    b_daemon.stop(b.path());
    c_daemon.stop(c.path());
}
