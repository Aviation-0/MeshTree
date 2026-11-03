//! §17 daemon endpoint access control (`SPEC.md` §17, §10 IPC table).
//!
//! Unix: the socket's directory must belong to the daemon user and not be
//! group/world-writable, else `bind` is refused (another user could swap the
//! socket). Windows: the pipe's security descriptor, read back from a live
//! pipe, grants access to the owner and SYSTEM only. Each half is compiled
//! for its platform, so every CI OS runs exactly one of the two.

#![allow(dead_code)]

use meshtree_platform::ipc::{Endpoint, IpcListener};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

struct TempRepo(PathBuf);

static COUNTER: AtomicU64 = AtomicU64::new(0);

impl TempRepo {
    fn new() -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "mt-sec-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(path.join(".meshtree")).expect("create temp repo");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempRepo {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ =
                fs::set_permissions(self.0.join(".meshtree"), fs::Permissions::from_mode(0o755));
        }
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(unix)]
#[test]
fn security_unix_socket_parent_dir_rejected() {
    use std::io::ErrorKind;
    use std::os::unix::fs::PermissionsExt;

    let repo = TempRepo::new();
    let dir = repo.path().join(".meshtree");
    let endpoint = Endpoint::for_repo(repo.path()).unwrap();
    assert!(
        !endpoint.is_fallback(),
        "test needs the in-repo socket path"
    );

    // Group- or world-writable socket directory: bind refused, no socket left.
    for mode in [0o777, 0o775, 0o757, 0o722] {
        fs::set_permissions(&dir, fs::Permissions::from_mode(mode)).unwrap();
        let err = IpcListener::bind(&endpoint)
            .err()
            .unwrap_or_else(|| panic!("bind must be refused with a {mode:o} socket directory"));
        assert_eq!(err.kind(), ErrorKind::PermissionDenied, "{mode:o}: {err}");
        assert!(
            !endpoint.path().exists(),
            "{mode:o}: refused socket must be removed"
        );
    }

    // Owner-only directory: bind succeeds and the socket is 0600.
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    let listener = IpcListener::bind(&endpoint).expect("bind with a safe directory");
    let mode = fs::metadata(endpoint.path()).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    drop(listener);
}

#[cfg(windows)]
#[test]
fn security_windows_pipe_dacl_owner_system_only() {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;

    type Handle = *mut c_void;
    const READ_CONTROL: u32 = 0x0002_0000;
    const OPEN_EXISTING: u32 = 3;
    const SE_KERNEL_OBJECT: u32 = 6;
    const OWNER_SECURITY_INFORMATION: u32 = 0x1;
    const DACL_SECURITY_INFORMATION: u32 = 0x4;
    const SDDL_REVISION_1: u32 = 1;
    const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;

    #[link(name = "kernel32")]
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
        fn CloseHandle(handle: Handle) -> i32;
        fn LocalFree(mem: *mut c_void) -> *mut c_void;
    }
    #[link(name = "advapi32")]
    extern "system" {
        fn GetSecurityInfo(
            handle: Handle,
            object_type: u32,
            info: u32,
            owner: *mut *mut c_void,
            group: *mut *mut c_void,
            dacl: *mut *mut c_void,
            sacl: *mut *mut c_void,
            descriptor: *mut *mut c_void,
        ) -> u32;
        fn ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor: *mut c_void,
            revision: u32,
            info: u32,
            string: *mut *mut u16,
            len: *mut u32,
        ) -> i32;
    }

    let repo = TempRepo::new();
    let endpoint = Endpoint::for_repo(repo.path()).unwrap();
    let listener = IpcListener::bind(&endpoint).expect("bind pipe");

    // Open a client handle with READ_CONTROL only and read the descriptor.
    let name: Vec<u16> = endpoint
        .path()
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    // SAFETY: NUL-terminated name; plain Win32 calls; every allocation and
    // handle is released before the block ends.
    let sddl = unsafe {
        let handle = CreateFileW(
            name.as_ptr(),
            READ_CONTROL,
            0,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        );
        assert!(
            handle != INVALID_HANDLE_VALUE,
            "open pipe: {}",
            std::io::Error::last_os_error()
        );
        let mut descriptor: *mut c_void = std::ptr::null_mut();
        let rc = GetSecurityInfo(
            handle,
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut descriptor,
        );
        CloseHandle(handle);
        assert_eq!(rc, 0, "GetSecurityInfo failed: {rc}");
        let mut text: *mut u16 = std::ptr::null_mut();
        let mut len = 0u32;
        let ok = ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor,
            SDDL_REVISION_1,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut text,
            &mut len,
        );
        LocalFree(descriptor);
        assert!(
            ok != 0,
            "SDDL conversion: {}",
            std::io::Error::last_os_error()
        );
        let mut n = 0;
        while *text.add(n) != 0 {
            n += 1;
        }
        let sddl = String::from_utf16_lossy(std::slice::from_raw_parts(text, n));
        LocalFree(text.cast());
        sddl
    };
    drop(listener);

    // "O:<owner>D:P(A;;<rights>;;;<trustee>)…"
    let owner = sddl
        .strip_prefix("O:")
        .and_then(|rest| rest.split("D:").next())
        .unwrap_or_else(|| panic!("no owner in {sddl}"));
    let dacl = sddl
        .split("D:")
        .nth(1)
        .unwrap_or_else(|| panic!("no DACL in {sddl}"));
    assert!(
        dacl.starts_with('P'),
        "DACL must be protected (no inherited ACEs): {sddl}"
    );
    let aces: Vec<Vec<&str>> = dacl
        .split('(')
        .skip(1)
        .map(|ace| ace.trim_end_matches(')').split(';').collect())
        .collect();
    assert_eq!(aces.len(), 2, "exactly two ACEs (owner, SYSTEM): {sddl}");
    let mut trustees: Vec<&str> = Vec::new();
    for ace in &aces {
        assert_eq!(ace.len(), 6, "{sddl}");
        assert_eq!(ace[0], "A", "only allow ACEs: {sddl}");
        trustees.push(ace[5]);
    }
    assert!(trustees.contains(&"SY"), "SYSTEM is granted: {sddl}");
    assert!(
        trustees.iter().any(|t| *t == "OW" || *t == owner),
        "the owner is granted: {sddl}"
    );
    // Nobody else: no Everyone, network logons, users, guests or anonymous.
    for other in ["WD", "NU", "BU", "AU", "BG", "AN", "IU"] {
        assert!(
            !trustees.contains(&other),
            "{other} must not be granted: {sddl}"
        );
    }
}
