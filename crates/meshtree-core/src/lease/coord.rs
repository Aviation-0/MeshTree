//! `refs/meshtree/coord` tree codec (`SPEC.md` §5, §9).
//!
//! ```text
//! refs/meshtree/coord
//! ├── leases/<sha1(normalized-path)>.json   # derived snapshot (fast remote reads)
//! ├── log/<ULID>.json                        # append-only event log (source of truth)
//! └── tasks/<task-id>.json                    # cross-machine task registry
//! ```
//!
//! Pure: maps between events / replay output and `path → bytes` files. Git
//! object plumbing lives in `meshtree-platform`. Encoding is canonical
//! (pretty JSON + trailing newline, `BTreeMap` order), so two machines that
//! hold the same event set write byte-identical trees.

use super::{LeaseEvent, Replay};
use serde::Serialize;
use std::collections::BTreeMap;

/// Event log directory (source of truth).
pub const LOG_DIR: &str = "log";
/// Derived lease snapshot directory.
pub const LEASES_DIR: &str = "leases";
/// Cross-machine task registry directory.
pub const TASKS_DIR: &str = "tasks";

/// `leases/<sha1(normalized-path)>.json`.
pub fn lease_file(normalized_path: &str) -> String {
    format!("{LEASES_DIR}/{}.json", sha1_hex(normalized_path.as_bytes()))
}

/// `log/<ULID>.json`.
pub fn log_file(event_id: &str) -> String {
    format!("{LOG_DIR}/{event_id}.json")
}

/// Canonical JSON bytes: pretty-printed with a trailing newline.
pub fn canonical_json<T: Serialize>(value: &T) -> Vec<u8> {
    let mut bytes = serde_json::to_vec_pretty(value).expect("coord types always serialize");
    bytes.push(b'\n');
    bytes
}

/// Encodes the full tree: one `log/` file per distinct event plus the derived
/// `leases/` snapshot of `replay`.
pub fn encode(events: &[LeaseEvent], replay: &Replay) -> BTreeMap<String, Vec<u8>> {
    let mut files = BTreeMap::new();
    for event in events {
        // First occurrence wins, matching replay's id de-duplication after
        // sorting; callers pass the unioned (deduplicated) log.
        files
            .entry(log_file(&event.id))
            .or_insert_with(|| canonical_json(event));
    }
    for lease in replay.leases.values() {
        files.insert(lease_file(&lease.path), canonical_json(lease));
    }
    files
}

/// A `log/` entry that could not be decoded; skipped identically on every
/// machine so replay stays deterministic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeIssue {
    /// Tree path of the offending file.
    pub path: String,
    /// Why it was skipped.
    pub reason: String,
}

/// Decodes the event log from tree files. Only `log/*.json` is read (leases
/// are derived and always recomputed). An entry is skipped with a
/// [`DecodeIssue`] if it is not valid event JSON or its filename does not
/// match its `id`. Output is in path order.
pub fn decode_log<'a, I>(files: I) -> (Vec<LeaseEvent>, Vec<DecodeIssue>)
where
    I: IntoIterator<Item = (&'a str, &'a [u8])>,
{
    let mut sorted: Vec<(&str, &[u8])> = files
        .into_iter()
        .filter(|(path, _)| path.starts_with("log/") && path.ends_with(".json"))
        .collect();
    sorted.sort();
    let mut events = Vec::new();
    let mut issues = Vec::new();
    for (path, bytes) in sorted {
        match serde_json::from_slice::<LeaseEvent>(bytes) {
            Ok(event) if log_file(&event.id) == path => events.push(event),
            Ok(event) => issues.push(DecodeIssue {
                path: path.to_string(),
                reason: format!("filename does not match event id {}", event.id),
            }),
            Err(err) => issues.push(DecodeIssue {
                path: path.to_string(),
                reason: err.to_string(),
            }),
        }
    }
    (events, issues)
}

/// Lowercase hex SHA-1 (FIPS 180-4). Used only for stable snapshot
/// filenames, not for security.
pub fn sha1_hex(data: &[u8]) -> String {
    sha1(data).iter().map(|b| format!("{b:02x}")).collect()
}

/// SHA-1 digest of `data`.
#[allow(clippy::needless_range_loop)]
pub fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [
        0x6745_2301,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for block in msg.chunks_exact(64) {
        let mut w = [0u32; 80];
        for (i, word) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let t = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        for (hi, v) in h.iter_mut().zip([a, b, c, d, e]) {
            *hi = hi.wrapping_add(v);
        }
    }

    let mut out = [0u8; 20];
    for (chunk, word) in out.chunks_exact_mut(4).zip(h) {
        chunk.copy_from_slice(&word.to_be_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lease::{replay, EventType, ReplayConfig};
    use crate::task::TaskIdAllocator;
    use crate::time::Timestamp;

    #[test]
    fn coord_snapshot_sha1_path_encoding() {
        // FIPS 180 / well-known SHA-1 vectors.
        assert_eq!(sha1_hex(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(
            sha1_hex(b"The quick brown fox jumps over the lazy dog"),
            "2fd4e1c67a2d28fced849ee1bb76e7391b93eb12"
        );
        assert_eq!(
            sha1_hex(&vec![b'a'; 1_000_000]),
            "34aa973cd4c4daa4f61eeb2bdbad27316534016f"
        );
        // Padding edge cases: 55, 56, 63, 64 bytes all hash without panicking
        // and to distinct values.
        let edge: Vec<String> = [55, 56, 63, 64, 65]
            .iter()
            .map(|&n| sha1_hex(&vec![0u8; n]))
            .collect();
        let mut unique = edge.clone();
        unique.dedup();
        assert_eq!(unique.len(), edge.len());

        // Snapshot / log paths.
        assert_eq!(
            lease_file("src/auth/**"),
            "leases/95e612c3eaa66dd1ac6f58515b0070288135300f.json"
        );
        assert_eq!(
            log_file("01JAY7K2N8AAAAAAAAAAAAAAAA"),
            "log/01JAY7K2N8AAAAAAAAAAAAAAAA.json"
        );

        // Encode → decode round trip; bytes are independent of input order.
        let mut alloc = TaskIdAllocator::new();
        let (a, b) = (alloc.allocate(1, 1), alloc.allocate(1, 2));
        let ev = |id: &str, kind, path: &str, task: &crate::task::TaskId, secs: u64| LeaseEvent {
            id: id.to_string(),
            kind,
            path: path.to_string(),
            task: task.clone(),
            machine: "mc-test".to_string(),
            ts: Timestamp::from_unix_ms(secs * 1000),
            counter: 0,
            ttl_secs: Some(900),
        };
        let events = vec![
            ev("01A", EventType::Acquire, "src/auth/**", &a, 1),
            ev("01B", EventType::Acquire, "docs/**", &b, 2),
            ev("01C", EventType::Acquire, "src/auth/**", &b, 3),
        ];
        let config = ReplayConfig {
            default_ttl_secs: 900,
            skew_grace_secs: 600,
        };
        let derived = replay(&events, &config, |x, y| x == y);
        let files = encode(&events, &derived);
        let reversed: Vec<LeaseEvent> = events.iter().rev().cloned().collect();
        assert_eq!(
            encode(&reversed, &replay(&reversed, &config, |x, y| x == y)),
            files
        );
        let paths: Vec<&str> = files.keys().map(String::as_str).collect();
        assert_eq!(
            paths,
            [
                "leases/95e612c3eaa66dd1ac6f58515b0070288135300f.json",
                lease_file("docs/**").as_str(),
                "log/01A.json",
                "log/01B.json",
                "log/01C.json",
            ]
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
        );
        assert!(files.values().all(|bytes| bytes.ends_with(b"}\n")));

        let (decoded, issues) = decode_log(files.iter().map(|(p, b)| (p.as_str(), b.as_slice())));
        assert!(issues.is_empty());
        assert_eq!(decoded, events);

        // Malformed and misnamed log entries are skipped deterministically.
        let mut tampered = files.clone();
        tampered.insert("log/01Z.json".to_string(), b"{ not json".to_vec());
        tampered.insert("log/01Y.json".to_string(), canonical_json(&events[0]));
        tampered.insert("tasks/x.json".to_string(), b"ignored".to_vec());
        let (decoded, issues) =
            decode_log(tampered.iter().map(|(p, b)| (p.as_str(), b.as_slice())));
        assert_eq!(decoded, events);
        let bad: Vec<&str> = issues.iter().map(|i| i.path.as_str()).collect();
        assert_eq!(bad, ["log/01Y.json", "log/01Z.json"]);
    }
}
