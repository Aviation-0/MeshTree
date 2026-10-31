mod clock;
mod exit;
mod init;
mod repo;
mod spawn;

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
        Some(Command::Spawn { name, base, agent }) => {
            let report = spawn::run(&current_dir()?, spawn::SpawnArgs { name, base, agent })?;
            println!("task_id: {}", report.task.id);
            println!("branch: {}", report.task.branch);
            println!("worktree: {}", report.worktree_path.display());
            Ok(())
        }
    }
}

fn current_dir() -> Result<std::path::PathBuf, CliError> {
    std::env::current_dir()
        .map_err(|err| CliError::new(exit::PLATFORM, format!("current dir: {err}")))
}
