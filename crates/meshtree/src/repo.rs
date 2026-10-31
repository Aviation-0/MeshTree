//! Repository discovery and `.meshtree/` paths (`SPEC.md` §5).

use crate::exit::{CliError, PLATFORM, USAGE};
use meshtree_core::Config;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Directory holding all MeshTree state inside the repository.
pub const MESHTREE_DIR: &str = ".meshtree";

/// Finds the repository root: the nearest ancestor of `start` containing `.git`
/// (a directory, or a file for linked worktrees and submodules).
pub fn find_repo_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|dir| dir.join(".git").exists())
        .map(Path::to_path_buf)
}

/// Like [`find_repo_root`], but a missing repository is a §14 usage error.
pub fn require_repo_root(cwd: &Path) -> Result<PathBuf, CliError> {
    find_repo_root(cwd).ok_or_else(|| {
        CliError::new(
            USAGE,
            format!(
                "not inside a git repository (no .git found above {})",
                cwd.display()
            ),
        )
    })
}

/// Loads and validates `.meshtree/config.toml`; missing or invalid config is a
/// usage error (`MT-050` is carried in the message for invalid config).
pub fn load_config(repo_root: &Path) -> Result<Config, CliError> {
    let path = repo_root.join(MESHTREE_DIR).join("config.toml");
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Err(CliError::new(
                USAGE,
                format!("{} not found; run `meshtree init` first", path.display()),
            ))
        }
        Err(err) => return Err(platform_error(&path, &err)),
    };
    Config::from_toml_str(&text)
        .map_err(|err| CliError::new(USAGE, format!("{err} ({})", path.display())))
}

/// Maps an I/O failure on `path` to a §14 platform error.
pub fn platform_error(path: &Path, err: &io::Error) -> CliError {
    CliError::new(PLATFORM, format!("{}: {err}", path.display()))
}
