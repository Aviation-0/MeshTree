mod exit;
mod init;

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
            let cwd = std::env::current_dir()
                .map_err(|err| CliError::new(exit::PLATFORM, format!("current dir: {err}")))?;
            let report = init::run(&cwd)?;
            let dir = report.repo_root.join(init::MESHTREE_DIR);
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
    }
}
