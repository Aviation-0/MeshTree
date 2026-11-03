//! OS machine-id derivation (`SPEC.md` §6, `ADR-0011`).
//!
//! The derived id is cross-checked against an independent read of the same
//! OS source, so the test fails if the platform reader returns the wrong
//! value, not only if it returns none. Every CI OS has a machine id.

use meshtree_core::lease::coord::sha1_hex;
use meshtree_platform::machine::{derive, derived_machine_id, os_machine_id};

/// The OS identifier read a second way (a different code path from the
/// module under test).
fn independent_os_id() -> String {
    #[cfg(target_os = "linux")]
    let raw = std::fs::read_to_string("/etc/machine-id")
        .or_else(|_| std::fs::read_to_string("/var/lib/dbus/machine-id"))
        .expect("Linux machine-id file");
    #[cfg(target_os = "macos")]
    let raw = {
        let out = std::process::Command::new("ioreg")
            .args(["-rd1", "-c", "IOPlatformExpertDevice"])
            .output()
            .expect("run ioreg");
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        let line = text
            .lines()
            .find(|l| l.contains("\"IOPlatformUUID\""))
            .expect("IOPlatformUUID line")
            .to_string();
        line.rsplit('"').nth(1).expect("quoted UUID").to_string()
    };
    #[cfg(windows)]
    let raw = {
        let out = std::process::Command::new("reg")
            .args([
                "query",
                r"HKLM\SOFTWARE\Microsoft\Cryptography",
                "/v",
                "MachineGuid",
                "/reg:64",
            ])
            .output()
            .expect("run reg query");
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        text.lines()
            .find(|l| l.trim_start().starts_with("MachineGuid"))
            .and_then(|l| l.split_whitespace().last())
            .expect("MachineGuid value")
            .to_string()
    };
    raw.trim().to_string()
}

#[test]
fn machine_id_derived_from_os_and_stable() {
    let raw = os_machine_id().expect("every CI OS exposes a machine id");
    assert_eq!(raw, independent_os_id(), "reader disagrees with the OS");

    let id = derived_machine_id().expect("derived id");
    let expected = format!("mc-{}", &sha1_hex(raw.as_bytes())[..16]);
    assert_eq!(id, expected);
    assert_eq!(id, derive(&raw));

    // Shape: "mc-" + 16 lowercase hex; the raw identifier is not exposed.
    let hex = id.strip_prefix("mc-").expect("mc- prefix");
    assert_eq!(hex.len(), 16, "{id}");
    assert!(hex
        .bytes()
        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
    assert!(!id.contains(raw.as_str()));

    // Stable across calls.
    for _ in 0..3 {
        assert_eq!(derived_machine_id().as_deref(), Some(id.as_str()));
    }
}
