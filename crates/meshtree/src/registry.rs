//! Local task registry: `.meshtree/tasks/<id>/task.json` (`SPEC.md` §5, §7).

use crate::exit::{CliError, PLATFORM, TASK};
use crate::repo::{platform_error, MESHTREE_DIR};
use meshtree_core::{Task, TaskId};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// `.meshtree/tasks` under `repo_root`.
pub fn tasks_dir(repo_root: &Path) -> PathBuf {
    repo_root.join(MESHTREE_DIR).join("tasks")
}

/// Writes `task.json` via a temp file and rename so readers never see a
/// partial record. Creates the task directory if needed.
pub fn write_task(repo_root: &Path, task: &Task) -> Result<(), CliError> {
    let dir = tasks_dir(repo_root).join(task.id.as_str());
    fs::create_dir_all(&dir).map_err(|err| platform_error(&dir, &err))?;
    let mut json = serde_json::to_string_pretty(task)
        .map_err(|err| CliError::new(PLATFORM, format!("serialize task.json: {err}")))?;
    json.push('\n');
    let tmp = dir.join("task.json.tmp");
    let path = dir.join("task.json");
    fs::write(&tmp, json).map_err(|err| platform_error(&tmp, &err))?;
    fs::rename(&tmp, &path).map_err(|err| platform_error(&path, &err))
}

/// Reads one task; an unknown or malformed record is a §14 task error (exit 20).
pub fn read_task(repo_root: &Path, id: &TaskId) -> Result<Task, CliError> {
    let path = tasks_dir(repo_root).join(id.as_str()).join("task.json");
    match parse_task_file(&path, Some(id))? {
        Some(task) => Ok(task),
        None => Err(CliError::new(TASK, format!("unknown task {id}"))),
    }
}

/// Result of scanning the registry.
#[derive(Debug, Default)]
pub struct Listing {
    /// Valid task records, sorted by task ID.
    pub tasks: Vec<Task>,
    /// Unreadable records: `(path, reason)`, sorted by path.
    pub malformed: Vec<(PathBuf, String)>,
}

/// Scans `.meshtree/tasks/*/task.json`.
///
/// A missing `tasks/` directory is an empty registry. Directories without a
/// `task.json` (e.g. handoff records merged from other task branches, §13) are
/// skipped. Malformed records are collected, not fatal, so one bad file never
/// hides the other tasks.
pub fn list_tasks(repo_root: &Path) -> Result<Listing, CliError> {
    let dir = tasks_dir(repo_root);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Listing::default()),
        Err(err) => return Err(platform_error(&dir, &err)),
    };
    let mut listing = Listing::default();
    for entry in entries {
        let entry = entry.map_err(|err| platform_error(&dir, &err))?;
        if !entry.path().is_dir() {
            continue;
        }
        let path = entry.path().join("task.json");
        let expected = TaskId::parse(&entry.file_name().to_string_lossy());
        if expected.is_none() {
            if path.exists() {
                listing
                    .malformed
                    .push((path, "directory name is not a task id".to_string()));
            }
            continue;
        }
        match parse_task_file(&path, expected.as_ref()) {
            Ok(Some(task)) => listing.tasks.push(task),
            Ok(None) => {}
            Err(err) => listing.malformed.push((path, err.message)),
        }
    }
    listing.tasks.sort_by(|a, b| a.id.cmp(&b.id));
    listing.malformed.sort();
    Ok(listing)
}

/// `Ok(None)` when the file does not exist.
fn parse_task_file(path: &Path, expected: Option<&TaskId>) -> Result<Option<Task>, CliError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(platform_error(path, &err)),
    };
    let task: Task = serde_json::from_str(&text).map_err(|err| {
        CliError::new(
            TASK,
            format!("{}: malformed task.json: {err}", path.display()),
        )
    })?;
    if let Some(expected) = expected {
        if task.id != *expected {
            return Err(CliError::new(
                TASK,
                format!(
                    "{}: task.json id {} does not match its directory",
                    path.display(),
                    task.id
                ),
            ));
        }
    }
    Ok(Some(task))
}
