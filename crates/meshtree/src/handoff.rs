//! `meshtree handoff <task-id> --json-file <path> | --set key=value…`
//! (`SPEC.md` §5, §13, §14).
//!
//! The record is committed **on the task branch** as
//! `.meshtree/tasks/<id>/{handoff.json,HANDOFF.md}`, then the root
//! `AGENTS.md` block is re-rendered.
//!
//! The commit is built with plumbing (`hash-object`, `mktree` along the
//! `.meshtree/tasks/<id>` spine only, `commit-tree` with MeshTree's fixed
//! identity: no hooks, no signing prompt, no host identity needed) on top of
//! the branch tip, and the task worktree is then fast-forwarded to it
//! (`merge --ff-only`). Git updates the branch, index and the two files
//! together and leaves the agent's other uncommitted changes alone; if the
//! agent has local edits to the handoff files themselves, or the branch moved
//! meanwhile, the fast-forward fails and nothing changes.
//!
//! `--json-file` replaces the whole record (`-` reads stdin). `--set` edits
//! the record already on the branch: `objective=` / `state=` replace the
//! text; list keys (`done`, `next`, `failing_tests`, `ruled_out`,
//! `decisions`) are replaced by the values given for them, in order
//! (`--set next=a --set next=b`; an empty value clears the list); a decision
//! is `decisions=<what>|<why>`. Unknown keys are usage errors.

use crate::agents::{self, branch_handoff, handoff_path, Rendered};
use crate::exit::{CliError, PLATFORM, TASK, USAGE};
use crate::registry;
use crate::repo::require_repo_root;
use meshtree_core::handoff::{render_handoff_md, Decision, HandoffRecord};
use meshtree_core::{Task, TaskId, TaskStatus};
use meshtree_platform::gitops::{CliGitOps, GitError, GitOps, MergeOps, TreeEntry};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

/// The `handoff.json` fields (`SPEC.md` §13).
const KEYS: [&str; 7] = [
    "objective",
    "state",
    "done",
    "next",
    "failing_tests",
    "decisions",
    "ruled_out",
];

/// Where the new record comes from.
#[derive(Debug)]
pub enum Input {
    /// A complete `handoff.json`; `-` is stdin.
    JsonFile(PathBuf),
    /// `key=value` edits of the record on the branch.
    Set(Vec<String>),
}

/// Result of `meshtree handoff`.
#[derive(Debug)]
pub struct Report {
    pub task: Task,
    /// The new task-branch commit, or `None` if the record was unchanged.
    pub commit: Option<String>,
    pub agents: Rendered,
}

fn git_error(err: GitError) -> CliError {
    CliError::new(PLATFORM, err.to_string())
}

/// Runs `meshtree handoff`.
pub fn run(cwd: &Path, raw_id: &str, input: Input) -> Result<Report, CliError> {
    let repo_root = require_repo_root(cwd)?;
    let id = TaskId::parse(raw_id)
        .ok_or_else(|| CliError::new(USAGE, format!("'{raw_id}' is not a task id")))?;
    let task = registry::read_task(&repo_root, &id)?;
    if task.status != TaskStatus::Active {
        return Err(CliError::new(
            TASK,
            format!("{id} is {}; only active tasks take handoffs", task.status),
        ));
    }

    let git = CliGitOps::new(&repo_root);
    let record = match input {
        Input::JsonFile(path) => parse_json(&read_input(&path)?)?,
        Input::Set(pairs) => {
            let base = branch_handoff(&git, &task)
                .map_err(|reason| CliError::new(TASK, format!("{id}: {reason}")))?
                .unwrap_or_default();
            apply_sets(base, &pairs)?
        }
    };
    let commit = commit_on_branch(&repo_root, &git, &task, &record)?;
    let agents = agents::render(&repo_root)?;
    Ok(Report {
        task,
        commit,
        agents,
    })
}

fn read_input(path: &Path) -> Result<String, CliError> {
    if path.as_os_str() == "-" {
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .map_err(|err| CliError::new(USAGE, format!("stdin: {err}")))?;
        return Ok(text);
    }
    std::fs::read_to_string(path)
        .map_err(|err| CliError::new(USAGE, format!("{}: {err}", path.display())))
}

/// A complete record; unknown fields are rejected rather than dropped.
fn parse_json(text: &str) -> Result<HandoffRecord, CliError> {
    let usage = |msg: String| CliError::new(USAGE, format!("handoff json: {msg}"));
    let value: serde_json::Value = serde_json::from_str(text).map_err(|e| usage(e.to_string()))?;
    let object = value
        .as_object()
        .ok_or_else(|| usage("expected a JSON object".to_string()))?;
    if let Some(key) = object.keys().find(|k| !KEYS.contains(&k.as_str())) {
        return Err(usage(format!(
            "unknown field '{key}' (expected one of {})",
            KEYS.join(", ")
        )));
    }
    serde_json::from_value(value).map_err(|e| usage(e.to_string()))
}

/// Applies `key=value` edits to `record` (see the module docs).
fn apply_sets(mut record: HandoffRecord, pairs: &[String]) -> Result<HandoffRecord, CliError> {
    let usage = |msg: String| CliError::new(USAGE, format!("--set: {msg}"));
    let mut grouped: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for pair in pairs {
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| usage(format!("'{pair}' is not key=value")))?;
        let key = key.trim();
        if !KEYS.contains(&key) {
            return Err(usage(format!(
                "unknown key '{key}' (expected one of {})",
                KEYS.join(", ")
            )));
        }
        grouped.entry(key).or_default().push(value);
    }
    for (key, values) in grouped {
        let items: Vec<String> = values
            .iter()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .collect();
        match key {
            "objective" | "state" => {
                if values.len() > 1 {
                    return Err(usage(format!("'{key}' given more than once")));
                }
                let text = values[0].trim().to_string();
                if key == "objective" {
                    record.objective = text;
                } else {
                    record.state = text;
                }
            }
            "done" => record.done = items,
            "next" => record.next = items,
            "failing_tests" => record.failing_tests = items,
            "ruled_out" => record.ruled_out = items,
            "decisions" => {
                record.decisions = items
                    .iter()
                    .map(|item| match item.split_once('|') {
                        Some((what, why)) if !what.trim().is_empty() && !why.trim().is_empty() => {
                            Ok(Decision {
                                what: what.trim().to_string(),
                                why: why.trim().to_string(),
                            })
                        }
                        _ => Err(usage(format!("decision '{item}' must be <what>|<why>"))),
                    })
                    .collect::<Result<_, _>>()?;
            }
            _ => unreachable!("keys are checked above"),
        }
    }
    Ok(record)
}

/// Commits `record` on `task`'s branch and fast-forwards its worktree.
/// Returns `None` when the branch already holds exactly these bytes.
fn commit_on_branch(
    repo_root: &Path,
    git: &CliGitOps,
    task: &Task,
    record: &HandoffRecord,
) -> Result<Option<String>, CliError> {
    let branch_ref = format!("refs/heads/{}", task.branch);
    let worktree_path = repo_root.join(&task.worktree);
    let worktree = CliGitOps::new(&worktree_path);
    match worktree.head_branch() {
        Ok(Some(head)) if head == branch_ref => {}
        Ok(head) => {
            return Err(CliError::new(
                TASK,
                format!(
                    "{}: worktree {} is on {} instead of {branch_ref}; check it out to record a handoff",
                    task.id,
                    worktree_path.display(),
                    head.as_deref().unwrap_or("a detached HEAD")
                ),
            ))
        }
        Err(err) => {
            return Err(CliError::new(
                PLATFORM,
                format!("{}: task worktree unavailable: {err}", task.id),
            ))
        }
    }

    let tip = git.rev_parse(&branch_ref).map_err(git_error)?;
    let tip_tree = git
        .rev_parse(&format!("{tip}^{{tree}}"))
        .map_err(git_error)?;
    let files = [
        ("handoff.json", record.to_json()),
        (
            "HANDOFF.md",
            render_handoff_md(&task.id, &task.name, record),
        ),
    ];
    let mut leaf = dir_entries(git, &tip, &format!(".meshtree/tasks/{}", task.id))?;
    leaf.retain(|e| !files.iter().any(|(name, _)| e.path == *name));
    for (name, text) in &files {
        let blob = git.hash_object(text.as_bytes()).map_err(git_error)?;
        leaf.push(TreeEntry::blob(*name, blob));
    }
    // Rebuild only the spine root → .meshtree → tasks → <id>.
    let spine = [
        "".to_string(),
        ".meshtree".to_string(),
        ".meshtree/tasks".to_string(),
    ];
    let names = [
        ".meshtree".to_string(),
        "tasks".to_string(),
        task.id.to_string(),
    ];
    let mut child = git.mktree(&leaf).map_err(git_error)?;
    for (dir, name) in spine.iter().zip(&names).rev() {
        let mut entries = dir_entries(git, &tip, dir)?;
        if let Some(clash) = entries.iter().find(|e| e.path == *name && e.kind != "tree") {
            return Err(CliError::new(
                TASK,
                format!(
                    "{}: '{}' on {branch_ref} is a {}, not a directory",
                    task.id,
                    if dir.is_empty() {
                        name.clone()
                    } else {
                        format!("{dir}/{name}")
                    },
                    clash.kind
                ),
            ));
        }
        entries.retain(|e| e.path != *name);
        entries.push(TreeEntry::tree(name.clone(), child));
        child = git.mktree(&entries).map_err(git_error)?;
    }
    if child == tip_tree {
        return Ok(None);
    }
    let message = format!("meshtree: handoff {}", task.id);
    let commit = git
        .commit_tree(&child, &[tip.as_str()], &message)
        .map_err(git_error)?;
    worktree.merge_ff_only(&commit).map_err(|err| {
        CliError::new(
            TASK,
            format!(
                "{}: handoff not committed; the worktree could not fast-forward \
                 (uncommitted edits to {} or {}, or the branch moved): {err}",
                task.id,
                handoff_path(task, "handoff.json"),
                handoff_path(task, "HANDOFF.md")
            ),
        )
    })?;
    Ok(Some(commit))
}

/// Direct children of `dir` (`""` = root) in commit `tip`, with
/// single-component names; empty if `dir` does not exist there.
fn dir_entries(git: &CliGitOps, tip: &str, dir: &str) -> Result<Vec<TreeEntry>, CliError> {
    let treeish = if dir.is_empty() {
        format!("{tip}^{{tree}}")
    } else {
        format!("{tip}:{dir}")
    };
    let Ok(tree) = git.rev_parse(&treeish) else {
        return Ok(Vec::new());
    };
    // `ls_tree` is recursive: direct blobs/links/submodules have no '/', and
    // each first path component with one is a direct subtree.
    let mut entries = Vec::new();
    let mut subdirs: Vec<String> = Vec::new();
    for entry in git.ls_tree(&tree).map_err(git_error)? {
        match entry.path.split_once('/') {
            None => entries.push(entry),
            Some((first, _)) => {
                if !subdirs.iter().any(|s| s == first) {
                    subdirs.push(first.to_string());
                }
            }
        }
    }
    for name in subdirs {
        let oid = git
            .rev_parse(&format!("{tree}:{name}"))
            .map_err(git_error)?;
        entries.push(TreeEntry::tree(name, oid));
    }
    Ok(entries)
}
