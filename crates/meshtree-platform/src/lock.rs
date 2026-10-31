//! Singleton `daemon.lock` with stale-lock detection.
//!
//! Authoritative specification: `SPEC.md` §5 (`.meshtree/daemon.lock`: pid,
//! boot_id, machine_id), §10 (singleton + stale lock detection; Windows adds
//! `CreateMutex` belt-and-braces), §16 (`MT-011`), §18 (stale lock with live
//! vs dead pid → correct adoption).
//!
//! Acquisition takes two independent locks, and both must agree the lock is
//! free:
//!
//! 1. **Kernel guard** — makes acquisition atomic and is released by the OS
//!    when the holder dies (including `SIGKILL` / `TerminateProcess`). Unix:
//!    `flock(LOCK_EX | LOCK_NB)` on `daemon.lock` itself. Windows: the named
//!    mutex `Local\meshtree-<repo-hash-16>`; `ERROR_ALREADY_EXISTS` means a
//!    live process holds a handle to it.
//! 2. **Lock record** — `{"pid", "boot_id", "machine_id"}` JSON in
//!    `daemon.lock`. With the guard held, a record whose pid is alive on the
//!    current boot still refuses acquisition; a dead pid, a previous boot of
//!    this machine, or an unreadable record is a stale lock that is adopted
//!    (`MT-011`). A record from another machine and another boot cannot be
//!    probed and is refused.
//!
//! The record is rewritten in place and cleared (not deleted) on release:
//! replacing or unlinking the file would hand a second process a fresh,
//! unlocked inode while the first still holds `flock` on the old one.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Lock file name inside `.meshtree/` (`SPEC.md` §5).
pub const LOCK_FILE: &str = "daemon.lock";

/// `MT-011`: stale daemon lock, auto-recovered (`SPEC.md` §16).
pub const ERROR_CODE_STALE_LOCK: &str = "MT-011";

/// Boot ids of the form `boot-<unix secs>` within this many seconds denote the
/// same boot (Windows derives boot time from wall clock − uptime, which moves
/// with clock adjustments; Linux and macOS ids are exact).
const BOOT_TIME_TOLERANCE_SECS: u64 = 30;

/// Contents of `daemon.lock`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockRecord {
    pub pid: u32,
    pub boot_id: String,
    pub machine_id: String,
}

impl LockRecord {
    /// The record for this process on this boot.
    pub fn current(machine_id: &str) -> io::Result<Self> {
        Ok(Self {
            pid: std::process::id(),
            boot_id: boot_id()?,
            machine_id: machine_id.to_string(),
        })
    }
}

/// Why an existing record was judged stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaleReason {
    /// Same boot, holder pid no longer exists.
    DeadPid,
    /// Same machine, earlier boot: every process of that boot is gone.
    PreviousBoot,
    /// The record could not be parsed (e.g. holder died mid-write).
    Unreadable,
}

/// How a lock record relates to the current process and boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The holder may still be running: refuse.
    Live,
    /// Safe to adopt.
    Stale(StaleReason),
    /// Another machine on another boot: liveness cannot be probed, refuse.
    Foreign,
}

/// Classifies `record` against the current boot and machine; `pid_alive`
/// probes a pid on this boot. Pure, so the rules are testable without
/// processes.
pub fn assess(
    record: &LockRecord,
    current_boot: &str,
    current_machine: &str,
    pid_alive: impl Fn(u32) -> bool,
) -> Verdict {
    if same_boot(&record.boot_id, current_boot) {
        if pid_alive(record.pid) {
            Verdict::Live
        } else {
            Verdict::Stale(StaleReason::DeadPid)
        }
    } else if record.machine_id == current_machine {
        Verdict::Stale(StaleReason::PreviousBoot)
    } else {
        Verdict::Foreign
    }
}

/// `true` if two boot ids denote the same boot of the OS.
pub fn same_boot(a: &str, b: &str) -> bool {
    let secs = |id: &str| id.strip_prefix("boot-")?.parse::<u64>().ok();
    match (secs(a), secs(b)) {
        (Some(x), Some(y)) => x.abs_diff(y) <= BOOT_TIME_TOLERANCE_SECS,
        _ => a == b,
    }
}

/// A stale lock that was adopted (`MT-011`, informational).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleLock {
    /// The previous record, if it was readable.
    pub previous: Option<LockRecord>,
    pub reason: StaleReason,
}

impl StaleLock {
    /// Always `MT-011`.
    pub fn code(&self) -> &'static str {
        ERROR_CODE_STALE_LOCK
    }
}

impl fmt::Display for StaleLock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let why = match self.reason {
            StaleReason::DeadPid => "holder process is gone",
            StaleReason::PreviousBoot => "held before the last reboot",
            StaleReason::Unreadable => "lock record unreadable",
        };
        write!(
            f,
            "[{ERROR_CODE_STALE_LOCK}] stale daemon lock recovered ({why}"
        )?;
        if let Some(prev) = &self.previous {
            write!(f, "; pid {}, machine {}", prev.pid, prev.machine_id)?;
        }
        write!(f, ")")
    }
}

/// Lock acquisition failure.
#[derive(Debug)]
pub enum LockError {
    /// A daemon on this machine holds the lock. `holder` is `None` if its
    /// record was not yet written or unreadable.
    Held {
        path: PathBuf,
        holder: Option<LockRecord>,
    },
    /// The record names another machine and boot; liveness is unknowable.
    Foreign { path: PathBuf, holder: LockRecord },
    /// Filesystem or OS failure.
    Io { context: String, source: io::Error },
}

impl fmt::Display for LockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LockError::Held { path, holder } => {
                write!(f, "a meshtree daemon already holds {}", path.display())?;
                if let Some(h) = holder {
                    write!(f, " (pid {})", h.pid)?;
                }
                Ok(())
            }
            LockError::Foreign { path, holder } => write!(
                f,
                "{} is held by machine {} (pid {}), whose liveness cannot be checked \
                 from here; remove the file if that daemon is gone",
                path.display(),
                holder.machine_id,
                holder.pid
            ),
            LockError::Io { context, source } => write!(f, "{context}: {source}"),
        }
    }
}

impl std::error::Error for LockError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            LockError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// A successful acquisition.
#[derive(Debug)]
pub struct Acquired {
    pub lock: DaemonLock,
    /// `Some` if a stale lock was adopted (report as `MT-011`).
    pub recovered: Option<StaleLock>,
}

/// The held singleton lock; released (record cleared) on drop.
#[derive(Debug)]
pub struct DaemonLock {
    // Declared first so it drops (unlocks) while `file` is still open.
    _guard: sys::Guard,
    file: File,
    path: PathBuf,
    record: LockRecord,
}

impl DaemonLock {
    /// Acquires `<repo_root>/.meshtree/daemon.lock` for this process.
    pub fn acquire(repo_root: &Path, machine_id: &str) -> Result<Acquired, LockError> {
        let path = repo_root.join(".meshtree").join(LOCK_FILE);
        let io_err = |context: &str| {
            let context = format!("{context} {}", path.display());
            move |source| LockError::Io { context, source }
        };
        // std opens with FILE_SHARE_READ | WRITE | DELETE on Windows (§10).
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(io_err("open"))?;

        let Some(guard) = sys::Guard::try_acquire(&file, repo_root).map_err(io_err("lock"))? else {
            let holder = read_record(&mut file).ok().flatten();
            return Err(LockError::Held { path, holder });
        };

        let record = LockRecord::current(machine_id).map_err(io_err("read boot id for"))?;
        let recovered = match read_record(&mut file) {
            Ok(None) => None,
            Err(_) => Some(StaleLock {
                previous: None,
                reason: StaleReason::Unreadable,
            }),
            Ok(Some(prev)) => match assess(&prev, &record.boot_id, machine_id, pid_alive) {
                Verdict::Live => {
                    return Err(LockError::Held {
                        path,
                        holder: Some(prev),
                    })
                }
                Verdict::Foreign => return Err(LockError::Foreign { path, holder: prev }),
                Verdict::Stale(reason) => Some(StaleLock {
                    previous: Some(prev),
                    reason,
                }),
            },
        };

        let mut bytes = serde_json::to_vec(&record).expect("lock record serializes");
        bytes.push(b'\n');
        write_in_place(&mut file, &bytes).map_err(io_err("write"))?;
        Ok(Acquired {
            lock: DaemonLock {
                _guard: guard,
                file,
                path,
                record,
            },
            recovered,
        })
    }

    /// This holder's record.
    pub fn record(&self) -> &LockRecord {
        &self.record
    }

    /// Path of `daemon.lock`.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for DaemonLock {
    fn drop(&mut self) {
        // Clear before the guard drops so the next holder sees a clean file.
        let _ = write_in_place(&mut self.file, b"");
    }
}

/// `Ok(None)` for an empty file, `Err` for unparseable contents.
fn read_record(file: &mut File) -> io::Result<Option<LockRecord>> {
    let mut text = String::new();
    file.seek(SeekFrom::Start(0))?;
    file.read_to_string(&mut text)?;
    if text.trim().is_empty() {
        return Ok(None);
    }
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn write_in_place(file: &mut File, bytes: &[u8]) -> io::Result<()> {
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Boot identifier of the running OS instance: Linux
/// `/proc/sys/kernel/random/boot_id`; macOS `kern.boottime`; Windows boot
/// time derived from `GetTickCount64`.
pub fn boot_id() -> io::Result<String> {
    sys::boot_id()
}

/// `true` if a process with `pid` exists on this boot (Unix `kill(pid, 0)`,
/// Windows `OpenProcess` + `GetExitCodeProcess`). A pid reused by an
/// unrelated process reads as alive, which errs toward refusing the lock.
pub fn pid_alive(pid: u32) -> bool {
    sys::pid_alive(pid)
}

#[cfg(unix)]
mod sys {
    use std::fs::File;
    use std::io;
    use std::os::unix::io::AsRawFd;
    use std::path::Path;

    const LOCK_EX: i32 = 2;
    const LOCK_NB: i32 = 4;
    const LOCK_UN: i32 = 8;
    const EPERM: i32 = 1;

    extern "C" {
        fn flock(fd: i32, operation: i32) -> i32;
        fn kill(pid: i32, sig: i32) -> i32;
    }

    /// `flock` held through the lock's own file descriptor. Dropping it
    /// unlocks explicitly: the lock belongs to the open file description, and
    /// a concurrently forked child that has not yet exec'd holds a copy of
    /// every fd, so a plain `close` would leave the lock held until that
    /// child execs. The OS also releases it if the process dies.
    #[derive(Debug)]
    pub struct Guard(i32);

    impl Guard {
        pub fn try_acquire(file: &File, _repo_root: &Path) -> io::Result<Option<Self>> {
            loop {
                // SAFETY: valid open fd for the duration of the call.
                if unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) } == 0 {
                    return Ok(Some(Guard(file.as_raw_fd())));
                }
                let err = io::Error::last_os_error();
                match err.kind() {
                    io::ErrorKind::WouldBlock => return Ok(None),
                    io::ErrorKind::Interrupted => continue,
                    _ => return Err(err),
                }
            }
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            // SAFETY: the fd belongs to `DaemonLock::file`, which is still open
            // (field order); unlocking cannot invalidate it.
            unsafe { flock(self.0, LOCK_UN) };
        }
    }

    pub fn pid_alive(pid: u32) -> bool {
        // pid 0 and negative pids address process groups, never one process.
        let Ok(pid) = i32::try_from(pid) else {
            return false;
        };
        if pid == 0 {
            return false;
        }
        // SAFETY: signal 0 performs only the existence/permission check.
        let rc = unsafe { kill(pid, 0) };
        rc == 0 || io::Error::last_os_error().raw_os_error() == Some(EPERM)
    }

    #[cfg(target_os = "linux")]
    pub fn boot_id() -> io::Result<String> {
        let id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
        Ok(format!("linux-{}", id.trim()))
    }

    #[cfg(target_os = "macos")]
    pub fn boot_id() -> io::Result<String> {
        use std::ffi::c_void;

        #[repr(C)]
        struct Timeval {
            sec: i64,
            usec: i32,
        }
        extern "C" {
            fn sysctlbyname(
                name: *const std::ffi::c_char,
                oldp: *mut c_void,
                oldlenp: *mut usize,
                newp: *mut c_void,
                newlen: usize,
            ) -> i32;
        }
        let mut tv = Timeval { sec: 0, usec: 0 };
        let mut len = std::mem::size_of::<Timeval>();
        // SAFETY: `tv`/`len` describe a writable buffer of the size passed.
        let rc = unsafe {
            sysctlbyname(
                b"kern.boottime\0".as_ptr().cast(),
                (&mut tv as *mut Timeval).cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(format!("boot-{}", tv.sec))
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub fn boot_id() -> io::Result<String> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "boot id is only implemented for Linux, macOS, and Windows",
        ))
    }
}

#[cfg(windows)]
mod sys {
    use std::ffi::c_void;
    use std::fs::File;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};

    type Handle = *mut c_void;

    const ERROR_ALREADY_EXISTS: u32 = 183;
    const ERROR_ACCESS_DENIED: u32 = 5;
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const STILL_ACTIVE: u32 = 259;

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateMutexW(attributes: *const c_void, initial_owner: i32, name: *const u16) -> Handle;
        fn OpenProcess(access: u32, inherit_handle: i32, pid: u32) -> Handle;
        fn GetExitCodeProcess(process: Handle, exit_code: *mut u32) -> i32;
        fn GetTickCount64() -> u64;
        fn GetLastError() -> u32;
        fn CloseHandle(handle: Handle) -> i32;
    }

    /// Handle to the named mutex `Local\meshtree-<repo-hash-16>`. The kernel
    /// object exists exactly while some live process holds a handle, so
    /// creating it fresh means no other daemon is running.
    #[derive(Debug)]
    pub struct Guard(Handle);

    // SAFETY: a kernel handle may be used and closed from any thread.
    unsafe impl Send for Guard {}
    unsafe impl Sync for Guard {}

    impl Guard {
        pub fn try_acquire(_file: &File, repo_root: &Path) -> io::Result<Option<Self>> {
            // Same key as the pipe name (`ipc::Endpoint::for_repo`).
            let canonical = std::fs::canonicalize(repo_root)?;
            let key = canonical.to_string_lossy().to_lowercase();
            let hash = meshtree_core::lease::coord::sha1_hex(key.as_bytes());
            let name: Vec<u16> = std::ffi::OsStr::new(&format!(r"Local\meshtree-{}", &hash[..16]))
                .encode_wide()
                .chain(Some(0))
                .collect();
            // SAFETY: `name` is NUL-terminated; null attributes = defaults.
            let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: no intervening call can reset the thread's last error.
            if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
                // SAFETY: `handle` is a valid handle we own.
                unsafe { CloseHandle(handle) };
                return Ok(None);
            }
            Ok(Some(Guard(handle)))
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            // SAFETY: `self.0` is a valid handle we own.
            unsafe { CloseHandle(self.0) };
        }
    }

    pub fn pid_alive(pid: u32) -> bool {
        // SAFETY: plain Win32 calls; the handle is closed before returning.
        unsafe {
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if process.is_null() {
                // Exists but owned by someone we may not inspect.
                return GetLastError() == ERROR_ACCESS_DENIED;
            }
            let mut code = 0u32;
            let ok = GetExitCodeProcess(process, &mut code);
            CloseHandle(process);
            ok != 0 && code == STILL_ACTIVE
        }
    }

    pub fn boot_id() -> io::Result<String> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_secs();
        // SAFETY: no arguments, cannot fail.
        let uptime = unsafe { GetTickCount64() } / 1000;
        Ok(format!("boot-{}", now.saturating_sub(uptime)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(pid: u32, boot: &str, machine: &str) -> LockRecord {
        LockRecord {
            pid,
            boot_id: boot.to_string(),
            machine_id: machine.to_string(),
        }
    }

    #[test]
    fn assess_rules() {
        let alive = |pid: u32| pid == 42;
        let boot = "linux-aaaa";
        assert_eq!(
            assess(&rec(42, boot, "mc-a"), boot, "mc-a", alive),
            Verdict::Live
        );
        // A config-renamed machine on the same boot is still probed by pid.
        assert_eq!(
            assess(&rec(42, boot, "mc-old"), boot, "mc-a", alive),
            Verdict::Live
        );
        assert_eq!(
            assess(&rec(7, boot, "mc-a"), boot, "mc-a", alive),
            Verdict::Stale(StaleReason::DeadPid)
        );
        // Earlier boot of this machine: stale even if the pid number is live now.
        assert_eq!(
            assess(&rec(42, "linux-bbbb", "mc-a"), boot, "mc-a", alive),
            Verdict::Stale(StaleReason::PreviousBoot)
        );
        assert_eq!(
            assess(&rec(42, "linux-bbbb", "mc-b"), boot, "mc-a", alive),
            Verdict::Foreign
        );
    }

    #[test]
    fn boot_time_ids_tolerate_clock_adjustment() {
        assert!(same_boot("boot-1000", "boot-1000"));
        assert!(same_boot("boot-1000", "boot-1030"));
        assert!(same_boot("boot-1030", "boot-1000"));
        assert!(!same_boot("boot-1000", "boot-1031"));
        assert!(!same_boot("boot-1000", "linux-1000"));
        assert!(same_boot("linux-x", "linux-x"));
        assert!(!same_boot("linux-x", "linux-y"));
    }

    #[test]
    fn current_process_probes() {
        assert!(pid_alive(std::process::id()));
        assert!(!pid_alive(0));
        let id = boot_id().unwrap();
        assert!(!id.is_empty());
        assert!(
            same_boot(&boot_id().unwrap(), &id),
            "boot id is stable within a boot"
        );
        let display = StaleLock {
            previous: Some(rec(9, &id, "mc-a")),
            reason: StaleReason::DeadPid,
        }
        .to_string();
        assert!(
            display.starts_with("[MT-011]") && display.contains("pid 9"),
            "{display}"
        );
    }
}
