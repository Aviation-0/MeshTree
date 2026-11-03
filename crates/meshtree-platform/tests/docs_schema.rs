//! `docs/protocol.md` and `docs/architecture.md` stay in step with the code
//! (`T-033`; `SPEC.md` §9, §10, §16).
//!
//! Every ` ```json <kind> ` block in those documents is parsed as the Rust
//! type its tag names and must serialize back to the same JSON, so a renamed,
//! added or misspelled field fails here. Untagged JSON blocks are refused, so
//! no example escapes the check. The error catalog must list exactly the
//! codes of `SPEC.md` §16, and the IPC method table exactly the daemon's
//! methods.

use meshtree_core::handoff::HandoffRecord;
use meshtree_core::lease::{Lease, LeaseEvent, Rejection};
use meshtree_core::queue::QueueTicket;
use meshtree_core::{Task, TaskId, Timestamp};
use meshtree_platform::ipc::{Request, Response};
use meshtree_platform::wal::WalEntry;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

const DOCS: [&str; 2] = ["docs/protocol.md", "docs/architecture.md"];

/// Every kind the documents must exemplify at least once.
const KINDS: [&str; 11] = [
    "lease-event",
    "lease",
    "rejection",
    "task-record",
    "snapshot",
    "ipc-request",
    "ipc-response",
    "task",
    "queue-ticket",
    "handoff",
    "wal-entry",
];

fn repo_file(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()))
}

/// `(document, line, kind, body)` for each ` ```json ` block.
fn json_blocks() -> Vec<(&'static str, usize, String, String)> {
    let mut blocks = Vec::new();
    for doc in DOCS {
        let text = repo_file(doc);
        let mut lines = text.lines().enumerate();
        while let Some((n, line)) = lines.next() {
            let Some(info) = line.strip_prefix("```json") else {
                continue;
            };
            let kind = info.trim().to_string();
            assert!(
                !kind.is_empty(),
                "{doc}:{}: JSON example without a kind tag",
                n + 1
            );
            let body: Vec<&str> = lines
                .by_ref()
                .map(|(_, l)| l)
                .take_while(|l| !l.starts_with("```"))
                .collect();
            blocks.push((doc, n + 1, kind, body.join("\n")));
        }
    }
    blocks
}

/// Parses `value` as `T` and checks it serializes back unchanged.
fn round_trip<T: DeserializeOwned + Serialize>(value: &Value, at: &str) -> T {
    let parsed: T = serde_json::from_value(value.clone())
        .unwrap_or_else(|err| panic!("{at}: does not parse: {err}"));
    let back = serde_json::to_value(&parsed).unwrap();
    assert_eq!(&back, value, "{at}: does not round-trip");
    parsed
}

fn assert_ulid(id: &str, at: &str) {
    assert!(
        TaskId::parse(&format!("t-{id}")).is_some(),
        "{at}: '{id}' is not a canonical ULID"
    );
}

/// The snapshot watermark: the §9 ordering key of the newest folded event.
#[derive(Serialize, Deserialize)]
struct Watermark {
    ts: Timestamp,
    counter: u64,
    machine: String,
    id: String,
}

fn check(kind: &str, value: &Value, at: &str) {
    match kind {
        "lease-event" => {
            let event: LeaseEvent = round_trip(value, at);
            assert_ulid(&event.id, at);
        }
        "lease" => drop(round_trip::<Lease>(value, at)),
        "rejection" => {
            let rejection: Rejection = round_trip(value, at);
            assert_eq!(rejection.code, "MT-001", "{at}");
            assert_ulid(&rejection.event, at);
        }
        "task" => drop(round_trip::<Task>(value, at)),
        "task-record" => {
            let mut record = value.clone();
            let revision = record
                .as_object_mut()
                .and_then(|o| o.remove("revision"))
                .unwrap_or_else(|| panic!("{at}: registry record without 'revision'"));
            assert!(revision.is_u64(), "{at}: 'revision' must be an integer");
            round_trip::<Task>(&record, at);
        }
        "snapshot" => {
            let object = value.as_object().expect("snapshot is an object");
            let keys: Vec<&str> = object.keys().map(String::as_str).collect();
            assert_eq!(keys, ["leases", "rejected", "watermark"], "{at}");
            let watermark: Watermark = round_trip(&object["watermark"], at);
            assert_ulid(&watermark.id, at);
            // The documented example is the type the code reads and writes.
            drop(round_trip::<meshtree_core::lease::Snapshot>(value, at));
            let leases: Vec<Lease> = round_trip(&object["leases"], at);
            assert!(
                leases.windows(2).all(|w| w[0].path < w[1].path),
                "{at}: snapshot leases must be in path order"
            );
            let rejected: Vec<String> = round_trip(&object["rejected"], at);
            for id in &rejected {
                assert_ulid(id, at);
            }
        }
        "ipc-request" => drop(round_trip::<Request>(value, at)),
        "ipc-response" => {
            let response: Response = round_trip(value, at);
            assert_eq!(response.ok, response.result.is_some(), "{at}");
            assert_eq!(response.ok, response.error.is_none(), "{at}");
        }
        "queue-ticket" => {
            let ticket: QueueTicket = round_trip(value, at);
            let ulid = ticket.id.strip_prefix("q-").expect("ticket id is q-<ULID>");
            assert_ulid(ulid, at);
        }
        "handoff" => drop(round_trip::<HandoffRecord>(value, at)),
        "wal-entry" => drop(round_trip::<WalEntry>(value, at)),
        other => panic!("{at}: unknown example kind '{other}'"),
    }
}

#[test]
fn docs_json_examples_parse_against_core_types() {
    let blocks = json_blocks();
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    for (doc, line, kind, body) in &blocks {
        let at = format!("{doc}:{line} ({kind})");
        let value: Value =
            serde_json::from_str(body).unwrap_or_else(|err| panic!("{at}: invalid JSON: {err}"));
        check(kind, &value, &at);
        *seen.entry(kind.clone()).or_default() += 1;
    }
    for kind in KINDS {
        assert!(seen.contains_key(kind), "no '{kind}' example in the docs");
    }

    // Error catalog: exactly the SPEC §16 codes.
    let codes = |text: &str| -> BTreeSet<String> {
        text.lines()
            .filter_map(|l| l.strip_prefix("| `MT-"))
            .map(|rest| format!("MT-{}", &rest[..3]))
            .collect()
    };
    let spec = repo_file("SPEC.md");
    let section16 = spec
        .split("## 16. Error Model")
        .nth(1)
        .and_then(|s| s.split("\n## ").next())
        .expect("SPEC §16");
    let protocol = repo_file("docs/protocol.md");
    let catalog = protocol
        .split("## 10. Error catalog")
        .nth(1)
        .expect("protocol.md §10");
    let spec_codes = codes(section16);
    assert!(spec_codes.len() >= 13, "{spec_codes:?}");
    assert_eq!(codes(catalog), spec_codes, "error catalog vs SPEC §16");

    // IPC method table: exactly the daemon's dispatch arms.
    let daemon = repo_file("crates/meshtree/src/daemon.rs");
    let dispatch = daemon
        .split("fn dispatch(")
        .nth(1)
        .and_then(|s| s.split("other =>").next())
        .expect("Daemon::dispatch");
    let implemented: BTreeSet<&str> = dispatch
        .lines()
        .filter_map(|l| l.trim().strip_prefix('"'))
        .filter_map(|l| l.split_once("\" =>").map(|(name, _)| name))
        .collect();
    let methods = protocol
        .split("### 9.4 Methods")
        .nth(1)
        .and_then(|s| s.split("\n#### ").next())
        .expect("protocol.md §9.4");
    let documented: BTreeSet<&str> = methods
        .lines()
        .filter_map(|l| l.strip_prefix("| `"))
        .filter_map(|l| l.split_once('`').map(|(name, _)| name))
        .collect();
    assert!(implemented.len() >= 17, "{implemented:?}");
    assert_eq!(documented, implemented, "IPC method table vs daemon");
}
