//! Singleton `daemon.lock` and stale-lock adoption (`SPEC.md` §5, §10, §16
//! `MT-011`, §18 chaos: "stale `daemon.lock` with live pid vs dead pid →
//! correct adoption").
//!
//! Real holder processes are this test binary re-executed with
//! [`HELPER_ENV`] set, running only `lock_holder_helper`.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};

use meshtree_platform::lock::{
    boot_id, pid_alive, same_boot, Acquired, DaemonLock, LockError, LockRecord, StaleReason,
    ERROR_CODE_STALE_LOCK, LOCK_FILE,
};

const HELPER_ENV: &str = "MESHTREE_LOCK_HELPER";
const MACHINE: &str = "mc-test";

struct TempRepo(PathBuf);

static COUNTER: AtomicU64 = AtomicU64::new(0);

impl TempRepo {
    fn new() -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "mt-lock-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(path.join(".meshtree")).expect("create temp repo");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn lock_file(&self) -> PathBuf {
        self.0.join(".meshtree").join(LOCK_FILE)
    }

    fn write_record(&self, record: &LockRecord) {
        fs::write(self.lock_file(), serde_json::to_vec(record).unwrap()).unwrap();
    }

    fn read_record(&self) -> Option<LockRecord> {
        let text = fs::read_to_string(self.lock_file()).unwrap();
        (!text.trim().is_empty()).then(|| serde_json::from_str(&text).unwrap())
    }
}

impl Drop for TempRepo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Child-process body. `hold:<repo>` acquires the lock; `idle` holds nothing.
/// Prints `READY <pid>` and then blocks until killed. A no-op in normal runs.
#[test]
fn lock_holder_helper() {
    let Ok(mode) = std::env::var(HELPER_ENV) else {
        return;
    };
    let _held = mode.strip_prefix("hold:").map(|repo| {
        DaemonLock::acquire(Path::new(repo), MACHINE).expect("helper acquires the lock")
    });
    println!("READY {}", std::process::id());
    loop {
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
}

/// A helper process, killed and reaped on drop so a failing assertion never
/// leaves an orphan holding the test harness's pipes open.
struct Helper(Option<Child>);

impl Helper {
    fn id(&self) -> u32 {
        self.0.as_ref().unwrap().id()
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Spawns a helper and waits until it reports ready.
fn helper(mode: &str) -> Helper {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "lock_holder_helper",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(HELPER_ENV, mode)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn helper");
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let helper = Helper(Some(child));
    let ready = format!("READY {}", helper.id());
    loop {
        let line = lines
            .next()
            .expect("helper exited before READY")
            .expect("read helper stdout");
        if line.contains(&ready) {
            break;
        }
    }
    helper
}

/// `SIGKILL` (Unix) / `TerminateProcess` (Windows), then reap.
fn kill(mut helper: Helper) {
    let mut child = helper.0.take().unwrap();
    child.kill().expect("kill helper");
    child.wait().expect("reap helper");
}

/// The pid of a process that has exited and been reaped.
fn dead_pid() -> u32 {
    let mut child = Command::new("git")
        .arg("--version")
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn git");
    let pid = child.id();
    child.wait().unwrap();
    assert!(!pid_alive(pid));
    pid
}

fn acquire(repo: &TempRepo) -> Result<Acquired, LockError> {
    DaemonLock::acquire(repo.path(), MACHINE)
}

#[test]
fn chaos_stale_daemon_lock_live_vs_dead_pid_adoption() {
    let repo = TempRepo::new();
    let path = repo.path().to_str().unwrap().to_string();

    // 1. A live daemon process holds the lock: rejected, holder named.
    let holder = helper(&format!("hold:{path}"));
    let holder_pid = holder.id();
    let on_disk = repo.read_record().expect("holder wrote its record");
    assert_eq!(on_disk.pid, holder_pid);
    assert!(same_boot(&on_disk.boot_id, &boot_id().unwrap()));
    assert_eq!(on_disk.machine_id, MACHINE);
    match acquire(&repo) {
        Err(LockError::Held {
            holder: Some(h), ..
        }) => assert_eq!(h, on_disk),
        other => panic!("live holder must be rejected, got {other:?}"),
    }

    // 2. The daemon is SIGKILLed: its record stays behind with a dead pid.
    kill(holder);
    assert!(!pid_alive(holder_pid));
    assert_eq!(
        repo.read_record().as_ref(),
        Some(&on_disk),
        "record survives the crash"
    );

    // 3. The next daemon adopts the stale lock and reports MT-011.
    let adopted = acquire(&repo).expect("dead-pid lock is adopted");
    let stale = adopted.recovered.as_ref().expect("adoption is reported");
    assert_eq!(stale.code(), ERROR_CODE_STALE_LOCK);
    assert_eq!(stale.reason, StaleReason::DeadPid);
    assert_eq!(stale.previous.as_ref(), Some(&on_disk));
    assert!(stale.to_string().contains("MT-011"));
    assert_eq!(adopted.lock.record().pid, std::process::id());
    assert_eq!(repo.read_record().as_ref(), Some(adopted.lock.record()));

    // While held, a second acquisition in the same process is also refused.
    match acquire(&repo) {
        Err(LockError::Held {
            holder: Some(h), ..
        }) => assert_eq!(h.pid, std::process::id()),
        other => panic!("held lock must be refused, got {other:?}"),
    }

    // 4. Clean release clears the record; the next acquisition is fresh.
    drop(adopted);
    assert!(
        repo.lock_file().exists(),
        "the file is cleared, never unlinked"
    );
    assert_eq!(repo.read_record(), None);
    let fresh = acquire(&repo).expect("released lock is free");
    assert!(fresh.recovered.is_none());
    drop(fresh);

    // 5. Belt-and-braces: with no kernel guard held, a record whose pid is a
    //    live process on this boot still refuses; once that pid dies it is
    //    adopted.
    let live = helper("idle");
    let live_record = LockRecord {
        pid: live.id(),
        boot_id: boot_id().unwrap(),
        machine_id: MACHINE.to_string(),
    };
    repo.write_record(&live_record);
    match acquire(&repo) {
        Err(LockError::Held {
            holder: Some(h), ..
        }) => assert_eq!(h, live_record),
        other => panic!("live pid in record must be rejected, got {other:?}"),
    }
    assert_eq!(
        repo.read_record().as_ref(),
        Some(&live_record),
        "refusal writes nothing"
    );
    kill(live);
    let adopted = acquire(&repo).expect("dead pid adopted");
    let stale = adopted.recovered.expect("adoption is reported");
    assert_eq!(stale.reason, StaleReason::DeadPid);
    assert_eq!(stale.previous, Some(live_record));
}

#[test]
fn daemon_lock_stale_record_rules() {
    let repo = TempRepo::new();
    let me = std::process::id();

    // Same machine, earlier boot: stale even though the pid (ours) is live.
    repo.write_record(&LockRecord {
        pid: me,
        boot_id: "linux-previous-boot".to_string(),
        machine_id: MACHINE.to_string(),
    });
    let got = acquire(&repo).expect("previous boot adopted");
    assert_eq!(got.recovered.unwrap().reason, StaleReason::PreviousBoot);
    drop(got.lock);

    // Another machine on another boot: cannot be probed, refused untouched.
    let foreign = LockRecord {
        pid: 1,
        boot_id: "linux-other-boot".to_string(),
        machine_id: "mc-elsewhere".to_string(),
    };
    repo.write_record(&foreign);
    match acquire(&repo) {
        Err(LockError::Foreign { holder, .. }) => assert_eq!(holder, foreign),
        other => panic!("foreign lock must be refused, got {other:?}"),
    }
    assert_eq!(repo.read_record(), Some(foreign));

    // Torn write from a crashed holder: adopted as unreadable.
    fs::write(repo.lock_file(), b"{\"pid\": 12").unwrap();
    let got = acquire(&repo).expect("unreadable record adopted");
    let stale = got.recovered.unwrap();
    assert_eq!(stale.reason, StaleReason::Unreadable);
    assert_eq!(stale.previous, None);
}

#[test]
fn daemon_lock_concurrent_acquire_exactly_one_wins() {
    let repo = TempRepo::new();

    // The kernel guard alone refuses: the holder's record is wiped, so only
    // flock / the named mutex still says "held".
    let held = acquire(&repo).expect("first acquisition");
    fs::write(repo.lock_file(), b"").unwrap();
    match acquire(&repo) {
        Err(LockError::Held { holder: None, .. }) => {}
        other => panic!("kernel guard must refuse, got {other:?}"),
    }
    drop(held);

    // Daemons restarting together after a crash race to adopt the same stale
    // record: exactly one adopts it, every other one is refused.
    repo.write_record(&LockRecord {
        pid: dead_pid(),
        boot_id: boot_id().unwrap(),
        machine_id: MACHINE.to_string(),
    });
    let barrier = Arc::new(Barrier::new(8));
    let results: Vec<Result<Acquired, LockError>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let barrier = Arc::clone(&barrier);
                let repo = &repo;
                s.spawn(move || {
                    barrier.wait();
                    acquire(repo)
                })
            })
            .collect();
        // Every winner stays held until all threads have tried.
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let winners: Vec<&Acquired> = results.iter().filter_map(|r| r.as_ref().ok()).collect();
    assert_eq!(winners.len(), 1, "{results:?}");
    let stale = winners[0].recovered.as_ref().expect("the winner adopted");
    assert_eq!(stale.reason, StaleReason::DeadPid);
    for r in &results {
        if let Err(err) = r {
            assert!(matches!(err, LockError::Held { .. }), "{err}");
        }
    }
}

/// Regression: a release must free the lock even while other threads are
/// spawning processes (a forked, not-yet-exec'd child holds copies of every
/// fd, so closing the lock file alone left `flock` held).
#[test]
fn daemon_lock_release_survives_concurrent_spawns() {
    let repo = TempRepo::new();
    let stop = std::sync::atomic::AtomicBool::new(false);
    let refused = std::thread::scope(|s| {
        for _ in 0..4 {
            s.spawn(|| {
                while !stop.load(Ordering::Relaxed) {
                    let _ = Command::new("git")
                        .arg("--version")
                        .stdout(Stdio::null())
                        .status();
                }
            });
        }
        let refused = (0..200).filter(|_| acquire(&repo).is_err()).count();
        stop.store(true, Ordering::Relaxed);
        refused
    });
    assert_eq!(refused, 0, "acquire refused right after a release");
}
