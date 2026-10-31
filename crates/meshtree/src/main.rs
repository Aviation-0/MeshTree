mod abort;
mod claims;
mod clock;
mod exit;
mod init;
mod registry;
mod repo;
mod spawn;
mod status;

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
    },
    /// Release path claims held by an active task
    Release {
        /// Task ID
        task_id: String,
        /// Path patterns
        #[arg(required = true, value_name = "GLOB")]
        globs: Vec<String>,
    },
    /// Flush local claim events and pull remote ones (refs/meshtree/coord)
    Sync,
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
            for path in &report.task.claims {
                println!("claim: {path}");
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
            let task = abort::run(&current_dir()?, &task_id)?;
            println!("{} {}", task.status, task.id);
            Ok(())
        }
        Some(Command::Claim { task_id, globs }) => {
            let report = claims::claim(&current_dir()?, &task_id, &globs)?;
            print_claim_report("claimed", &report);
            Ok(())
        }
        Some(Command::Release { task_id, globs }) => {
            let report = claims::release_cmd(&current_dir()?, &task_id, &globs)?;
            print_claim_report("released", &report);
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

fn print_claim_report(verb: &str, report: &claims::Report) {
    for path in &report.paths {
        println!("{verb} {path}");
    }
    println!("{}", claims::push_status(&report.outcome));
    if let Some(reason) = &report.outcome.deferred {
        eprintln!("meshtree: push deferred ({reason}); run `meshtree sync` later");
    }
}

fn current_dir() -> Result<std::path::PathBuf, CliError> {
    std::env::current_dir()
        .map_err(|err| CliError::new(exit::PLATFORM, format!("current dir: {err}")))
}
