//! `meshtree status [task-id] [--json]`: report tasks from the local registry.
//!
//! Authoritative specification: `SPEC.md` §7 (Task), §8, §14 (CLI, exit codes).
//! Reads `.meshtree/tasks/*/task.json` only; it never touches git or the
//! coordination ref, so it works with no daemon running.

use crate::exit::{CliError, PLATFORM, USAGE};
use crate::registry;
use crate::repo::require_repo_root;
use meshtree_core::{Task, TaskId};
use std::path::Path;

/// Runs `meshtree status`; returns stdout text plus warnings for stderr.
pub fn run(cwd: &Path, task_id: Option<&str>, json: bool) -> Result<Output, CliError> {
    let repo_root = require_repo_root(cwd)?;

    if let Some(raw) = task_id {
        let id = TaskId::parse(raw)
            .ok_or_else(|| CliError::new(USAGE, format!("'{raw}' is not a task id")))?;
        let task = registry::read_task(&repo_root, &id)?;
        let stdout = if json {
            json_line(serde_json::to_string_pretty(&task))?
        } else {
            format_line(&task)
        };
        return Ok(Output {
            stdout,
            warnings: Vec::new(),
        });
    }

    let listing = registry::list_tasks(&repo_root)?;
    let warnings = listing
        .malformed
        .iter()
        .map(|(path, reason)| format!("skipping {}: {reason}", path.display()))
        .collect();
    let stdout = if json {
        json_line(serde_json::to_string_pretty(&listing.tasks))?
    } else if listing.tasks.is_empty() {
        "no tasks\n".to_string()
    } else {
        listing.tasks.iter().map(format_line).collect()
    };
    Ok(Output { stdout, warnings })
}

/// Text for stdout and per-record warnings for stderr.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    /// Rendered report.
    pub stdout: String,
    /// One line per skipped malformed record.
    pub warnings: Vec<String>,
}

/// One task per line: id, status, name, branch, base, worktree.
fn format_line(task: &Task) -> String {
    format!(
        "{}  {:<10}  {}  branch={}  base={}  worktree={}\n",
        task.id,
        task.status.as_str(),
        task.name,
        task.branch,
        task.base,
        task.worktree
    )
}

/// Finishes a `serde_json::to_string_pretty` result with a trailing newline.
fn json_line(result: serde_json::Result<String>) -> Result<String, CliError> {
    let mut text =
        result.map_err(|err| CliError::new(PLATFORM, format!("serialize status: {err}")))?;
    text.push('\n');
    Ok(text)
}
