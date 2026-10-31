//! Worktree teardown pipeline and `meshtree prune` (`SPEC.md` §2 G7, §8
//! `prune`, §10 Trap 2, §14, §16 `MT-020`/`MT-021`).

mod common;

use common::{assert_exit, meshtree, spawn_task, TempDir};
use meshtree_platform::teardown::{teardown, Teardown};
use std::cell::Cell;
use std::fs;
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

fn set_status(root: &Path, id: &str, status: &str) {
    let path = root.join(".meshtree/tasks").join(id).join("task.json");
    let mut json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    json["status"] = status.into();
    fs::write(path, json.to_string()).unwrap();
}

fn stdout(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn teardown_backoff_and_prune_gc_sweep() {
    // Backoff: each failed attempt waits the next delay; success stops it.
    let tmp = TempDir::new();
    let wt = tmp.path().join("wt");
    fs::create_dir_all(wt.join("src")).unwrap();
    let delays = [Duration::from_millis(40), Duration::from_millis(80)];
    let calls = Cell::new(0);
    let start = Instant::now();
    let out = teardown(&wt, &tmp.path().join("q"), "t-x", &delays, || {
        calls.set(calls.get() + 1);
        if calls.get() < 3 {
            Err(io::Error::other("ERROR_SHARING_VIOLATION"))
        } else {
            fs::remove_dir_all(&wt)
        }
    });
    assert_eq!(out, Teardown::Removed { attempts: 3 });
    assert!(
        start.elapsed() >= Duration::from_millis(120),
        "both waits taken"
    );

    // prune: leftover worktrees of finished tasks, then the quarantine GC.
    let repo = TempDir::repo_with_commit();
    let root = repo.path();
    assert_exit(&meshtree(root, &["init"]), 0);
    let swept = spawn_task(root, "swept");
    let active = spawn_task(root, "active");
    let aborted = spawn_task(root, "aborted");
    let wt_of = |id: &str| root.join(".meshtree/worktrees").join(id);

    // A normal abort removes its worktree outright (no note).
    let out = meshtree(root, &["abort", &aborted]);
    assert_exit(&out, 0);
    assert!(!wt_of(&aborted).exists());
    assert!(String::from_utf8_lossy(&out.stderr).is_empty());

    // The daemon's sweeper marks a silent task abandoned and leaves its
    // worktree for prune.
    set_status(root, &swept, "abandoned");
    let quarantine = root.join(".meshtree/quarantine");
    let old = quarantine.join("t-old");
    fs::create_dir_all(old.join("deep/er")).unwrap();
    fs::write(old.join("deep/er/file.txt"), "x").unwrap();

    let out = meshtree(root, &["prune", "--stale-only"]);
    assert_exit(&out, 0);
    assert!(
        stdout(&out).contains(&format!("removed worktree of {swept}")),
        "{}",
        stdout(&out)
    );
    assert!(!wt_of(&swept).exists());
    assert!(wt_of(&active).exists(), "active tasks are never pruned");
    assert!(old.exists(), "--stale-only skips the quarantine");
    // git no longer lists the removed worktree.
    let listed = common::git(root, &["worktree", "list", "--porcelain"]);
    assert!(!listed.contains(&swept), "{listed}");

    // Full prune: quarantine GC.
    #[cfg(windows)]
    let locked = {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = quarantine.join("t-locked");
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("held.txt");
        fs::write(&file, "held").unwrap();
        // Exclusive handle, as an editor or indexer might hold.
        let handle = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&file)
            .unwrap();
        (dir, handle)
    };
    let out = meshtree(root, &["prune"]);
    assert_exit(&out, 0);
    assert!(
        stdout(&out).contains("removed quarantined"),
        "{}",
        stdout(&out)
    );
    assert!(!old.exists());
    #[cfg(windows)]
    {
        let (dir, handle) = locked;
        let err = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(err.contains("MT-020"), "locked tree reported: {err}");
        assert!(dir.join("held.txt").exists(), "a locked tree stays");
        drop(handle);
        assert_exit(&meshtree(root, &["prune"]), 0);
        assert!(!dir.exists(), "released lock: the next sweep removes it");
    }
    let out = meshtree(root, &["prune"]);
    assert_exit(&out, 0);
    assert!(
        stdout(&out).contains("nothing to prune"),
        "{}",
        stdout(&out)
    );
}
