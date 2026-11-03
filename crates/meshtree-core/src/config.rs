//! Configuration load and validation (`.meshtree/config.toml`).
//!
//! Authoritative specification: `SPEC.md` §6, §11, §12, §16 (`MT-050`).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

/// Error code for configuration errors per `SPEC.md` §16.
pub const ERROR_CODE_CONFIG_INVALID: &str = "MT-050";

/// Configuration error returned when parsing or validating `.meshtree/config.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    /// Error code (always `"MT-050"` per `SPEC.md` §16).
    pub code: &'static str,
    /// Detailed error description.
    pub message: String,
}

impl ConfigError {
    /// Creates a new `ConfigError` with code `MT-050`.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            code: ERROR_CODE_CONFIG_INVALID,
            message: message.into(),
        }
    }

    /// Returns the typed error code (`MT-050`).
    pub fn code(&self) -> &'static str {
        self.code
    }

    /// Returns the descriptive error message.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] config invalid: {}", self.code, self.message)
    }
}

impl std::error::Error for ConfigError {}

/// Root configuration model loaded from `.meshtree/config.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Config {
    /// Project-level branch and validation settings.
    #[serde(default)]
    pub project: ProjectConfig,

    /// Lease timing and expiry settings.
    #[serde(default)]
    pub lease: LeaseConfig,

    /// Dependency sharing definitions (`SPEC.md` §6, §11).
    #[serde(default)]
    pub share: Vec<ShareConfig>,

    /// Named shared-operation FIFO queues (`SPEC.md` §6, §12).
    /// Using `BTreeMap` ensures deterministic iteration order.
    #[serde(default)]
    pub queue: BTreeMap<String, QueueConfig>,

    /// Generated `AGENTS.md` block settings (`SPEC.md` §13, `ADR-0011`).
    #[serde(default)]
    pub agents: AgentsConfig,

    /// Machine identity configuration (optional).
    #[serde(default)]
    pub machine: Option<MachineConfig>,
}

impl Config {
    /// Parses and validates a configuration from a TOML string.
    pub fn from_toml_str(toml_str: &str) -> Result<Self, ConfigError> {
        let config: Config =
            toml::from_str(toml_str).map_err(|err| ConfigError::new(err.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    /// Serializes the configuration into a pretty-printed TOML string.
    pub fn to_toml_string(&self) -> Result<String, ConfigError> {
        toml::to_string_pretty(self).map_err(|err| ConfigError::new(err.to_string()))
    }

    /// Returns the standard default template text for `.meshtree/config.toml`.
    pub fn template() -> &'static str {
        r#"[project]
base_branch = "main"
integration_branch = "meshtree/integration"
validate = ["npm test"]            # run inside the task worktree before merge
promote_validate = ["npm test"]    # run on integration branch before promote

[lease]
ttl_secs = 900                     # claim expires if not heartbeaten
heartbeat_secs = 300
skew_grace_secs = 600              # cross-machine clock-skew allowance

[[share]]                          # dependency sharing (§11)
source = "node_modules"
strategy = "auto"                  # auto | clonefile | junction | symlink | none

[[share]]
source = "target"
strategy = "none"                  # cargo target stays per-worktree; queue serializes

[queue.test]
concurrency = 1                    # serialized shared operations
command = "npm test"
# ticket_ttl_secs = 900            # optional; max slot hold after grant (default lease.ttl_secs)

[queue.build]
concurrency = 1
command = "npm run build"

[agents]
max_lines = 10                     # cap for the generated AGENTS.md block

[machine]                          # optional; if absent: "mc-" + 16 hex of sha1(OS machine-id),
# id = "custom-id"                 # else "mc-<hostname>"
"#
    }

    /// Validates all configuration invariants per `SPEC.md` §6, §11, §12, §16.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.project.validate()?;
        self.lease.validate()?;
        for (i, share) in self.share.iter().enumerate() {
            share.validate(i)?;
        }
        for (name, queue) in &self.queue {
            queue.validate(name)?;
        }
        self.agents.validate()?;
        if let Some(ref machine) = self.machine {
            machine.validate()?;
        }
        Ok(())
    }
}

/// Project-level settings (`[project]` table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectConfig {
    /// Base branch from which task branches diverge (default: `"main"`).
    #[serde(default = "default_base_branch")]
    pub base_branch: String,

    /// Integration branch into which task branches merge (default: `"meshtree/integration"`).
    #[serde(default = "default_integration_branch")]
    pub integration_branch: String,

    /// Commands to execute in task worktree before merge (default: empty).
    #[serde(default)]
    pub validate: Vec<String>,

    /// Commands to execute on integration branch before promote (default: empty).
    #[serde(default)]
    pub promote_validate: Vec<String>,
}

fn default_base_branch() -> String {
    "main".to_string()
}

fn default_integration_branch() -> String {
    "meshtree/integration".to_string()
}

impl Default for ProjectConfig {
    fn default() -> Self {
        Self {
            base_branch: default_base_branch(),
            integration_branch: default_integration_branch(),
            validate: Vec::new(),
            promote_validate: Vec::new(),
        }
    }
}

impl ProjectConfig {
    /// Validates project configuration fields.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.base_branch.trim().is_empty() {
            return Err(ConfigError::new("project.base_branch must not be empty"));
        }
        if self.integration_branch.trim().is_empty() {
            return Err(ConfigError::new(
                "project.integration_branch must not be empty",
            ));
        }
        for (i, cmd) in self.validate.iter().enumerate() {
            if cmd.trim().is_empty() {
                return Err(ConfigError::new(format!(
                    "project.validate[{i}] command must not be empty"
                )));
            }
        }
        for (i, cmd) in self.promote_validate.iter().enumerate() {
            if cmd.trim().is_empty() {
                return Err(ConfigError::new(format!(
                    "project.promote_validate[{i}] command must not be empty"
                )));
            }
        }
        Ok(())
    }
}

/// Lease and heartbeat configuration (`[lease]` table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseConfig {
    /// Time-to-live in seconds before an untouched lease expires (default: 900).
    #[serde(default = "default_ttl_secs")]
    pub ttl_secs: u64,

    /// Interval in seconds between lease heartbeats (default: 300).
    #[serde(default = "default_heartbeat_secs")]
    pub heartbeat_secs: u64,

    /// Cross-machine clock-skew allowance in seconds (default: 600).
    #[serde(default = "default_skew_grace_secs")]
    pub skew_grace_secs: u64,
}

fn default_ttl_secs() -> u64 {
    900
}

fn default_heartbeat_secs() -> u64 {
    300
}

fn default_skew_grace_secs() -> u64 {
    600
}

impl Default for LeaseConfig {
    fn default() -> Self {
        Self {
            ttl_secs: default_ttl_secs(),
            heartbeat_secs: default_heartbeat_secs(),
            skew_grace_secs: default_skew_grace_secs(),
        }
    }
}

impl LeaseConfig {
    /// Validates lease configuration fields.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.ttl_secs == 0 {
            return Err(ConfigError::new("lease.ttl_secs must be greater than 0"));
        }
        if self.heartbeat_secs == 0 {
            return Err(ConfigError::new(
                "lease.heartbeat_secs must be greater than 0",
            ));
        }
        let hb = self.heartbeat_secs;
        let ttl = self.ttl_secs;
        if hb >= ttl {
            return Err(ConfigError::new(format!(
                "lease.heartbeat_secs ({hb}) must be strictly less than lease.ttl_secs ({ttl})"
            )));
        }
        Ok(())
    }
}

/// Sharing strategy for worktree dependencies (`SPEC.md` §6, §11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ShareStrategy {
    /// Auto-detect platform-optimal strategy (junction on Windows, clonefile/symlink on Unix).
    #[default]
    Auto,
    /// APFS copy-on-write clonefile (`cp -c`).
    Clonefile,
    /// NTFS junction point (`mklink /J` semantics).
    Junction,
    /// POSIX filesystem symlink.
    Symlink,
    /// No sharing; directory remains local and unshared.
    None,
}

impl fmt::Display for ShareStrategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ShareStrategy::Auto => write!(f, "auto"),
            ShareStrategy::Clonefile => write!(f, "clonefile"),
            ShareStrategy::Junction => write!(f, "junction"),
            ShareStrategy::Symlink => write!(f, "symlink"),
            ShareStrategy::None => write!(f, "none"),
        }
    }
}

impl FromStr for ShareStrategy {
    type Err = ConfigError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "auto" => Ok(ShareStrategy::Auto),
            "clonefile" => Ok(ShareStrategy::Clonefile),
            "junction" => Ok(ShareStrategy::Junction),
            "symlink" => Ok(ShareStrategy::Symlink),
            "none" => Ok(ShareStrategy::None),
            other => Err(ConfigError::new(format!(
                "unknown share strategy '{other}', expected auto|clonefile|junction|symlink|none"
            ))),
        }
    }
}

/// Dependency sharing configuration entry (`[[share]]` array of tables).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareConfig {
    /// Source relative directory path to share (e.g. `"node_modules"`, `"target"`).
    pub source: String,

    /// Linking strategy to use.
    #[serde(default)]
    pub strategy: ShareStrategy,
}

impl ShareConfig {
    /// Validates share entry fields.
    pub fn validate(&self, index: usize) -> Result<(), ConfigError> {
        let trimmed = self.source.trim();
        if trimmed.is_empty() {
            return Err(ConfigError::new(format!(
                "share[{index}].source must not be empty"
            )));
        }
        let src = &self.source;
        if trimmed.starts_with('/') || trimmed.starts_with('\\') || trimmed.contains(':') {
            return Err(ConfigError::new(format!(
                "share[{index}].source must be a relative path, got '{src}'"
            )));
        }
        if trimmed.split(['/', '\\']).any(|segment| segment == "..") {
            return Err(ConfigError::new(format!(
                "share[{index}].source cannot contain parent directory traversal ('..'), got '{src}'"
            )));
        }
        Ok(())
    }
}

/// Shared-operation FIFO queue configuration (`[queue.<name>]` table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueConfig {
    /// Maximum concurrent operations in this queue (must be >= 1).
    pub concurrency: usize,

    /// Command executed by this queue.
    pub command: String,

    /// Arguments an agent may append via `command_override` (`SPEC.md` §17,
    /// `ADR-0005`): exact arguments, or `--flag=*` for `--flag=<safe value>`.
    /// Empty (the default) accepts no overrides.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_args: Vec<String>,

    /// How long a granted slot may be held before it lapses, even if the
    /// holder is alive (`SPEC.md` §12, `ADR-0011`). Absent: `lease.ttl_secs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket_ttl_secs: Option<u64>,
}

impl QueueConfig {
    /// Validates queue configuration fields.
    pub fn validate(&self, name: &str) -> Result<(), ConfigError> {
        if name.trim().is_empty() {
            return Err(ConfigError::new("queue name must not be empty"));
        }
        if self.concurrency == 0 {
            return Err(ConfigError::new(format!(
                "queue.{name}.concurrency must be greater than 0, got 0"
            )));
        }
        if self.command.trim().is_empty() {
            return Err(ConfigError::new(format!(
                "queue.{name}.command must not be empty"
            )));
        }
        for arg in &self.allow_args {
            if arg.is_empty() || arg.chars().any(char::is_whitespace) {
                return Err(ConfigError::new(format!(
                    "queue.{name}.allow_args entries must be non-empty without whitespace, got '{arg}'"
                )));
            }
        }
        if self.ticket_ttl_secs == Some(0) {
            return Err(ConfigError::new(format!(
                "queue.{name}.ticket_ttl_secs must be greater than 0, got 0"
            )));
        }
        Ok(())
    }

    /// Slot TTL in seconds: `ticket_ttl_secs`, else the lease TTL.
    pub fn ticket_ttl_secs_or(&self, lease_ttl_secs: u64) -> u64 {
        self.ticket_ttl_secs.unwrap_or(lease_ttl_secs)
    }
}

/// Default cap for the generated `AGENTS.md` block (`SPEC.md` §13).
pub const DEFAULT_AGENTS_MAX_LINES: usize = 10;

/// Generated `AGENTS.md` block settings (`[agents]` table, `ADR-0011`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentsConfig {
    /// Maximum lines of the active-sessions list (must be >= 1).
    #[serde(default = "default_agents_max_lines")]
    pub max_lines: usize,
}

fn default_agents_max_lines() -> usize {
    DEFAULT_AGENTS_MAX_LINES
}

impl Default for AgentsConfig {
    fn default() -> Self {
        Self {
            max_lines: DEFAULT_AGENTS_MAX_LINES,
        }
    }
}

impl AgentsConfig {
    /// Validates `[agents]` fields.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.max_lines == 0 {
            return Err(ConfigError::new(
                "agents.max_lines must be greater than 0, got 0",
            ));
        }
        Ok(())
    }
}

/// Machine identification settings (`[machine]` table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct MachineConfig {
    /// Explicit machine identifier (auto-derived if absent).
    #[serde(default)]
    pub id: Option<String>,
}

impl MachineConfig {
    /// Validates machine configuration fields.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self
            .id
            .as_deref()
            .map(|id| id.trim().is_empty())
            .unwrap_or(false)
        {
            return Err(ConfigError::new(
                "machine.id must not be empty when specified",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies parsing the exact example from `SPEC.md` §6.
    #[test]
    fn config_parses_spec_example() {
        let spec_example = r#"
[project]
base_branch = "main"
integration_branch = "meshtree/integration"
validate = ["npm test"]            # run inside the task worktree before merge
promote_validate = ["npm test"]    # run on integration branch before promote

[lease]
ttl_secs = 900                     # claim expires if not heartbeaten
heartbeat_secs = 300
skew_grace_secs = 600              # cross-machine clock-skew allowance

[[share]]                          # dependency sharing (§9)
source = "node_modules"
strategy = "auto"                  # auto | clonefile | junction | symlink | none

[[share]]
source = "target"
strategy = "none"                  # cargo target stays per-worktree; queue serializes

[queue.test]
concurrency = 1                    # serialized shared operations
command = "npm test"

[queue.build]
concurrency = 1
command = "npm run build"

[machine]                          # optional; auto-derived from OS machine-id if absent
id = "mc-lucas-mac"
"#;

        let config = Config::from_toml_str(spec_example)
            .expect("SPEC.md §6 configuration must parse successfully");

        assert_eq!(config.project.base_branch, "main");
        assert_eq!(config.project.integration_branch, "meshtree/integration");
        assert_eq!(config.project.validate, vec!["npm test"]);
        assert_eq!(config.project.promote_validate, vec!["npm test"]);

        assert_eq!(config.lease.ttl_secs, 900);
        assert_eq!(config.lease.heartbeat_secs, 300);
        assert_eq!(config.lease.skew_grace_secs, 600);

        assert_eq!(config.share.len(), 2);
        assert_eq!(config.share[0].source, "node_modules");
        assert_eq!(config.share[0].strategy, ShareStrategy::Auto);
        assert_eq!(config.share[1].source, "target");
        assert_eq!(config.share[1].strategy, ShareStrategy::None);

        assert_eq!(config.queue.len(), 2);
        assert_eq!(config.queue["test"].concurrency, 1);
        assert_eq!(config.queue["test"].command, "npm test");
        assert_eq!(config.queue["build"].concurrency, 1);
        assert_eq!(config.queue["build"].command, "npm run build");

        assert_eq!(
            config.machine.as_ref().and_then(|m| m.id.as_deref()),
            Some("mc-lucas-mac")
        );
    }

    /// Verifies that invalid configuration inputs are rejected with typed `MT-050` errors.
    #[test]
    fn config_rejects_invalid_fields_with_mt050() {
        // 1. Malformed TOML syntax
        let malformed_toml = "invalid [ toml syntax";
        let err = Config::from_toml_str(malformed_toml).expect_err("malformed TOML must fail");
        assert_eq!(err.code(), ERROR_CODE_CONFIG_INVALID);

        // 2. Queue concurrency = 0
        let zero_concurrency = r#"
[queue.test]
concurrency = 0
command = "cargo test"
"#;
        let err = Config::from_toml_str(zero_concurrency)
            .expect_err("zero queue concurrency must fail validation");
        assert_eq!(err.code(), ERROR_CODE_CONFIG_INVALID);
        assert!(err.message().contains("concurrency"));

        // 3. Unknown share strategy
        let invalid_strategy = r#"
[[share]]
source = "node_modules"
strategy = "magical_linking"
"#;
        let err =
            Config::from_toml_str(invalid_strategy).expect_err("unknown share strategy must fail");
        assert_eq!(err.code(), ERROR_CODE_CONFIG_INVALID);

        // 4. Empty base_branch
        let empty_base = r#"
[project]
base_branch = "   "
"#;
        let err =
            Config::from_toml_str(empty_base).expect_err("empty base_branch must fail validation");
        assert_eq!(err.code(), ERROR_CODE_CONFIG_INVALID);

        // 5. Zero lease TTL
        let zero_ttl = r#"
[lease]
ttl_secs = 0
"#;
        let err = Config::from_toml_str(zero_ttl).expect_err("zero TTL must fail validation");
        assert_eq!(err.code(), ERROR_CODE_CONFIG_INVALID);

        // 6. Heartbeat >= TTL
        let heartbeat_ge_ttl = r#"
[lease]
ttl_secs = 300
heartbeat_secs = 300
"#;
        let err = Config::from_toml_str(heartbeat_ge_ttl)
            .expect_err("heartbeat >= ttl must fail validation");
        assert_eq!(err.code(), ERROR_CODE_CONFIG_INVALID);

        // 7. Empty share source path
        let empty_share_source = r#"
[[share]]
source = "  "
"#;
        let err = Config::from_toml_str(empty_share_source)
            .expect_err("empty share source must fail validation");
        assert_eq!(err.code(), ERROR_CODE_CONFIG_INVALID);

        // 8. Traversal in share source path
        let traversal_share = r#"
[[share]]
source = "../escape"
"#;
        let err = Config::from_toml_str(traversal_share)
            .expect_err("share traversal must fail validation");
        assert_eq!(err.code(), ERROR_CODE_CONFIG_INVALID);

        // 9. Empty queue command
        let empty_command = r#"
[queue.test]
concurrency = 1
command = ""
"#;
        let err = Config::from_toml_str(empty_command)
            .expect_err("empty queue command must fail validation");
        assert_eq!(err.code(), ERROR_CODE_CONFIG_INVALID);

        // 10. Empty machine id when specified
        let empty_machine = r#"
[machine]
id = ""
"#;
        let err = Config::from_toml_str(empty_machine)
            .expect_err("empty machine id must fail validation");
        assert_eq!(err.code(), ERROR_CODE_CONFIG_INVALID);
    }

    #[test]
    fn config_default_is_valid() {
        let default_config = Config::default();
        default_config
            .validate()
            .expect("Config::default() must pass validation");
    }

    #[test]
    fn config_template_parses_and_validates() {
        let template = Config::template();
        let config = Config::from_toml_str(template)
            .expect("Config::template() must parse and validate cleanly");
        assert_eq!(config.project.base_branch, "main");
        assert_eq!(config.share.len(), 2);
        // No machine id is baked into the template: each machine derives its own.
        assert_eq!(config.machine.and_then(|m| m.id), None);
    }

    #[test]
    fn config_agents_max_lines_and_queue_ticket_ttl() {
        // Defaults: cap 10, slot TTL = lease TTL.
        let base = "[project]\nbase_branch = \"main\"\n\n[queue.test]\nconcurrency = 1\ncommand = \"npm test\"\n";
        let config = Config::from_toml_str(base).expect("defaults parse");
        assert_eq!(config.agents.max_lines, DEFAULT_AGENTS_MAX_LINES);
        assert_eq!(config.queue["test"].ticket_ttl_secs, None);
        assert_eq!(
            config.queue["test"].ticket_ttl_secs_or(config.lease.ttl_secs),
            900
        );

        // Explicit values (the SPEC §6 example keys).
        let set = format!("{base}ticket_ttl_secs = 120\n\n[agents]\nmax_lines = 4\n");
        let config = Config::from_toml_str(&set).expect("explicit values parse");
        assert_eq!(config.agents.max_lines, 4);
        assert_eq!(config.queue["test"].ticket_ttl_secs_or(900), 120);
        let back = Config::from_toml_str(&config.to_toml_string().unwrap()).unwrap();
        assert_eq!(back, config);

        // Invalid values are MT-050.
        for bad in [
            format!("{base}ticket_ttl_secs = 0\n"),
            format!("{base}ticket_ttl_secs = -5\n"),
            format!("{base}ticket_ttl_secs = \"soon\"\n"),
            format!("{base}\n[agents]\nmax_lines = 0\n"),
            format!("{base}\n[agents]\nmax_lines = -1\n"),
            format!("{base}\n[agents]\nmax_lines = \"ten\"\n"),
        ] {
            let err = Config::from_toml_str(&bad).expect_err(&bad);
            assert_eq!(err.code(), ERROR_CODE_CONFIG_INVALID, "{bad}");
        }

        // The shipped template sets both keys' documented defaults.
        let template = Config::from_toml_str(Config::template()).unwrap();
        assert_eq!(template.agents.max_lines, 10);
    }

    #[test]
    fn config_roundtrip_toml() {
        let original = Config::default();
        let toml_str = original.to_toml_string().expect("must serialize to toml");
        let parsed = Config::from_toml_str(&toml_str).expect("must deserialize from toml");
        assert_eq!(original, parsed);
    }

    #[test]
    fn share_strategy_display_and_from_str() {
        for strategy in [
            ShareStrategy::Auto,
            ShareStrategy::Clonefile,
            ShareStrategy::Junction,
            ShareStrategy::Symlink,
            ShareStrategy::None,
        ] {
            let s = strategy.to_string();
            let parsed: ShareStrategy = s.parse().expect("strategy must roundtrip via FromStr");
            assert_eq!(strategy, parsed);
        }
        assert!("invalid".parse::<ShareStrategy>().is_err());
    }
}
