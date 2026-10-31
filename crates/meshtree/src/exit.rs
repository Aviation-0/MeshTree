//! Process exit codes (`SPEC.md` §14).

/// Success.
pub const OK: u8 = 0;
/// Usage error (bad arguments, not inside a git repository, invalid config).
pub const USAGE: u8 = 10;
/// Task or lease error (`MT-00x`).
#[allow(dead_code)]
pub const TASK: u8 = 20;
/// Daemon error (`MT-01x`).
#[allow(dead_code)]
pub const DAEMON: u8 = 30;
/// Platform error (filesystem, locked file, quarantine).
pub const PLATFORM: u8 = 40;

/// A CLI failure carrying its §14 exit code and a user-facing message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliError {
    /// Exit code from this module.
    pub code: u8,
    /// Message printed to stderr.
    pub message: String,
}

impl CliError {
    /// Creates an error with the given exit code.
    pub fn new(code: u8, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}
