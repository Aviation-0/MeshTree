//! Task record, `"t-" + ULID` ID allocation, and the lifecycle state machine.
//!
//! Authoritative specification: `SPEC.md` §7 (Task), §8 (Workspace Lifecycle),
//! §16 (`MT-004`, `ADR-0002`).
//!
//! Pure and deterministic: the allocator takes the wall-clock milliseconds and
//! entropy as arguments, so callers in `meshtree-platform` / `meshtree` own
//! all clock and RNG access.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Error code for illegal task state transitions per `SPEC.md` §16 (`ADR-0002`).
pub const ERROR_CODE_ILLEGAL_TRANSITION: &str = "MT-004";

/// Prefix of every task ID (`SPEC.md` §8 `spawn` step 1).
pub const TASK_ID_PREFIX: &str = "t-";

/// Crockford base32 alphabet used by ULID.
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Length of an encoded ULID.
const ULID_LEN: usize = 26;

/// Number of random bits in a ULID (low bits; the high 48 bits are the timestamp).
const RANDOM_BITS: u32 = 80;

/// Largest timestamp representable in a ULID's 48-bit time field.
const MAX_TIMESTAMP_MS: u64 = (1 << 48) - 1;

/// Task lifecycle error returned on an illegal state transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskError {
    /// Error code (always `"MT-004"` per `SPEC.md` §16).
    pub code: &'static str,
    /// Detailed error description.
    pub message: String,
}

impl TaskError {
    /// Creates a new `TaskError` with code `MT-004`.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            code: ERROR_CODE_ILLEGAL_TRANSITION,
            message: message.into(),
        }
    }

    /// Returns the typed error code (`MT-004`).
    pub fn code(&self) -> &'static str {
        self.code
    }

    /// Returns the descriptive error message.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for TaskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[{}] illegal task transition: {}",
            self.code, self.message
        )
    }
}

impl std::error::Error for TaskError {}

/// Task ID of the form `"t-" + ULID` (26 uppercase Crockford base32 characters).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct TaskId(String);

impl TaskId {
    /// Builds a task ID from a raw 128-bit ULID value.
    pub fn from_ulid(ulid: u128) -> Self {
        let mut encoded = String::with_capacity(TASK_ID_PREFIX.len() + ULID_LEN);
        encoded.push_str(TASK_ID_PREFIX);
        for i in 0..ULID_LEN {
            let shift = 125 - 5 * i as u32;
            let index = ((ulid >> shift) & 0x1f) as usize;
            encoded.push(CROCKFORD[index] as char);
        }
        Self(encoded)
    }

    /// Parses and validates a canonical task ID string.
    pub fn parse(s: &str) -> Option<Self> {
        Self::decode(s).map(|_| Self(s.to_string()))
    }

    /// Returns the raw 128-bit ULID value encoded in this ID.
    pub fn ulid(&self) -> u128 {
        Self::decode(&self.0).expect("TaskId invariant: always a canonical ULID")
    }

    /// Returns the 48-bit millisecond timestamp encoded in this ID.
    pub fn timestamp_ms(&self) -> u64 {
        (self.ulid() >> RANDOM_BITS) as u64
    }

    /// Returns the ID as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn decode(s: &str) -> Option<u128> {
        let body = s.strip_prefix(TASK_ID_PREFIX)?;
        if body.len() != ULID_LEN {
            return None;
        }
        let mut value: u128 = 0;
        for (i, byte) in body.bytes().enumerate() {
            let digit = CROCKFORD.iter().position(|&c| c == byte)? as u128;
            // The first character carries only the top 3 bits of 128.
            if i == 0 && digit > 7 {
                return None;
            }
            value = (value << 5) | digit;
        }
        Some(value)
    }
}

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for TaskId {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if Self::decode(&value).is_some() {
            Ok(Self(value))
        } else {
            Err(format!(
                "invalid task id '{value}', expected \"t-\" + 26-char ULID"
            ))
        }
    }
}

impl From<TaskId> for String {
    fn from(id: TaskId) -> Self {
        id.0
    }
}

/// Monotonic `"t-" + ULID` allocator (`SPEC.md` §8 `spawn` step 1).
///
/// IDs from one allocator are strictly increasing, both as ULID values and as
/// strings, even when the supplied clock stalls or moves backwards: in that
/// case the previous ULID is incremented by one instead of using new entropy.
#[derive(Debug, Clone, Default)]
pub struct TaskIdAllocator {
    last: Option<u128>,
}

impl TaskIdAllocator {
    /// Creates an allocator with no prior IDs.
    pub fn new() -> Self {
        Self::default()
    }

    /// Allocates the next task ID from wall-clock milliseconds and 80 bits of entropy.
    ///
    /// `unix_ms` is clamped to the 48-bit ULID time field; only the low 80
    /// bits of `entropy` are used.
    pub fn allocate(&mut self, unix_ms: u64, entropy: u128) -> TaskId {
        let ms = unix_ms.min(MAX_TIMESTAMP_MS) as u128;
        let fresh = (ms << RANDOM_BITS) | (entropy & ((1u128 << RANDOM_BITS) - 1));
        let ulid = match self.last {
            Some(last) if fresh <= last => last
                .checked_add(1)
                .expect("ULID space exhausted (timestamp field at 48-bit maximum)"),
            _ => fresh,
        };
        self.last = Some(ulid);
        TaskId::from_ulid(ulid)
    }
}

/// Task lifecycle status (`SPEC.md` §7, §8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    /// Worktree and dependency links being created.
    Spawning,
    /// Agent is working in the task worktree.
    Active,
    /// `collect` running: validation and claim check.
    Collecting,
    /// Merging into the integration branch.
    Merging,
    /// Merged into the integration branch (terminal).
    Merged,
    /// Setup error or claim conflict (terminal).
    Failed,
    /// Aborted or TTL-expired (terminal).
    Abandoned,
}

impl TaskStatus {
    /// All statuses in lifecycle order.
    pub const ALL: [TaskStatus; 7] = [
        TaskStatus::Spawning,
        TaskStatus::Active,
        TaskStatus::Collecting,
        TaskStatus::Merging,
        TaskStatus::Merged,
        TaskStatus::Failed,
        TaskStatus::Abandoned,
    ];

    /// Returns `true` for statuses with no outgoing transitions.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            TaskStatus::Merged | TaskStatus::Failed | TaskStatus::Abandoned
        )
    }

    /// Returns `true` iff `self → next` is an edge of the `SPEC.md` §8 state diagram.
    pub fn can_transition_to(self, next: TaskStatus) -> bool {
        use TaskStatus::*;
        matches!(
            (self, next),
            // worktree + links ready / setup error
            (Spawning, Active)
                | (Spawning, Failed)
                // collect_task / abort_task or TTL expiry
                | (Active, Collecting)
                | (Active, Abandoned)
                // validate passed, claims clear / validate failed / claim conflict
                | (Collecting, Merging)
                | (Collecting, Active)
                | (Collecting, Failed)
                // merged into integration / rebase conflict
                | (Merging, Merged)
                | (Merging, Active)
        )
    }

    /// Returns the lowercase wire name used in `task.json`.
    pub fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Spawning => "spawning",
            TaskStatus::Active => "active",
            TaskStatus::Collecting => "collecting",
            TaskStatus::Merging => "merging",
            TaskStatus::Merged => "merged",
            TaskStatus::Failed => "failed",
            TaskStatus::Abandoned => "abandoned",
        }
    }
}

impl fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Caller-supplied fields for a new task; the rest are derived from the ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSpec {
    /// Human-readable task name.
    pub name: String,
    /// Agent identifier (`--agent`), if given.
    pub agent: Option<String>,
    /// Machine identifier of the owning daemon.
    pub machine: String,
    /// Base ref the task branch diverges from.
    pub base: String,
    /// RFC 3339 UTC creation timestamp.
    pub created_at: String,
    /// Initial claimed path patterns.
    pub claims: Vec<String>,
}

/// Task record (`SPEC.md` §7), persisted as `task.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    /// `"t-" + ULID` task identifier.
    pub id: TaskId,
    /// Human-readable task name.
    pub name: String,
    /// Agent identifier, if given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Machine identifier of the owning daemon.
    pub machine: String,
    /// Task branch, `meshtree/<task-id>`.
    pub branch: String,
    /// Worktree path relative to the repo root, `.meshtree/worktrees/<task-id>`.
    pub worktree: String,
    /// Base ref.
    pub base: String,
    /// RFC 3339 UTC creation timestamp.
    pub created_at: String,
    /// Claimed path patterns.
    #[serde(default)]
    pub claims: Vec<String>,
    /// Current lifecycle status.
    pub status: TaskStatus,
}

impl Task {
    /// Creates a task in `spawning` with branch and worktree derived from `id` (`SPEC.md` §8).
    pub fn new(id: TaskId, spec: TaskSpec) -> Self {
        Self {
            branch: format!("meshtree/{id}"),
            worktree: format!(".meshtree/worktrees/{id}"),
            id,
            name: spec.name,
            agent: spec.agent,
            machine: spec.machine,
            base: spec.base,
            created_at: spec.created_at,
            claims: spec.claims,
            status: TaskStatus::Spawning,
        }
    }

    /// Moves the task to `next`, or returns `MT-004` and leaves it unchanged.
    pub fn transition(&mut self, next: TaskStatus) -> Result<(), TaskError> {
        if !self.status.can_transition_to(next) {
            let id = &self.id;
            let from = self.status;
            return Err(TaskError::new(format!("{id}: {from} -> {next}")));
        }
        self.status = next;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_task() -> Task {
        let mut alloc = TaskIdAllocator::new();
        let id = alloc.allocate(1_730_642_531_000, 0x1234_5678_9abc_def0_1234);
        Task::new(
            id,
            TaskSpec {
                name: "fix-auth-middleware".to_string(),
                agent: Some("vibe-cli".to_string()),
                machine: "mc-lucas-win".to_string(),
                base: "main".to_string(),
                created_at: "2026-11-03T14:02:11Z".to_string(),
                claims: vec!["src/auth/**".to_string(), "tests/auth/**".to_string()],
            },
        )
    }

    #[test]
    fn task_id_ulid_format_and_monotonic() {
        // Known vector: all-zero and all-one ULIDs.
        assert_eq!(
            TaskId::from_ulid(0).as_str(),
            "t-00000000000000000000000000"
        );
        assert_eq!(
            TaskId::from_ulid(u128::MAX).as_str(),
            "t-7ZZZZZZZZZZZZZZZZZZZZZZZZZ"
        );

        let mut alloc = TaskIdAllocator::new();
        let first = alloc.allocate(1_000, 42);
        assert_eq!(first.as_str().len(), 2 + 26);
        assert!(first.as_str().starts_with("t-"));
        assert_eq!(first.timestamp_ms(), 1_000);
        assert_eq!(TaskId::parse(first.as_str()), Some(first.clone()));

        // Same millisecond, smaller entropy: still strictly increasing (+1).
        let second = alloc.allocate(1_000, 0);
        assert_eq!(second.ulid(), first.ulid() + 1);
        assert!(second > first);

        // Clock moves backwards: still strictly increasing.
        let third = alloc.allocate(500, u128::MAX);
        assert_eq!(third.ulid(), second.ulid() + 1);
        assert!(third > second);

        // Clock advances: fresh timestamp and entropy are used.
        let fourth = alloc.allocate(2_000, 7);
        assert_eq!(fourth.timestamp_ms(), 2_000);
        assert_eq!(fourth.ulid() & ((1 << 80) - 1), 7);
        assert!(fourth > third);

        // Rejects malformed IDs.
        for bad in [
            "",
            "t-",
            "01JAXGZK7Q4D3RT2WFH8MB3VQY",
            "x-01JAXGZK7Q4D3RT2WFH8MB3VQY",
            "t-01JAXGZK7Q4D3RT2WFH8MB3VQ",
            "t-01JAXGZK7Q4D3RT2WFH8MB3VQYZ",
            "t-01jaxgzk7q4d3rt2wfh8mb3vqy",
            "t-01JAXGZK7Q4D3RT2WFH8MB3VQI",
            "t-81JAXGZK7Q4D3RT2WFH8MB3VQY",
        ] {
            assert_eq!(TaskId::parse(bad), None, "must reject {bad:?}");
        }
        // The SPEC §7 example ID is canonical.
        assert!(TaskId::parse("t-01JAXGZK7Q4D3RT2WFH8MB3VQY").is_some());
    }

    #[test]
    fn task_json_serde_roundtrip() {
        let task = sample_task();
        let json = serde_json::to_string(&task).expect("task must serialize");
        let parsed: Task = serde_json::from_str(&json).expect("task must deserialize");
        assert_eq!(task, parsed);

        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["status"], "spawning");
        assert_eq!(value["branch"], format!("meshtree/{}", task.id));
        assert_eq!(
            value["worktree"],
            format!(".meshtree/worktrees/{}", task.id)
        );

        // The SPEC §7 example parses.
        let spec_example = r#"{
          "id": "t-01JAXGZK7Q4D3RT2WFH8MB3VQY",
          "name": "fix-auth-middleware",
          "agent": "vibe-cli",
          "machine": "mc-lucas-win",
          "branch": "meshtree/t-01JAXGZK7Q4D3RT2WFH8MB3VQY",
          "worktree": ".meshtree/worktrees/t-01JAXGZK7Q4D3RT2WFH8MB3VQY",
          "base": "main",
          "created_at": "2026-11-03T14:02:11Z",
          "claims": ["src/auth/**", "tests/auth/**"],
          "status": "active"
        }"#;
        let parsed: Task = serde_json::from_str(spec_example).expect("SPEC §7 example must parse");
        assert_eq!(parsed.status, TaskStatus::Active);

        // Every status uses its lowercase wire name.
        for status in TaskStatus::ALL {
            let json = serde_json::to_string(&status).unwrap();
            assert_eq!(json, format!("\"{}\"", status.as_str()));
            assert_eq!(serde_json::from_str::<TaskStatus>(&json).unwrap(), status);
        }

        // Invalid IDs and statuses are rejected at the serde boundary.
        let bad_id = spec_example.replace(
            "\"id\": \"t-01JAXGZK7Q4D3RT2WFH8MB3VQY\"",
            "\"id\": \"t-bogus\"",
        );
        assert_ne!(bad_id, spec_example);
        assert!(serde_json::from_str::<Task>(&bad_id).is_err());
        let bad_status = spec_example.replace("\"active\"", "\"init\"");
        assert!(serde_json::from_str::<Task>(&bad_status).is_err());
    }

    #[test]
    fn task_status_state_machine_transitions() {
        use TaskStatus::*;
        // The nine edges of the SPEC §8 state diagram.
        let edges = [
            (Spawning, Active),
            (Spawning, Failed),
            (Active, Collecting),
            (Active, Abandoned),
            (Collecting, Merging),
            (Collecting, Active),
            (Collecting, Failed),
            (Merging, Merged),
            (Merging, Active),
        ];

        // Exhaustive 7×7 table: legal iff listed above.
        for from in TaskStatus::ALL {
            for to in TaskStatus::ALL {
                let expected = edges.contains(&(from, to));
                assert_eq!(from.can_transition_to(to), expected, "{from} -> {to}");

                let mut task = sample_task();
                task.status = from;
                let result = task.transition(to);
                if expected {
                    assert_eq!(result, Ok(()));
                    assert_eq!(task.status, to);
                } else {
                    let err = result.expect_err("illegal transition must fail");
                    assert_eq!(err.code(), ERROR_CODE_ILLEGAL_TRANSITION);
                    assert_eq!(task.status, from, "state must be unchanged on error");
                }
            }
        }

        // Terminal statuses have no outgoing edges.
        for status in TaskStatus::ALL {
            let has_out = TaskStatus::ALL
                .iter()
                .any(|&to| status.can_transition_to(to));
            assert_eq!(status.is_terminal(), !has_out, "{status}");
        }

        // Happy path from SPEC §8, including both retry loops.
        let mut task = sample_task();
        for next in [
            Active, Collecting, Active, Collecting, Merging, Active, Collecting, Merging, Merged,
        ] {
            task.transition(next).expect("happy path must succeed");
        }
        assert_eq!(task.status, Merged);
    }
}
