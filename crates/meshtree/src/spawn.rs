//! `meshtree spawn`: create an isolated task worktree, branch, and `task.json`.
//!
//! Authoritative specification: `SPEC.md` §5 (layout), §7 (Task), §8 (`spawn`),
//! §14 (CLI, exit codes).
//!
//! Implemented here: steps 1, 2, 4 (local registry) and 5 of §8 `spawn`.
//! Dependency sharing (step 3) lands in `T-023`; the coordination-ref
//! registry (step 4, `tasks/<id>.json`) and `--claim` land in `T-013`/`T-014`.

use crate::exit::{CliError, PLATFORM, USAGE};
use crate::repo::{load_config, require_repo_root};
use crate::{clock, registry};
use meshtree_core::{Config, Task, TaskIdAllocator, TaskSpec, TaskStatus};
use meshtree_platform::gitops::{CliGitOps, GitOps};
use std::path::{Path, PathBuf};

/// Arguments of `meshtree spawn`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnArgs {
    /// Task name.
    pub name: String,
    /// Base ref (`--base`); defaults to `project.base_branch`.
    pub base: Option<String>,
    /// Agent identifier (`--agent`).
    pub agent: Option<String>,
}

/// Result returned to the calling agent (§8 `spawn` step 5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnReport {
    /// The registered task (status `active`).
    pub task: Task,
    /// Absolute worktree path.
    pub worktree_path: PathBuf,
}

/// Runs `meshtree spawn` from working directory `cwd`.
pub fn run(cwd: &Path, args: SpawnArgs) -> Result<SpawnReport, CliError> {
    if args.name.trim().is_empty() {
        return Err(CliError::new(USAGE, "task name must not be empty"));
    }
    let repo_root = require_repo_root(cwd)?;
    let config = load_config(&repo_root)?;
    let git = CliGitOps::new(&repo_root);

    let base = args
        .base
        .unwrap_or_else(|| config.project.base_branch.clone());
    // Reject an unknown base before anything is written, so a typo leaves no
    // failed task behind.
    git.rev_parse(&format!("{base}^{{commit}}"))
        .map_err(|_| CliError::new(USAGE, format!("base ref '{base}' is not a commit")))?;

    // Step 1: allocate "t-" + ULID.
    let now_ms = clock::now_unix_ms();
    let id = TaskIdAllocator::new().allocate(now_ms, clock::entropy());
    let mut task = Task::new(
        id,
        TaskSpec {
            name: args.name,
            agent: args.agent,
            machine: machine_id(&config),
            base,
            created_at: clock::rfc3339_utc(now_ms),
            claims: Vec::new(),
        },
    );

    // Register as `spawning` first so a crash mid-setup leaves a record.
    registry::write_task(&repo_root, &task)?;

    // Step 2: git worktree add -b meshtree/<id> .meshtree/worktrees/<id> <base>.
    let worktree_path = repo_root.join(&task.worktree);
    if let Err(err) = git.worktree_add(&worktree_path, &task.branch, &task.base) {
        transition(&mut task, TaskStatus::Failed)?;
        registry::write_task(&repo_root, &task)?;
        return Err(CliError::new(
            PLATFORM,
            format!("{}: worktree setup failed: {err}", task.id),
        ));
    }

    transition(&mut task, TaskStatus::Active)?;
    registry::write_task(&repo_root, &task)?;

    Ok(SpawnReport {
        task,
        worktree_path,
    })
}

/// Machine identifier: `[machine].id` from config, else `mc-<hostname>`.
/// Full OS machine-id derivation belongs to the platform layer (`T-016`).
fn machine_id(config: &Config) -> String {
    if let Some(id) = config.machine.as_ref().and_then(|m| m.id.clone()) {
        return id;
    }
    hostname()
        .map(|host| format!("mc-{host}"))
        .unwrap_or_else(|| "mc-local".to_string())
}

/// Host name, lowercased with characters outside `[a-z0-9-]` mapped to `-`.
/// `COMPUTERNAME` is always set on Windows; `HOSTNAME` is usually a
/// non-exported shell variable on Unix, so fall back to the `hostname`
/// command, which exists on all three OSes.
fn hostname() -> Option<String> {
    let raw = ["COMPUTERNAME", "HOSTNAME"]
        .iter()
        .filter_map(|var| std::env::var(var).ok())
        .find(|host| !host.trim().is_empty())
        .or_else(|| {
            let out = std::process::Command::new("hostname").output().ok()?;
            out.status
                .success()
                .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
        })?;
    let host: String = raw
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    (!host.is_empty()).then_some(host)
}

fn transition(task: &mut Task, next: TaskStatus) -> Result<(), CliError> {
    // Spawn only walks legal §8 edges, so MT-004 here is an internal bug.
    task.transition(next)
        .map_err(|err| CliError::new(crate::exit::TASK, err.to_string()))
}
