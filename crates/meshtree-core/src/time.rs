//! Millisecond UTC timestamps with a canonical RFC 3339 wire form.
//!
//! Lease events carry `ts` like `2026-11-03T14:05:00.500Z` (`SPEC.md` §9).
//! Ordering must not depend on string formatting, so timestamps are held as
//! Unix milliseconds and always serialized as `YYYY-MM-DDTHH:MM:SS.mmmZ`.
//! Pure: no clock access.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Milliseconds since the Unix epoch, serialized as canonical RFC 3339 UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Timestamp(pub u64);

impl Timestamp {
    /// Builds a timestamp from Unix milliseconds.
    pub fn from_unix_ms(ms: u64) -> Self {
        Self(ms)
    }

    /// Unix milliseconds.
    pub fn unix_ms(self) -> u64 {
        self.0
    }

    /// Parses RFC 3339 UTC (`Z` suffix only), with 0–9 fractional digits
    /// truncated to milliseconds. Years 1970–9999.
    pub fn parse(s: &str) -> Option<Self> {
        let b = s.as_bytes();
        if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' {
            return None;
        }
        if b[13] != b':' || b[16] != b':' || *b.last()? != b'Z' {
            return None;
        }
        let num = |range: std::ops::Range<usize>| -> Option<i64> {
            let part = s.get(range)?;
            if !part.bytes().all(|c| c.is_ascii_digit()) {
                return None;
            }
            part.parse().ok()
        };
        let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
        let (hour, minute, second) = (num(11..13)?, num(14..16)?, num(17..19)?);
        if year < 1970 || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
            return None;
        }
        if hour > 23 || minute > 59 || second > 59 {
            return None;
        }
        let frac = s.get(19..s.len() - 1)?;
        let millis = if frac.is_empty() {
            0
        } else {
            let digits = frac.strip_prefix('.')?;
            if digits.is_empty() || digits.len() > 9 || !digits.bytes().all(|c| c.is_ascii_digit())
            {
                return None;
            }
            let ms3: String = digits.chars().chain("000".chars()).take(3).collect();
            ms3.parse::<u64>().ok()?
        };
        let days = days_from_civil(year, month, day);
        // Reject impossible dates such as Feb 30 by round-tripping.
        if civil_from_days(days) != (year, month, day) {
            return None;
        }
        let secs = days * 86_400 + hour * 3600 + minute * 60 + second;
        Some(Self(secs as u64 * 1000 + millis))
    }

    /// Canonical form `YYYY-MM-DDTHH:MM:SS.mmmZ`.
    pub fn to_rfc3339(self) -> String {
        let secs = (self.0 / 1000) as i64;
        let millis = self.0 % 1000;
        let (days, sod) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
        let (year, month, day) = civil_from_days(days);
        let (h, m, s) = (sod / 3600, sod % 3600 / 60, sod % 60);
        format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}.{millis:03}Z")
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_rfc3339())
    }
}

impl TryFrom<String> for Timestamp {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value).ok_or_else(|| format!("invalid RFC 3339 UTC timestamp '{value}'"))
    }
}

impl From<Timestamp> for String {
    fn from(ts: Timestamp) -> Self {
        ts.to_rfc3339()
    }
}

/// Proleptic Gregorian date → days since 1970-01-01 (Howard Hinnant).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Days since 1970-01-01 → proleptic Gregorian (year, month, day).
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
    fn timestamp_known_vectors_and_roundtrip() {
        let ts = Timestamp::parse("2026-11-03T14:05:00.500Z").unwrap();
        assert_eq!(ts.unix_ms(), 1_793_714_700_500);
        assert_eq!(ts.to_rfc3339(), "2026-11-03T14:05:00.500Z");
        assert_eq!(Timestamp::parse("1970-01-01T00:00:00Z"), Some(Timestamp(0)));
        assert_eq!(
            Timestamp::parse("2000-02-29T23:59:59.9999Z"),
            Some(Timestamp(951_868_799_999))
        );
        assert_eq!(
            Timestamp::parse("2026-11-03T14:05:00.5Z"),
            Some(Timestamp(1_793_714_700_500))
        );
        for ms in [
            0,
            1,
            999,
            951_868_799_999,
            1_793_714_700_500,
            253_402_300_799_999,
        ] {
            let ts = Timestamp(ms);
            assert_eq!(Timestamp::parse(&ts.to_rfc3339()), Some(ts));
        }
        for bad in [
            "",
            "2026-11-03 14:05:00Z",
            "2026-11-03T14:05:00",
            "2026-11-03T14:05:00+01:00",
            "2026-02-30T00:00:00Z",
            "2026-13-01T00:00:00Z",
            "2026-11-03T24:00:00Z",
            "2026-11-03T14:05:00.Z",
            "1969-12-31T23:59:59Z",
            "2026-11-03T14:05:00.1234567890Z",
            "2026-1a-03T14:05:00Z",
        ] {
            assert_eq!(Timestamp::parse(bad), None, "must reject {bad:?}");
        }
    }
}
