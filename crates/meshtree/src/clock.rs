//! Wall-clock and entropy sources for the binary.
//!
//! `meshtree-core` stays pure (no clock, no RNG); these are the values the
//! binary injects into it.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::time::{SystemTime, UNIX_EPOCH};

/// Current time as milliseconds since the Unix epoch.
pub fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Current time as nanoseconds since the Unix epoch; used as the lease
/// event `counter` so later events from this machine always order after
/// earlier ones, even within one millisecond and across processes.
pub fn now_unix_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// 128 bits of per-call entropy for ULID allocation.
///
/// Uses the std `RandomState` keys (randomly seeded per process, distinct per
/// call) mixed with the clock and pid, so no RNG dependency is needed. ULIDs
/// need uniqueness, not cryptographic strength.
pub fn entropy() -> u128 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut value = 0u128;
    for _ in 0..2 {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u128(nanos);
        hasher.write_u32(std::process::id());
        value = (value << 64) | u128::from(hasher.finish());
    }
    value
}

/// Formats Unix milliseconds as an RFC 3339 UTC timestamp with second
/// precision, e.g. `2026-11-03T14:02:11Z` (`SPEC.md` §7).
pub fn rfc3339_utc(unix_ms: u64) -> String {
    let secs = (unix_ms / 1000) as i64;
    let (days, sod) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (year, month, day) = civil_from_days(days);
    let (h, m, s) = (sod / 3600, sod % 3600 / 60, sod % 60);
    format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Days since 1970-01-01 → proleptic Gregorian (year, month, day).
/// Howard Hinnant's `civil_from_days` algorithm.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_known_vectors() {
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_utc(1_793_714_531_999), "2026-11-03T14:02:11Z");
        assert_eq!(rfc3339_utc(951_868_799_000), "2000-02-29T23:59:59Z");
        assert_eq!(rfc3339_utc(951_868_800_000), "2000-03-01T00:00:00Z");
    }

    #[test]
    fn entropy_differs_between_calls() {
        assert_ne!(entropy(), entropy());
    }
}
