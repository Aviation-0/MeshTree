mod abort;
mod agents;
mod claims;
mod clock;
mod daemon;
mod exit;
mod handoff;
mod init;
mod mcp;
mod merge_gate;
mod prune;
mod queue;
mod registry;
mod repo;
mod spawn;
mod status;
mod worktree;

use clap::error::ErrorKind;
use clap::{Parser, Subcommand};
use exit::CliError;
use std::process::ExitCode;

#[derive(Debug, Parser)]
#[command(name = "meshtree", version, about = "MeshTree workspace coordinator")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Start the background service for this repository
    Daemon,
    /// Write the .meshtree/config.toml template and initialize .meshtree/
    Init,
    /// Create an isolated task worktree and branch, and register the task
    Spawn {
        /// Task name
        name: String,
        /// Base ref (default: project.base_branch)
        #[arg(long)]
        base: Option<String>,
        /// Agent identifier
        #[arg(long)]
        agent: Option<String>,
        /// Path pattern to claim (repeatable)
        #[arg(long = "claim", value_name = "GLOB")]
        claims: Vec<String>,
    },
    /// List tasks from the local registry, or show one task
    Status {
        /// Task ID (default: all tasks)
        task_id: Option<String>,
        /// Print task records as JSON
        #[arg(long)]
        json: bool,
    },
    /// Discard an active task: release claims, remove its worktree, mark it abandoned
    Abort {
        /// Task ID
        task_id: String,
    },
    /// Add path claims to an active task
    Claim {
        /// Task ID
        task_id: String,
        /// Path patterns
        #[arg(required = true, value_name = "GLOB")]
        globs: Vec<String>,
        /// With a daemon: answer only after the claim is committed and pushed
        /// (remote conflicts reported up front)
        #[arg(long)]
        sync: bool,
    },
    /// Release path claims held by an active task
    Release {
        /// Task ID
        task_id: String,
        /// Path patterns
        #[arg(required = true, value_name = "GLOB")]
        globs: Vec<String>,
    },
    /// Validate and merge a task into the integration branch (requires a running daemon)
    Collect {
        /// Task ID
        task_id: String,
        /// Merge commit message
        #[arg(long)]
        message: Option<String>,
    },
    /// Fast-forward the base branch to the integration branch
    Promote,
    /// Remove leftover worktrees of finished tasks and retry quarantined trees
    Prune {
        /// Only leftover worktrees of finished tasks (skip the quarantine GC)
        #[arg(long)]
        stale_only: bool,
    },
    /// Shared-operation FIFO queue (requires a running daemon)
    Queue {
        /// Queue name ([queue.<name>] in config.toml)
        queue: String,
        #[command(subcommand)]
        action: QueueAction,
    },
    /// Flush local claim events and pull remote ones (refs/meshtree/coord)
    Sync,
    /// Commit a handoff record on the task branch and regenerate AGENTS.md
    ///
    /// --set edits the record already on the branch: objective=/state= replace
    /// the text; done/next/failing_tests/ruled_out/decisions are replaced by
    /// the values given for that key, in order (an empty value clears the
    /// list); a decision is decisions=<what>|<why>.
    Handoff {
        /// Task ID
        task_id: String,
        /// Complete handoff.json to commit ("-" reads stdin)
        #[arg(
            long,
            value_name = "PATH",
            conflicts_with = "set",
            required_unless_present = "set"
        )]
        json_file: Option<std::path::PathBuf>,
        /// Edit one field (repeatable)
        #[arg(long = "set", value_name = "KEY=VALUE")]
        set: Vec<String>,
    },
    /// Regenerate the active-sessions block in the root AGENTS.md
    RenderAgents,
    /// Serve the MCP tools on stdin/stdout (each call proxies to the daemon)
    Mcp,
}

#[derive(Debug, Subcommand)]
enum QueueAction {
    /// Wait for a slot, run the queue command in the task worktree, release it
    Request {
        /// Task ID
        #[arg(long)]
        task: String,
        /// Extra arguments (must be in the queue's allow_args)
        #[arg(last = true, value_name = "ARG")]
        args: Vec<String>,
    },
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            // Help/version are not failures; every other parse error is a §14 usage error.
            let code = match err.kind() {
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => exit::OK,
                _ => exit::USAGE,
            };
            let _ = err.print();
            return ExitCode::from(code);
        }
    };

    match run(cli) {
        Ok(()) => ExitCode::from(exit::OK),
        Err(err) => {
            eprintln!("meshtree: {}", err.message);
            ExitCode::from(err.code)
        }
    }
}

fn run(cli: Cli) -> Result<(), CliError> {
    match cli.command {
        None => {
            println!("meshtree {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some(Command::Daemon) => daemon::run(&current_dir()?),
        Some(Command::Init) => {
            let report = init::run(&current_dir()?)?;
            let dir = report.repo_root.join(repo::MESHTREE_DIR);
            for (name, outcome) in [
                ("config.toml", report.config),
                (".gitignore", report.gitignore),
            ] {
                let verb = match outcome {
                    init::FileOutcome::Created => "created",
                    init::FileOutcome::Kept => "kept existing",
                };
                println!("{verb} {}", dir.join(name).display());
            }
            Ok(())
        }
        Some(Command::Spawn {
            name,
            base,
            agent,
            claims,
        }) => {
            let args = spawn::SpawnArgs {
                name,
                base,
                agent,
                claims,
            };
            let report = spawn::run(&current_dir()?, args)?;
            println!("task_id: {}", report.task.id);
            println!("branch: {}", report.task.branch);
            println!("worktree: {}", report.worktree_path.display());
            for outcome in &report.shares {
                match outcome.applied {
                    meshtree_platform::share::Applied::Linked(_) => println!("share: {outcome}"),
                    meshtree_platform::share::Applied::Fallback { .. } => {
                        eprintln!("meshtree: {outcome}")
                    }
                    meshtree_platform::share::Applied::Skipped(_) => {}
                }
            }
            for path in &report.task.claims {
                println!("claim: {path}");
            }
            if let Some(note) = &report.registry_note {
                eprintln!("meshtree: {note}");
            }
            Ok(())
        }
        Some(Command::Status { task_id, json }) => {
            let out = status::run(&current_dir()?, task_id.as_deref(), json)?;
            for warning in &out.warnings {
                eprintln!("meshtree: {warning}");
            }
            print!("{}", out.stdout);
            Ok(())
        }
        Some(Command::Abort { task_id }) => {
            let (task, note) = abort::run(&current_dir()?, &task_id)?;
            println!("{} {}", task.status, task.id);
            if let Some(note) = note {
                eprintln!("meshtree: {note}");
            }
            Ok(())
        }
        Some(Command::Prune { stale_only }) => {
            let report = prune::run(&current_dir()?, stale_only)?;
            for line in &report.stdout {
                println!("{line}");
            }
            for line in &report.notes {
                eprintln!("meshtree: {line}");
            }
            Ok(())
        }
        Some(Command::Claim {
            task_id,
            globs,
            sync,
        }) => {
            let report = claims::claim(&current_dir()?, &task_id, &globs, sync)?;
            print_claim_report("claimed", &report);
            Ok(())
        }
        Some(Command::Release { task_id, globs }) => {
            let report = claims::release_cmd(&current_dir()?, &task_id, &globs)?;
            print_claim_report("released", &report);
            Ok(())
        }
        Some(Command::Queue {
            queue,
            action: QueueAction::Request { task, args },
        }) => queue::request(&current_dir()?, &queue, &task, &args),
        Some(Command::Collect { task_id, message }) => {
            let repo_root = repo::require_repo_root(&current_dir()?)?;
            let params = serde_json::json!({ "task_id": task_id, "message": message });
            let result = daemon::call(&repo_root, "collect_task", params)
                .unwrap_or_else(|| Err(daemon::unreachable("collect runs in the daemon")))?;
            println!(
                "merged {} into {} at {}",
                task_id,
                result["integration_branch"].as_str().unwrap_or_default(),
                result["integration"].as_str().unwrap_or_default()
            );
            if let Some(note) = result["cleanup"].as_str() {
                eprintln!("meshtree: {note}");
            }
            Ok(())
        }
        Some(Command::Promote) => {
            println!("{}", merge_gate::promote(&current_dir()?)?);
            Ok(())
        }
        Some(Command::Handoff {
            task_id,
            json_file,
            set,
        }) => {
            let input = match json_file {
                Some(path) => handoff::Input::JsonFile(path),
                None => handoff::Input::Set(set),
            };
            let report = handoff::run(&current_dir()?, &task_id, input)?;
            match &report.commit {
                Some(commit) => println!(
                    "committed handoff for {} on {} ({commit})",
                    report.task.id, report.task.branch
                ),
                None => println!("handoff for {} unchanged", report.task.id),
            }
            print_rendered(&report.agents);
            Ok(())
        }
        Some(Command::Mcp) => mcp::run(&current_dir()?),
        Some(Command::RenderAgents) => {
            let repo_root = repo::require_repo_root(&current_dir()?)?;
            print_rendered(&agents::render(&repo_root)?);
            Ok(())
        }
        Some(Command::Sync) => {
            let (outcome, revoked, _) = claims::sync(&current_dir()?)?;
            println!("{}", claims::push_status(&outcome));
            println!("leases: {}", outcome.replay.leases.len());
            for r in &revoked {
                eprintln!(
                    "meshtree: [{}] {} lost {} to {}; claim revoked",
                    meshtree_core::ERROR_CODE_CLAIM_CONFLICT,
                    r.task,
                    r.path,
                    r.holder
                );
            }
            Ok(())
        }
    }
}

fn print_rendered(rendered: &agents::Rendered) {
    for warning in &rendered.warnings {
        eprintln!("meshtree: {warning}");
    }
    println!(
        "{} {} ({} active session{})",
        if rendered.changed {
            "updated"
        } else {
            "unchanged"
        },
        rendered.path.display(),
        rendered.sessions,
        if rendered.sessions == 1 { "" } else { "s" }
    );
}

fn print_claim_report(verb: &str, report: &claims::Report) {
    for path in &report.paths {
        println!("{verb} {path}");
    }
    println!("{}", report.coord);
    if let Some(reason) = &report.deferred {
        eprintln!("meshtree: push deferred ({reason}); run `meshtree sync` later");
    }
}

fn current_dir() -> Result<std::path::PathBuf, CliError> {
    std::env::current_dir()
        .map_err(|err| CliError::new(exit::PLATFORM, format!("current dir: {err}")))
}
