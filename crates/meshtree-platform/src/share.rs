//! Dependency sharing (`[[share]]`): link a primary-worktree directory into a
//! task worktree.
//!
//! Authoritative specification: `SPEC.md` §2 (G6), §6 (`[[share]]`), §11
//! (Trap 3: junctions on Windows, not symlinks; `clonefile` for read-only
//! caches; link creation < 100 ms per directory), §16 (`MT-040`: share-link
//! failed, strategy fell back to `none`).
//!
//! | strategy    | effect                                                            |
//! |-------------|-------------------------------------------------------------------|
//! | `auto`      | `junction` on Windows, `symlink` elsewhere                        |
//! | `junction`  | NTFS mount-point reparse point (no admin, no Developer Mode); Windows only |
//! | `clonefile` | APFS copy-on-write clone of the whole tree (`clonefile(2)`); macOS only |
//! | `symlink`   | directory symlink (needs Developer Mode on Windows)               |
//! | `none`      | nothing shared                                                    |
//!
//! A failed or unsupported strategy never fails `spawn`: anything partially
//! created is removed and the outcome is [`Applied::Fallback`] (`MT-040`),
//! i.e. the worktree simply has no shared copy.

use meshtree_core::{ShareConfig, ShareStrategy};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// `MT-040`: share-link failed, strategy fell back to `none` (`SPEC.md` §16).
pub const ERROR_CODE_SHARE_FAILED: &str = "MT-040";

/// A link mechanism.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkKind {
    Junction,
    Clonefile,
    Symlink,
}

impl fmt::Display for LinkKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            LinkKind::Junction => "junction",
            LinkKind::Clonefile => "clonefile",
            LinkKind::Symlink => "symlink",
        })
    }
}

/// What happened to one `[[share]]` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applied {
    /// The worktree now has `source` via this mechanism.
    Linked(LinkKind),
    /// Nothing to do: `none`, or the source does not exist in the primary
    /// worktree, or the worktree already has that path.
    Skipped(String),
    /// The strategy failed; nothing was created (`MT-040`).
    Fallback {
        requested: ShareStrategy,
        error: String,
    },
}

/// Outcome of [`apply`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareOutcome {
    /// The configured relative source.
    pub source: String,
    /// Destination inside the task worktree.
    pub link: PathBuf,
    pub applied: Applied,
    /// Time spent creating the link.
    pub elapsed: Duration,
}

impl ShareOutcome {
    /// `MT-040` for a fallback.
    pub fn code(&self) -> Option<&'static str> {
        matches!(self.applied, Applied::Fallback { .. }).then_some(ERROR_CODE_SHARE_FAILED)
    }
}

impl fmt::Display for ShareOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.applied {
            Applied::Linked(kind) => write!(f, "shared {} ({kind})", self.source),
            Applied::Skipped(why) => write!(f, "not shared {}: {why}", self.source),
            Applied::Fallback { requested, error } => write!(
                f,
                "[{ERROR_CODE_SHARE_FAILED}] share {} via {requested} failed, fell back to none: {error}",
                self.source
            ),
        }
    }
}

/// The concrete mechanism `strategy` means on this OS (`None` for `none`).
pub fn resolve(strategy: ShareStrategy) -> Option<LinkKind> {
    match strategy {
        ShareStrategy::Auto if cfg!(windows) => Some(LinkKind::Junction),
        ShareStrategy::Auto => Some(LinkKind::Symlink),
        ShareStrategy::Junction => Some(LinkKind::Junction),
        ShareStrategy::Clonefile => Some(LinkKind::Clonefile),
        ShareStrategy::Symlink => Some(LinkKind::Symlink),
        ShareStrategy::None => None,
    }
}

/// Shares `share.source` from `primary` into `worktree`.
pub fn apply(primary: &Path, worktree: &Path, share: &ShareConfig) -> ShareOutcome {
    let start = Instant::now();
    let source = primary.join(&share.source);
    let link = worktree.join(&share.source);
    let applied = match resolve(share.strategy) {
        None => Applied::Skipped("strategy none".to_string()),
        Some(_) if !source.is_dir() => {
            Applied::Skipped("source directory does not exist in the primary worktree".into())
        }
        Some(_) if fs::symlink_metadata(&link).is_ok() => {
            Applied::Skipped("the worktree already has this path".into())
        }
        Some(kind) => match create(kind, &source, &link) {
            Ok(()) => Applied::Linked(kind),
            Err(err) => {
                let _ = remove_partial(kind, &link);
                Applied::Fallback {
                    requested: share.strategy,
                    error: err.to_string(),
                }
            }
        },
    };
    ShareOutcome {
        source: share.source.clone(),
        link,
        applied,
        elapsed: start.elapsed(),
    }
}

/// [`apply`] for every entry, in order.
pub fn apply_all(primary: &Path, worktree: &Path, shares: &[ShareConfig]) -> Vec<ShareOutcome> {
    shares.iter().map(|s| apply(primary, worktree, s)).collect()
}

fn create(kind: LinkKind, source: &Path, link: &Path) -> io::Result<()> {
    if let Some(parent) = link.parent() {
        fs::create_dir_all(parent)?;
    }
    match kind {
        LinkKind::Symlink => symlink_dir(source, link),
        LinkKind::Junction => sys::junction(source, link),
        LinkKind::Clonefile => sys::clonefile(source, link),
    }
}

fn remove_partial(kind: LinkKind, link: &Path) -> io::Result<()> {
    match kind {
        LinkKind::Clonefile => fs::remove_dir_all(link),
        LinkKind::Junction | LinkKind::Symlink => unlink(link).map(drop),
    }
}

#[cfg(unix)]
fn symlink_dir(source: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(source, link)
}

#[cfg(windows)]
fn symlink_dir(source: &Path, link: &Path) -> io::Result<()> {
    std::os::windows::fs::symlink_dir(source, link)
}

/// Removes `path` if it is a symlink or junction, never following it (the
/// shared target is untouched). Returns `false` if `path` is absent or not a
/// link. Call before deleting a worktree so a recursive delete cannot reach
/// the primary worktree's directory through the link.
pub fn unlink(path: &Path) -> io::Result<bool> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err),
    };
    if !meta.file_type().is_symlink() {
        return Ok(false);
    }
    // Windows directory symlinks and junctions are directories to the
    // filesystem (RemoveDirectory); Unix symlinks are files (unlink).
    #[cfg(windows)]
    fs::remove_dir(path)?;
    #[cfg(not(windows))]
    fs::remove_file(path)?;
    Ok(true)
}

/// [`unlink`] for every configured share destination in `worktree`.
pub fn unlink_all(worktree: &Path, shares: &[ShareConfig]) -> io::Result<()> {
    for share in shares {
        unlink(&worktree.join(&share.source))?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
mod sys {
    use std::ffi::CString;
    use std::io;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    extern "C" {
        #[link_name = "clonefile"]
        fn clonefile_sys(
            src: *const std::ffi::c_char,
            dst: *const std::ffi::c_char,
            flags: u32,
        ) -> i32;
    }

    pub fn clonefile(source: &Path, link: &Path) -> io::Result<()> {
        let c = |p: &Path| {
            CString::new(p.as_os_str().as_bytes())
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))
        };
        let (src, dst) = (c(source)?, c(link)?);
        // SAFETY: both pointers are valid NUL-terminated paths for the call.
        if unsafe { clonefile_sys(src.as_ptr(), dst.as_ptr(), 0) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    pub fn junction(_source: &Path, _link: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "junctions exist only on Windows (NTFS)",
        ))
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod sys {
    use std::io;
    use std::path::Path;

    pub fn clonefile(_source: &Path, _link: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "clonefile is only available on macOS (APFS)",
        ))
    }

    pub fn junction(_source: &Path, _link: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "junctions exist only on Windows (NTFS)",
        ))
    }
}

#[cfg(windows)]
mod sys {
    use std::ffi::c_void;
    use std::fs;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    type Handle = *mut c_void;
    const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const OPEN_EXISTING: u32 = 3;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FSCTL_SET_REPARSE_POINT: u32 = 0x0009_00A4;
    const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;

    extern "system" {
        fn CreateFileW(
            name: *const u16,
            access: u32,
            share: u32,
            security: *const c_void,
            disposition: u32,
            flags: u32,
            template: Handle,
        ) -> Handle;
        fn DeviceIoControl(
            device: Handle,
            code: u32,
            in_buf: *const c_void,
            in_size: u32,
            out_buf: *mut c_void,
            out_size: u32,
            returned: *mut u32,
            overlapped: *mut c_void,
        ) -> i32;
        fn CloseHandle(handle: Handle) -> i32;
    }

    fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
        s.encode_wide().collect()
    }

    /// Creates `link` as an NTFS junction (mount point) to `source`.
    pub fn junction(source: &Path, link: &Path) -> io::Result<()> {
        // Absolute target without the `\\?\` verbatim prefix.
        let target = fs::canonicalize(source)?;
        let mut target = wide(target.as_os_str());
        if target.starts_with(&wide("\\\\?\\".as_ref())) {
            target.drain(..4);
        }
        let mut substitute = wide("\\??\\".as_ref());
        substitute.extend_from_slice(&target);
        let print = target;

        // REPARSE_DATA_BUFFER (MountPointReparseBuffer).
        let (sub_bytes, print_bytes) = (substitute.len() * 2, print.len() * 2);
        let path_bytes = sub_bytes + 2 + print_bytes + 2;
        let data_len = 8 + path_bytes;
        let to_u16 = |n: usize| {
            u16::try_from(n).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "junction target path too long")
            })
        };
        let mut buf: Vec<u8> = Vec::with_capacity(8 + data_len);
        buf.extend_from_slice(&IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
        buf.extend_from_slice(&to_u16(data_len)?.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes()); // SubstituteNameOffset
        buf.extend_from_slice(&to_u16(sub_bytes)?.to_le_bytes());
        buf.extend_from_slice(&to_u16(sub_bytes + 2)?.to_le_bytes()); // PrintNameOffset
        buf.extend_from_slice(&to_u16(print_bytes)?.to_le_bytes());
        for unit in substitute
            .iter()
            .chain(&[0])
            .chain(print.iter())
            .chain(&[0])
        {
            buf.extend_from_slice(&unit.to_le_bytes());
        }

        fs::create_dir(link)?;
        let mut name = wide(link.as_os_str());
        name.push(0);
        // SAFETY: `name` is NUL-terminated; the handle is closed below.
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_WRITE,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let mut returned = 0u32;
        // SAFETY: `buf` is a complete REPARSE_DATA_BUFFER of the given size.
        let ok = unsafe {
            DeviceIoControl(
                handle,
                FSCTL_SET_REPARSE_POINT,
                buf.as_ptr().cast(),
                buf.len() as u32,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        let err = io::Error::last_os_error();
        // SAFETY: valid handle from CreateFileW.
        unsafe { CloseHandle(handle) };
        if ok == 0 {
            return Err(err);
        }
        Ok(())
    }

    pub fn clonefile(_source: &Path, _link: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "clonefile is only available on macOS (APFS)",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_resolves_per_platform() {
        let expected = if cfg!(windows) {
            LinkKind::Junction
        } else {
            LinkKind::Symlink
        };
        assert_eq!(resolve(ShareStrategy::Auto), Some(expected));
        assert_eq!(resolve(ShareStrategy::None), None);
        assert_eq!(resolve(ShareStrategy::Clonefile), Some(LinkKind::Clonefile));
    }
}
