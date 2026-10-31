//! MeshTree core pure logic crate.
//!
//! Invariant-protected heart: pure data models, configuration, task state
//! machine, lease replay, queue logic, and handoff formatting. No filesystem,
//! no network, no git processes. Everything in this crate is deterministic
//! and property-tested (`SPEC.md` §18, Appendix B).

pub mod config;
pub mod handoff;
pub mod lease;
pub mod pattern;
pub mod queue;
pub mod task;
pub mod time;

pub use config::{
    Config, ConfigError, LeaseConfig, MachineConfig, ProjectConfig, QueueConfig, ShareConfig,
    ShareStrategy, ERROR_CODE_CONFIG_INVALID,
};
pub use lease::{
    replay, EventType, Lease, LeaseEvent, LeaseTable, Rejection, Replay, ReplayConfig,
    ERROR_CODE_CLAIM_CONFLICT,
};
pub use pattern::{conflicts, conflicts_raw, normalize, PatternError};
pub use task::{
    Task, TaskError, TaskId, TaskIdAllocator, TaskSpec, TaskStatus, ERROR_CODE_ILLEGAL_TRANSITION,
};
pub use time::Timestamp;
