//! Machine identity derived from the operating system.
//!
//! Authoritative specification: `SPEC.md` §6 (`[machine] id`: "auto-derived
//! from OS machine-id if absent"), `ADR-0011` decision 3.
//!
//! | OS | Source |
//! |---|---|
//! | Linux | `/etc/machine-id`, else `/var/lib/dbus/machine-id` |
//! | macOS | `IOPlatformUUID` from `ioreg -rd1 -c IOPlatformExpertDevice` |
//! | Windows | `HKLM\SOFTWARE\Microsoft\Cryptography\MachineGuid` (64-bit view) |
//!
//! The derived id is `mc-` + the first 16 hex digits of SHA-1 over the
//! trimmed identifier, so the raw OS identifier never reaches the shared
//! coordination ref. Callers fall back to `mc-<hostname>` when
//! [`os_machine_id`] returns `None`.

use meshtree_core::lease::coord::sha1_hex;

/// `mc-` + first 16 hex digits of SHA-1 over the trimmed OS identifier.
pub fn derive(os_id: &str) -> String {
    format!("mc-{}", &sha1_hex(os_id.trim().as_bytes())[..16])
}

/// The derived machine id, or `None` if the OS identifier cannot be read.
pub fn derived_machine_id() -> Option<String> {
    os_machine_id().map(|raw| derive(&raw))
}

/// The raw OS machine identifier, trimmed; `None` if unreadable or empty.
pub fn os_machine_id() -> Option<String> {
    sys::read()
        .map(|raw| raw.trim().to_string())
        .filter(|raw| !raw.is_empty())
}

#[cfg(target_os = "linux")]
mod sys {
    pub fn read() -> Option<String> {
        ["/etc/machine-id", "/var/lib/dbus/machine-id"]
            .iter()
            .filter_map(|path| std::fs::read_to_string(path).ok())
            .find(|text| !text.trim().is_empty())
    }
}

#[cfg(target_os = "macos")]
mod sys {
    pub fn read() -> Option<String> {
        let out = std::process::Command::new("ioreg")
            .args(["-rd1", "-c", "IOPlatformExpertDevice"])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        super::parse_ioreg(&String::from_utf8_lossy(&out.stdout))
    }
}

#[cfg(windows)]
mod sys {
    use std::ffi::c_void;

    const HKEY_LOCAL_MACHINE: isize = 0x8000_0002_u32 as i32 as isize;
    const RRF_RT_REG_SZ: u32 = 0x0000_0002;
    const RRF_SUBKEY_WOW6464KEY: u32 = 0x0001_0000;

    #[link(name = "advapi32")]
    extern "system" {
        fn RegGetValueW(
            key: isize,
            subkey: *const u16,
            value: *const u16,
            flags: u32,
            kind: *mut u32,
            data: *mut c_void,
            len: *mut u32,
        ) -> i32;
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    pub fn read() -> Option<String> {
        let subkey = wide(r"SOFTWARE\Microsoft\Cryptography");
        let value = wide("MachineGuid");
        let mut buf = [0u16; 128];
        let mut len = std::mem::size_of_val(&buf) as u32;
        // SAFETY: NUL-terminated names; `buf` is writable for `len` bytes and
        // RegGetValueW NUL-terminates REG_SZ data it returns.
        let rc = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                subkey.as_ptr(),
                value.as_ptr(),
                RRF_RT_REG_SZ | RRF_SUBKEY_WOW6464KEY,
                std::ptr::null_mut(),
                buf.as_mut_ptr().cast(),
                &mut len,
            )
        };
        if rc != 0 {
            return None;
        }
        let units = (len as usize / 2).min(buf.len());
        let text = &buf[..units];
        let end = text.iter().position(|&c| c == 0).unwrap_or(units);
        Some(String::from_utf16_lossy(&text[..end]))
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod sys {
    pub fn read() -> Option<String> {
        None
    }
}

/// The `IOPlatformUUID` value from `ioreg` output:
/// `"IOPlatformUUID" = "XXXXXXXX-…"`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn parse_ioreg(text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let rest = line.split_once("\"IOPlatformUUID\"")?.1;
        let value = rest.trim_start().strip_prefix('=')?.trim();
        let value = value.strip_prefix('"')?.strip_suffix('"')?;
        (!value.is_empty()).then(|| value.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_hashes_trimmed_identifier() {
        // sha1("abc") = a9993e364706816aba3e25717850c26c9cd0d89d
        assert_eq!(derive("abc"), "mc-a9993e364706816a");
        assert_eq!(derive("  abc\n"), derive("abc"));
        assert_ne!(derive("abd"), derive("abc"));
    }

    #[test]
    fn parse_ioreg_extracts_platform_uuid() {
        let out = "+-o J314sAP  <class IOPlatformExpertDevice>\n    {\n      \"IOPlatformSerialNumber\" = \"X\"\n      \"IOPlatformUUID\" = \"0A1B2C3D-4E5F-6071-8293-A4B5C6D7E8F9\"\n    }\n";
        assert_eq!(
            parse_ioreg(out).as_deref(),
            Some("0A1B2C3D-4E5F-6071-8293-A4B5C6D7E8F9")
        );
        assert_eq!(parse_ioreg("\"IOPlatformUUID\" = \"\""), None);
        assert_eq!(parse_ioreg("nothing here"), None);
    }
}
