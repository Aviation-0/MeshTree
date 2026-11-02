//! `meshtree spawn`: create an isolated task worktree, branch, and `task.json`.
//!
//! Authoritative specification: `SPEC.md` §5 (layout), §7 (Task), §8 (`spawn`),
//! §14 (CLI, exit codes).
//!
//! Implemented here: steps 1–3, 4 (local registry) and 5 of §8 `spawn`, plus
//! `--claim` (claims are acquired before the worktree is created, so a
//! conflicting spawn fails fast with `MT-001` and no worktree). Step 3 links
//! every `[[share]]` directory from the primary worktree (§11); a failed link
//! falls back to `none` (`MT-040`) and never fails the spawn. The
//! coordination-ref task registry (`tasks/<id>.json`) is `T-043`.

use crate::exit::{CliError, PLATFORM, USAGE};
use crate::repo::{load_config, machine_id, require_repo_root};
use crate::{claims, clock, registry};
use meshtree_core::{Task, TaskIdAllocator, TaskSpec, TaskStatus};
use meshtree_platform::gitops::{CliGitOps, GitOps};
use meshtree_platform::share::{self, ShareOutcome};
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
    /// One outcome per `[[share]]` entry (§11).
    pub shares: Vec<ShareOutcome>,
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
        if let Err(err) = claims::acquire(&repo_root, &config, &task.id, &claim_paths, None) {
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

    // Step 3: dependency sharing (links into the primary worktree).
    let shares = share::apply_all(&repo_root, &worktree_path, &config.share);
    exclude_links(&git, &repo_root, &shares)?;

    transition(&mut task, TaskStatus::Active)?;
    registry::write_task(&repo_root, &task)?;

    Ok(SpawnReport {
        task,
        worktree_path,
        shares,
    })
}

fn transition(task: &mut Task, next: TaskStatus) -> Result<(), CliError> {
    // Spawn only walks legal §8 edges, so MT-004 here is an internal bug.
    task.transition(next)
        .map_err(|err| CliError::new(crate::exit::TASK, err.to_string()))
}

/// Marks each linked share path as excluded in the repository's machine-local
/// `info/exclude` (shared by every worktree). A `.gitignore` entry such as
/// `node_modules/` matches only real directories, so without this a symlink
/// or junction would show up as untracked and could be committed by an agent.
fn exclude_links(
    git: &CliGitOps,
    repo_root: &Path,
    shares: &[ShareOutcome],
) -> Result<(), CliError> {
    let patterns: Vec<String> = shares
        .iter()
        .filter(|o| matches!(o.applied, share::Applied::Linked(_)))
        .map(|o| format!("/{}", o.source.trim_matches('/')))
        .collect();
    if patterns.is_empty() {
        return Ok(());
    }
    let common = PathBuf::from(
        git.rev_parse("--git-common-dir")
            .map_err(|err| CliError::new(PLATFORM, err.to_string()))?,
    );
    let common = if common.is_absolute() {
        common
    } else {
        repo_root.join(common)
    };
    let path = common.join("info").join("exclude");
    let mut text = std::fs::read_to_string(&path).unwrap_or_default();
    let missing: Vec<&String> = patterns
        .iter()
        .filter(|p| !text.lines().any(|line| line.trim() == p.as_str()))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str("# meshtree [[share]] links (SPEC.md §11)\n");
    for pattern in missing {
        text.push_str(pattern);
        text.push('\n');
    }
    std::fs::create_dir_all(path.parent().expect("info/ parent"))
        .and_then(|()| std::fs::write(&path, text))
        .map_err(|err| crate::repo::platform_error(&path, &err))
}
