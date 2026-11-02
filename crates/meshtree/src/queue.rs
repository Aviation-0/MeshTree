//! `meshtree queue <queue-name> request --task <task-id> [-- <args>…]`.
//!
//! Authoritative specification: `SPEC.md` §12 (per-queue FIFO, slot held
//! until the command exits; a dead holder is evicted by pid + boot id), §14,
//! §17 and `ADR-0005` (only allow-listed argument overrides).
//!
//! The FIFO lives in the daemon, so this command requires one (`MT-010`
//! otherwise). It requests a ticket for this process, waits for the grant,
//! runs the queue's configured command inside the task worktree, and
//! releases the slot. If this process is killed, the daemon notices the dead
//! pid and frees the slot.

use crate::daemon;
use crate::exit::{CliError, DAEMON, PLATFORM, TASK};
use crate::repo::require_repo_root;
use meshtree_platform::lock::boot_id;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Interval between ticket polls while waiting for a slot.
const POLL: Duration = Duration::from_millis(50);

fn call(repo_root: &Path, method: &str, params: Value) -> Result<Value, CliError> {
    daemon::call(repo_root, method, params).unwrap_or_else(|| {
        Err(daemon::unreachable(
            "shared-op queues are held by the daemon",
        ))
    })
}

/// Runs `meshtree queue <name> request`.
pub fn request(cwd: &Path, queue: &str, task_id: &str, args: &[String]) -> Result<(), CliError> {
    let repo_root = require_repo_root(cwd)?;
    let boot = boot_id().map_err(|err| CliError::new(PLATFORM, format!("boot id: {err}")))?;
    let mut params = json!({
        "task_id": task_id,
        "queue": queue,
        "pid": std::process::id(),
        "boot_id": boot,
    });
    if !args.is_empty() {
        params["command_override"] = json!(args);
    }
    let ticket = call(&repo_root, "request_shared_op", params)?;
    let field = |key: &str| -> Result<String, CliError> {
        ticket[key]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| CliError::new(DAEMON, format!("malformed ticket from daemon: {ticket}")))
    };
    let (id, command) = (field("id")?, field("command")?);
    let worktree = PathBuf::from(field("worktree")?);
    println!(
        "ticket {id} queue {queue} position {}",
        ticket["position"].as_u64().unwrap_or_default()
    );

    // Wait for the grant; always give the ticket back, granted or not.
    let outcome = wait_for_grant(&repo_root, &id).and_then(|()| {
        println!("granted {id}: {command}");
        run(&command, &worktree)
    });
    let released = call(&repo_root, "release_shared_op", json!({ "ticket_id": id }));
    let status = outcome?;
    released?;
    if status.success() {
        Ok(())
    } else {
        Err(CliError::new(
            TASK,
            format!("queue '{queue}' command failed ({status}): {command}"),
        ))
    }
}

/// Polls ticket `id` until it is granted (also used by `meshtree mcp`).
pub fn wait_for_grant(repo_root: &Path, id: &str) -> Result<(), CliError> {
    loop {
        let ticket = call(repo_root, "ticket_status", json!({ "ticket_id": id }))?;
        if ticket["granted"].as_bool() == Some(true) {
            return Ok(());
        }
        std::thread::sleep(POLL);
    }
}

/// Runs `command` through the platform shell in `dir`, inheriting stdio.
fn run(command: &str, dir: &Path) -> Result<std::process::ExitStatus, CliError> {
    let mut shell = shell(command);
    shell
        .current_dir(dir)
        .status()
        .map_err(|err| CliError::new(PLATFORM, format!("run `{command}`: {err}")))
}

/// `command` run through the platform shell (`cmd /C` on Windows, `sh -c`
/// elsewhere).
#[cfg(windows)]
pub fn shell(command: &str) -> Command {
    use std::os::windows::process::CommandExt;
    let mut cmd = Command::new("cmd");
    // Verbatim: `cmd /C` parses the rest of the line itself.
    cmd.arg("/C").raw_arg(command);
    cmd
}

/// `command` run through the platform shell (`cmd /C` on Windows, `sh -c`
/// elsewhere).
#[cfg(not(windows))]
pub fn shell(command: &str) -> Command {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(command);
    cmd
}
