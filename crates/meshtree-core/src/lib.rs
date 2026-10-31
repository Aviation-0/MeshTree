//! MeshTree core pure logic crate.
//!
//! Invariant-protected heart: pure data models, configuration, task state
//! machine, lease replay, queue logic, and handoff formatting. No filesystem,
//! no network, no git processes. Everything in this crate is deterministic
//! and property-tested (`SPEC.md` §18, Appendix B).

pub mod config;

pub use config::{
    Config, ConfigError, LeaseConfig, MachineConfig, ProjectConfig, QueueConfig, ShareConfig,
    ShareStrategy, ERROR_CODE_CONFIG_INVALID,
};
