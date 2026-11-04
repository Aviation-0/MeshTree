//! `GitOps::fetch_ref` in one network round trip (`T-055`; `SPEC.md` §9,
//! §18; `ADR-0001`).
//!
//! Its own test binary: it sets `GIT_TRACE` for the whole process to count
//! how many times git contacts the remote (one `upload-pack` per round trip
//! over `file://`), which would leak into other tests in a shared binary.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use meshtree_platform::gitops::{CliGitOps, GitOps};

const COORD: &str = "refs/meshtree/coord";
const TRACKING: &str = "refs/meshtree/remotes/origin/coord";

struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "meshtree-gitops-fetch-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TRACE", "0")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Runs `f` with a fresh trace file; returns its result and the number of
/// times git started `upload-pack` (one per round trip to the remote).
fn round_trips<T>(trace: &Path, f: impl FnOnce() -> T) -> (T, usize) {
    fs::write(trace, "").unwrap();
    let result = f();
    let text = fs::read_to_string(trace).unwrap_or_default();
    let count = text
        .lines()
        .filter(|l| l.contains("run_command") && l.contains("upload-pack"))
        .count();
    (result, count)
}

#[test]
fn gitops_fetch_ref_single_round_trip_absent_and_present() {
    let scratch = Dir::new("scratch");
    let remote = scratch.0.join("remote.git");
    let local = scratch.0.join("local");
    git(
        &scratch.0,
        &["init", "-q", "--bare", remote.to_str().unwrap()],
    );
    git(&scratch.0, &["init", "-q", local.to_str().unwrap()]);
    // `file:///C:/…` on Windows.
    let path = remote.to_string_lossy().replace('\\', "/");
    let url = if path.starts_with('/') {
        format!("file://{path}")
    } else {
        format!("file:///{path}")
    };
    git(&local, &["remote", "add", "origin", &url]);
    git(&local, &["commit", "-q", "--allow-empty", "-m", "one"]);
    let first = git(&local, &["rev-parse", "HEAD"]);
    git(&local, &["commit", "-q", "--allow-empty", "-m", "two"]);
    let second = git(&local, &["rev-parse", "HEAD"]);

    let trace = scratch.0.join("trace.txt");
    std::env::set_var("GIT_TRACE", &trace);
    let ops = CliGitOps::new(&local);

    // Absent on the remote: None, one round trip, no tracking ref.
    let (tip, trips) = round_trips(&trace, || ops.fetch_ref("origin", COORD, TRACKING));
    assert_eq!(tip.unwrap(), None);
    assert_eq!(trips, 1, "absent ref: one round trip");
    assert_eq!(ops.ref_read(TRACKING).unwrap(), None);

    // Present: its OID, one round trip. A longer ref name the glob also
    // matches does not change what is read back.
    git(
        &local,
        &["push", "-q", "origin", &format!("{first}:{COORD}")],
    );
    git(
        &local,
        &["push", "-q", "origin", &format!("{second}:{COORD}-other")],
    );
    let (tip, trips) = round_trips(&trace, || ops.fetch_ref("origin", COORD, TRACKING));
    assert_eq!(tip.unwrap().as_deref(), Some(first.as_str()));
    assert_eq!(trips, 1, "present ref: one round trip");

    // Moved: forced update to the new tip.
    git(
        &local,
        &["push", "-q", "-f", "origin", &format!("{second}:{COORD}")],
    );
    let (tip, _) = round_trips(&trace, || ops.fetch_ref("origin", COORD, TRACKING));
    assert_eq!(tip.unwrap().as_deref(), Some(second.as_str()));

    // Deleted on the remote: None, and the stale tracking ref is pruned.
    git(&local, &["push", "-q", "origin", &format!(":{COORD}")]);
    let (tip, trips) = round_trips(&trace, || ops.fetch_ref("origin", COORD, TRACKING));
    assert_eq!(tip.unwrap(), None);
    assert_eq!(trips, 1, "deleted ref: one round trip");
    assert_eq!(ops.ref_read(TRACKING).unwrap(), None);

    // An unreachable remote is an error, not "absent".
    git(
        &local,
        &["remote", "add", "gone", "file:///nonexistent/meshtree.git"],
    );
    assert!(ops.fetch_ref("gone", COORD, TRACKING).is_err());
    std::env::remove_var("GIT_TRACE");
}
