//! `meshtree render-agents`: regenerate the root `AGENTS.md` block
//! (`SPEC.md` §13, §14).
//!
//! Every live task (`active`, `collecting`, `merging`) is listed with the
//! handoff committed on its task branch; tasks without one are listed as
//! `active.`. The block is rendered and spliced by `meshtree-core::handoff`
//! (deterministic, task-id order, capped at
//! [`DEFAULT_LINE_CAP`](meshtree_core::handoff::DEFAULT_LINE_CAP) lines; `SPEC`
//! §6 has no key for the cap yet). The file is rewritten only when its bytes
//! change.

use crate::exit::{CliError, PLATFORM};
use crate::registry;
use crate::repo::platform_error;
use meshtree_core::handoff::{
    render_agents_block, splice_agents_md, ActiveSession, HandoffRecord, DEFAULT_LINE_CAP,
};
use meshtree_core::{Task, TaskStatus};
use meshtree_platform::gitops::{CliGitOps, GitOps};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Root `AGENTS.md` file name.
pub const AGENTS_MD: &str = "AGENTS.md";

/// Result of one render.
#[derive(Debug)]
pub struct Rendered {
    pub path: PathBuf,
    /// `false` when the file already held exactly this block.
    pub changed: bool,
    /// Sessions listed in the block.
    pub sessions: usize,
    /// Skipped registry records and unreadable handoffs.
    pub warnings: Vec<String>,
}

/// `.meshtree/tasks/<id>/<file>` inside the repository tree.
pub fn handoff_path(task: &Task, file: &str) -> String {
    format!(".meshtree/tasks/{}/{file}", task.id)
}

/// The handoff committed at the tip of `task`'s branch: `Ok(None)` if the
/// branch has none, `Err` with a reason if it cannot be read or parsed.
pub fn branch_handoff(git: &CliGitOps, task: &Task) -> Result<Option<HandoffRecord>, String> {
    let spec = format!(
        "refs/heads/{}:{}",
        task.branch,
        handoff_path(task, "handoff.json")
    );
    let Ok(oid) = git.rev_parse(&spec) else {
        return Ok(None);
    };
    let bytes = git.cat_blob(&oid).map_err(|err| err.to_string())?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|err| format!("malformed handoff.json: {err}"))
}

/// Re-renders the active-sessions block into `<repo_root>/AGENTS.md`.
pub fn render(repo_root: &Path) -> Result<Rendered, CliError> {
    let listing = registry::list_tasks(repo_root)?;
    let mut warnings: Vec<String> = listing
        .malformed
        .iter()
        .map(|(path, reason)| format!("skipped {}: {reason}", path.display()))
        .collect();
    let git = CliGitOps::new(repo_root);
    let sessions: Vec<ActiveSession> = listing
        .tasks
        .iter()
        .filter(|t| {
            matches!(
                t.status,
                TaskStatus::Active | TaskStatus::Collecting | TaskStatus::Merging
            )
        })
        .map(|task| {
            let handoff = branch_handoff(&git, task).unwrap_or_else(|reason| {
                warnings.push(format!("{}: {reason}", task.id));
                None
            });
            ActiveSession::from_task(task, handoff)
        })
        .collect();

    let block = render_agents_block(&sessions, DEFAULT_LINE_CAP);
    let path = repo_root.join(AGENTS_MD);
    let current = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(platform_error(&path, &err)),
    };
    let updated = splice_agents_md(&current, &block);
    let changed = updated != current;
    if changed {
        let tmp = repo_root.join(format!("{AGENTS_MD}.meshtree.tmp"));
        fs::write(&tmp, &updated).map_err(|err| platform_error(&tmp, &err))?;
        fs::rename(&tmp, &path).map_err(|err| {
            let _ = fs::remove_file(&tmp);
            CliError::new(PLATFORM, format!("{}: {err}", path.display()))
        })?;
    }
    Ok(Rendered {
        path,
        changed,
        sessions: sessions.len(),
        warnings,
    })
}
