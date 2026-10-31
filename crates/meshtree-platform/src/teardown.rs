//! Worktree teardown that never fails a task: retry with backoff, then
//! quarantine, then defer.
//!
//! Authoritative specification: `SPEC.md` §2 (G7), §10 (Trap 2: Windows
//! mandatory locking; pre-open discipline, retry 250 ms → 8 s, same-volume
//! move to `.meshtree/quarantine/<task>/`, idle GC sweep, last-resort
//! `MOVEFILE_DELAY_UNTIL_REBOOT`), §16 (`MT-020`, `MT-021`).
//!
//! Pre-open discipline: MeshTree opens its files through `std`, whose
//! Windows default share mode is `FILE_SHARE_READ | FILE_SHARE_WRITE |
//! FILE_SHARE_DELETE`, so MeshTree's own handles never block a teardown.
//!
//! [`teardown`] runs the caller's removal (e.g. `git worktree remove`) with
//! the [`BACKOFF`] schedule. If every attempt fails, the tree is renamed into
//! the quarantine directory (`MT-021`). If even the rename fails (on Windows
//! an exclusively opened file blocks moving its ancestors), everything
//! deletable is deleted, the rest is registered for deletion at reboot where
//! the OS allows it, and the outcome is [`Teardown::Deferred`] (`MT-020`).
//! None of these is an error: cleanup failure must not fail a merge.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// `MT-020`: file locked by another process (`SPEC.md` §16).
pub const ERROR_CODE_FILE_LOCKED: &str = "MT-020";
/// `MT-021`: worktree quarantined; cleanup deferred, work is safe.
pub const ERROR_CODE_QUARANTINED: &str = "MT-021";

/// `SPEC.md` §10: 250 ms → 8 s, six waits (an attempt before each, and one
/// after the last: seven attempts, ~16 s worst case).
pub const BACKOFF: [Duration; 6] = [
    Duration::from_millis(250),
    Duration::from_millis(500),
    Duration::from_millis(1_000),
    Duration::from_millis(2_000),
    Duration::from_millis(4_000),
    Duration::from_millis(8_000),
];

/// How a teardown ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Teardown {
    /// Removed after `attempts` tries.
    Removed { attempts: usize },
    /// Moved to `to` after every removal attempt failed (`MT-021`).
    Quarantined { to: PathBuf, error: String },
    /// Could not be removed or moved; whatever was deletable is gone and the
    /// rest stays at `path` (`MT-020`).
    Deferred {
        path: PathBuf,
        error: String,
        /// Entries registered for deletion at the next reboot.
        reboot_registered: usize,
    },
}

impl Teardown {
    /// The §16 code to report, if the tree was not simply removed.
    pub fn code(&self) -> Option<&'static str> {
        match self {
            Teardown::Removed { .. } => None,
            Teardown::Quarantined { .. } => Some(ERROR_CODE_QUARANTINED),
            Teardown::Deferred { .. } => Some(ERROR_CODE_FILE_LOCKED),
        }
    }
}

impl fmt::Display for Teardown {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Teardown::Removed { attempts } => write!(f, "removed ({attempts} attempt(s))"),
            Teardown::Quarantined { to, error } => write!(
                f,
                "[{ERROR_CODE_QUARANTINED}] worktree quarantined to {} ({error}); \
                 cleanup deferred, work is safe",
                to.display()
            ),
            Teardown::Deferred {
                path,
                error,
                reboot_registered,
            } => write!(
                f,
                "[{ERROR_CODE_FILE_LOCKED}] {} is locked by another process ({error}); \
                 left in place for `meshtree prune`{}",
                path.display(),
                if *reboot_registered > 0 {
                    format!(", {reboot_registered} entries scheduled for deletion at reboot")
                } else {
                    String::new()
                }
            ),
        }
    }
}

/// Removes `worktree` via `remove`, retrying after each delay in `backoff`;
/// then quarantines it as `quarantine_dir/<name>`; then defers.
pub fn teardown(
    worktree: &Path,
    quarantine_dir: &Path,
    name: &str,
    backoff: &[Duration],
    mut remove: impl FnMut() -> io::Result<()>,
) -> Teardown {
    let mut last_error = String::new();
    for attempt in 0..=backoff.len() {
        match remove() {
            Ok(()) => {
                return Teardown::Removed {
                    attempts: attempt + 1,
                }
            }
            // Gone anyway (e.g. a partial removal finished by someone else).
            Err(_) if fs::symlink_metadata(worktree).is_err() => {
                return Teardown::Removed {
                    attempts: attempt + 1,
                }
            }
            Err(err) => last_error = err.to_string(),
        }
        if let Some(delay) = backoff.get(attempt) {
            std::thread::sleep(*delay);
        }
    }
    quarantine(
        worktree,
        quarantine_dir,
        name,
        last_error,
        backoff.len() + 1,
    )
}

/// Moves `worktree` to `quarantine_dir/<name>` (a unique name if taken); if
/// that fails, deletes what it can and defers the rest. `attempts` is the
/// number of removal attempts already made (reported if the fallback delete
/// clears everything).
pub fn quarantine(
    worktree: &Path,
    quarantine_dir: &Path,
    name: &str,
    error: String,
    attempts: usize,
) -> Teardown {
    let target = unique(quarantine_dir, name);
    let moved = fs::create_dir_all(quarantine_dir).and_then(|()| fs::rename(worktree, &target));
    match moved {
        Ok(()) => Teardown::Quarantined { to: target, error },
        Err(move_err) => {
            delete_what_we_can(worktree);
            if fs::symlink_metadata(worktree).is_err() {
                return Teardown::Removed { attempts };
            }
            let reboot_registered = sys::delete_at_reboot(worktree);
            Teardown::Deferred {
                path: worktree.to_path_buf(),
                error: format!("{error}; move to quarantine failed: {move_err}"),
                reboot_registered,
            }
        }
    }
}

fn unique(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if fs::symlink_metadata(&first).is_err() {
        return first;
    }
    (1..)
        .map(|n| dir.join(format!("{name}.{n}")))
        .find(|p| fs::symlink_metadata(p).is_err())
        .expect("an unused name exists")
}

/// Post-order delete that skips (and keeps) anything that cannot be removed.
/// Never follows symlinks or junctions.
fn delete_what_we_can(path: &Path) {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return;
    };
    if meta.is_dir() && !meta.file_type().is_symlink() {
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                delete_what_we_can(&entry.path());
            }
        }
        let _ = fs::remove_dir(path);
    } else {
        // Files, and links (a Windows directory symlink or junction is
        // removed with RemoveDirectory, never following it).
        let _ = fs::remove_file(path).or_else(|_| fs::remove_dir(path));
    }
}

/// Result of a [`gc_quarantine`] sweep.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GcReport {
    /// Quarantined trees deleted.
    pub removed: Vec<PathBuf>,
    /// Trees still locked (left for the next sweep), with the error.
    pub locked: Vec<(PathBuf, String)>,
}

/// Retries deleting every tree in `quarantine_dir` (§10 step 4: the daemon
/// calls this at idle; `meshtree prune` calls it on demand). Trees that are
/// still locked are left, minus whatever could be deleted.
pub fn gc_quarantine(quarantine_dir: &Path) -> GcReport {
    let mut report = GcReport::default();
    let Ok(entries) = fs::read_dir(quarantine_dir) else {
        return report;
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        delete_what_we_can(&path);
        match fs::symlink_metadata(&path) {
            Err(_) => report.removed.push(path),
            Ok(_) => {
                let error = fs::remove_dir_all(&path)
                    .err()
                    .map_or_else(|| "in use".to_string(), |e| e.to_string());
                if fs::symlink_metadata(&path).is_err() {
                    report.removed.push(path);
                } else {
                    report.locked.push((path, error));
                }
            }
        }
    }
    report
}

#[cfg(not(windows))]
mod sys {
    use std::path::Path;

    /// Unix deletes open files freely; nothing to schedule.
    pub fn delete_at_reboot(_path: &Path) -> usize {
        0
    }
}

#[cfg(windows)]
mod sys {
    use std::fs;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    const MOVEFILE_DELAY_UNTIL_REBOOT: u32 = 0x4;

    extern "system" {
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }

    fn schedule(path: &Path) -> bool {
        let mut name: Vec<u16> = path.as_os_str().encode_wide().collect();
        name.push(0);
        // SAFETY: NUL-terminated path; a null destination means "delete".
        unsafe { MoveFileExW(name.as_ptr(), std::ptr::null(), MOVEFILE_DELAY_UNTIL_REBOOT) != 0 }
    }

    /// Registers every remaining entry (files first, then directories,
    /// deepest first) for deletion at reboot. Needs administrator rights;
    /// returns how many entries were registered (0 when not permitted).
    pub fn delete_at_reboot(path: &Path) -> usize {
        let mut count = 0;
        if let Ok(meta) = fs::symlink_metadata(path) {
            if meta.is_dir() && !meta.file_type().is_symlink() {
                if let Ok(entries) = fs::read_dir(path) {
                    for entry in entries.flatten() {
                        count += delete_at_reboot(&entry.path());
                    }
                }
            }
            if schedule(path) {
                count += 1;
            }
        }
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mt-teardown-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn tree(at: &Path) {
        fs::create_dir_all(at.join("src/deep")).unwrap();
        fs::write(at.join("src/deep/a.txt"), "a").unwrap();
        fs::write(at.join("b.txt"), "b").unwrap();
    }

    const FAST: [Duration; 3] = [Duration::from_millis(1); 3];

    #[test]
    fn retries_until_removal_succeeds() {
        let dir = temp("retry");
        let wt = dir.join("wt");
        tree(&wt);
        let calls = Cell::new(0);
        let out = teardown(&wt, &dir.join("q"), "t-1", &FAST, || {
            calls.set(calls.get() + 1);
            if calls.get() < 3 {
                Err(io::Error::other("locked"))
            } else {
                fs::remove_dir_all(&wt)
            }
        });
        assert_eq!(out, Teardown::Removed { attempts: 3 });
        assert_eq!(out.code(), None);
        assert!(!wt.exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn persistent_failure_quarantines_then_defers() {
        let dir = temp("quarantine");
        let (wt, q) = (dir.join("wt"), dir.join("q"));
        tree(&wt);
        let calls = Cell::new(0);
        let out = teardown(&wt, &q, "t-1", &FAST, || {
            calls.set(calls.get() + 1);
            Err(io::Error::other("sharing violation"))
        });
        assert_eq!(
            calls.get(),
            FAST.len() + 1,
            "one attempt per wait, plus one"
        );
        let Teardown::Quarantined { to, .. } = &out else {
            panic!("{out:?}")
        };
        assert_eq!(to, &q.join("t-1"));
        assert!(!wt.exists() && to.join("src/deep/a.txt").exists());
        assert_eq!(out.code(), Some(ERROR_CODE_QUARANTINED));
        assert!(out.to_string().contains("MT-021"));

        // A second tree with the same task name gets a unique slot.
        tree(&wt);
        let out = teardown(&wt, &q, "t-1", &[], || Err(io::Error::other("x")));
        assert!(matches!(&out, Teardown::Quarantined { to, .. } if to == &q.join("t-1.1")));

        // The quarantine directory cannot be created (a file is in the way):
        // delete what we can, never error. Nothing here is locked, so the
        // fallback delete clears it all (a real lock yields MT-020 Deferred,
        // covered on Windows by T-025).
        tree(&wt);
        let blocked = dir.join("blocked");
        fs::write(&blocked, "file, not a dir").unwrap();
        let out = teardown(&wt, &blocked, "t-1", &[], || Err(io::Error::other("x")));
        assert_eq!(out, Teardown::Removed { attempts: 1 });
        assert!(!wt.exists());

        // GC empties the quarantine.
        let gc = gc_quarantine(&q);
        assert_eq!(gc.removed.len(), 2);
        assert!(gc.locked.is_empty());
        assert_eq!(fs::read_dir(&q).unwrap().count(), 0);
        assert_eq!(gc_quarantine(&dir.join("missing")), GcReport::default());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn backoff_schedule_matches_spec() {
        assert_eq!(BACKOFF.first(), Some(&Duration::from_millis(250)));
        assert_eq!(BACKOFF.last(), Some(&Duration::from_secs(8)));
        assert!(BACKOFF.windows(2).all(|w| w[1] == w[0] * 2));
    }
}
