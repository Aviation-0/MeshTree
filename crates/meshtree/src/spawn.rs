//! `meshtree spawn`: create an isolated task worktree, branch, and `task.json`.
//!
//! Authoritative specification: `SPEC.md` §5 (layout), §7 (Task), §8 (`spawn`),
//! §14 (CLI, exit codes).
//!
//! Implemented here: steps 1, 2, 4 (local registry) and 5 of §8 `spawn`,
//! plus `--claim` (claims are acquired before the worktree is created, so a
//! conflicting spawn fails fast with `MT-001` and no worktree). Dependency
//! sharing (step 3) lands in `T-023`; the coordination-ref task registry
//! (`tasks/<id>.json`) is not written yet.

use crate::exit::{CliError, PLATFORM, USAGE};
use crate::repo::{load_config, machine_id, require_repo_root};
use crate::{claims, clock, registry};
use meshtree_core::{Task, TaskIdAllocator, TaskSpec, TaskStatus};
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
    /// Path patterns to claim (`--claim`).
    pub claims: Vec<String>,
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
    let claim_paths = claims::normalize_all(&args.claims)?;

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

    if !claim_paths.is_empty() {
        if let Err(err) = claims::acquire(&repo_root, &config, &task.id, &claim_paths) {
            transition(&mut task, TaskStatus::Failed)?;
            registry::write_task(&repo_root, &task)?;
            return Err(CliError::new(
                err.code,
                format!("{}: {}", task.id, err.message),
            ));
        }
        task.claims = claim_paths;
        registry::write_task(&repo_root, &task)?;
    }

    // Step 2: git worktree add -b meshtree/<id> .meshtree/worktrees/<id> <base>.
    let worktree_path = repo_root.join(&task.worktree);
    if let Err(err) = git.worktree_add(&worktree_path, &task.branch, &task.base) {
        if !task.claims.is_empty() {
            // Best effort: a failed task must not keep paths locked.
            let _ = claims::release(&repo_root, &config, &task.id, &task.claims);
        }
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

fn transition(task: &mut Task, next: TaskStatus) -> Result<(), CliError> {
    // Spawn only walks legal §8 edges, so MT-004 here is an internal bug.
    task.transition(next)
        .map_err(|err| CliError::new(crate::exit::TASK, err.to_string()))
}
