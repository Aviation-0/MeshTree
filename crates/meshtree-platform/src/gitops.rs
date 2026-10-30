//! Git plumbing via the `git` CLI behind the [`GitOps`] trait.
//!
//! Per `SPEC.md` §4 and Appendix A, all git operations shell out to the
//! `git` binary (machine-readable `--porcelain` / `-z` flags where useful)
//! instead of libgit2/gix: the CLI is the reference implementation and is
//! available everywhere git is. Placement of this module in
//! `meshtree-platform` is recorded in `docs/adr/0001-gitops-placement.md`
//! (ADR-0001, which amends `SPEC.md` Appendix B).

use std::error::Error;
use std::ffi::OsStr;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Errors returned by [`GitOps`] implementations.
#[derive(Debug)]
pub enum GitError {
    /// The `git` binary could not be spawned.
    Spawn {
        context: String,
        source: std::io::Error,
    },
    /// `git` exited non-zero.
    NonZeroExit {
        context: String,
        code: Option<i32>,
        stderr: String,
    },
    /// A compare-and-swap `update-ref` was rejected because the expected old
    /// OID did not match the current ref value (`SPEC.md` §9, Appendix A).
    CasRejected {
        reference: String,
        expected_old: String,
    },
}

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GitError::Spawn { context, source } => {
                write!(f, "failed to spawn `{context}`: {source}")
            }
            GitError::NonZeroExit {
                context,
                code,
                stderr,
            } => write!(f, "`{context}` failed (exit {code:?}): {stderr}"),
            GitError::CasRejected {
                reference,
                expected_old,
            } => write!(
                f,
                "CAS rejected for `{reference}`: expected old OID `{expected_old}` is stale"
            ),
        }
    }
}

impl Error for GitError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            GitError::Spawn { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// One entry of `git worktree list --porcelain`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    pub head: String,
    /// `Some("refs/heads/<name>")` when the worktree is attached to a branch,
    /// `None` when detached.
    pub branch: Option<String>,
}

/// Git operations MeshTree needs, all backed by the `git` CLI (`SPEC.md` §4).
pub trait GitOps {
    /// `git worktree add -b <branch> <path> <base>` (Appendix A).
    fn worktree_add(&self, path: &Path, branch: &str, base: &str) -> Result<(), GitError>;
    /// `git worktree remove --force <path>` (Appendix A).
    fn worktree_remove(&self, path: &Path) -> Result<(), GitError>;
    /// `git worktree list --porcelain`.
    fn worktree_list(&self) -> Result<Vec<WorktreeInfo>, GitError>;
    /// `git rev-parse <rev>` → resolved OID.
    fn rev_parse(&self, rev: &str) -> Result<String, GitError>;
    /// `git show-ref --verify --hash <reference>` → OID, or `None` when the
    /// ref does not exist.
    fn ref_read(&self, reference: &str) -> Result<Option<String>, GitError>;
    /// `git update-ref <reference> <new-oid> [<expected-old-oid>]` — the local
    /// CAS primitive (Appendix A). `Some(expected_old)` must match the
    /// current value or the update is rejected with [`GitError::CasRejected`].
    fn update_ref(
        &self,
        reference: &str,
        new_oid: &str,
        expected_old: Option<&str>,
    ) -> Result<(), GitError>;
}

/// [`GitOps`] backend that shells out to the `git` CLI in `repo`.
pub struct CliGitOps {
    repo: PathBuf,
}

impl CliGitOps {
    pub fn new(repo: impl Into<PathBuf>) -> Self {
        Self { repo: repo.into() }
    }

    fn context(&self, args: &[&OsStr]) -> String {
        let joined = args
            .iter()
            .map(|a| a.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");
        format!("git -C {} {joined}", self.repo.display())
    }

    fn run(&self, args: &[&OsStr]) -> Result<Output, GitError> {
        let context = self.context(args);
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.repo)
            .args(args)
            .output()
            .map_err(|source| GitError::Spawn {
                context: context.clone(),
                source,
            })?;
        if output.status.success() {
            Ok(output)
        } else {
            Err(GitError::NonZeroExit {
                context,
                code: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            })
        }
    }

    fn run_ok(&self, args: &[&OsStr]) -> Result<(), GitError> {
        self.run(args)?;
        Ok(())
    }
}

impl GitOps for CliGitOps {
    fn worktree_add(&self, path: &Path, branch: &str, base: &str) -> Result<(), GitError> {
        self.run_ok(&[
            OsStr::new("worktree"),
            OsStr::new("add"),
            OsStr::new("-b"),
            OsStr::new(branch),
            path.as_os_str(),
            OsStr::new(base),
        ])
    }

    fn worktree_remove(&self, path: &Path) -> Result<(), GitError> {
        self.run_ok(&[
            OsStr::new("worktree"),
            OsStr::new("remove"),
            OsStr::new("--force"),
            path.as_os_str(),
        ])
    }

    fn worktree_list(&self) -> Result<Vec<WorktreeInfo>, GitError> {
        let output = self.run(&[
            OsStr::new("worktree"),
            OsStr::new("list"),
            OsStr::new("--porcelain"),
        ])?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut entries = Vec::new();
        let mut path: Option<PathBuf> = None;
        let mut head: Option<String> = None;
        let mut branch: Option<String> = None;
        // `--porcelain` separates entries with a blank line; chain one so the
        // final entry is flushed too.
        for line in stdout.lines().chain(std::iter::once("")) {
            if line.is_empty() {
                if let (Some(p), Some(h)) = (path.take(), head.take()) {
                    entries.push(WorktreeInfo {
                        path: p,
                        head: h,
                        branch: branch.take(),
                    });
                }
                continue;
            }
            if let Some(v) = line.strip_prefix("worktree ") {
                path = Some(PathBuf::from(v));
            } else if let Some(v) = line.strip_prefix("HEAD ") {
                head = Some(v.to_string());
            } else if let Some(v) = line.strip_prefix("branch ") {
                branch = Some(v.to_string());
            }
            // `detached`, `bare`, `locked` and other keys are not needed here.
        }
        Ok(entries)
    }

    fn rev_parse(&self, rev: &str) -> Result<String, GitError> {
        let output = self.run(&[OsStr::new("rev-parse"), OsStr::new(rev)])?;
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    fn ref_read(&self, reference: &str) -> Result<Option<String>, GitError> {
        match self.run(&[
            OsStr::new("show-ref"),
            OsStr::new("--verify"),
            OsStr::new("--hash"),
            OsStr::new(reference),
        ]) {
            Ok(output) => Ok(Some(
                String::from_utf8_lossy(&output.stdout).trim().to_string(),
            )),
            // `show-ref --verify` exits 1 when the ref does not exist.
            Err(GitError::NonZeroExit { code: Some(1), .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn update_ref(
        &self,
        reference: &str,
        new_oid: &str,
        expected_old: Option<&str>,
    ) -> Result<(), GitError> {
        let mut args: Vec<&OsStr> = vec![
            OsStr::new("update-ref"),
            OsStr::new(reference),
            OsStr::new(new_oid),
        ];
        if let Some(old) = expected_old {
            args.push(OsStr::new(old));
        }
        match self.run(&args) {
            Ok(_) => Ok(()),
            Err(e) => match expected_old {
                // Any failure of a CAS update means the expected old OID was stale.
                Some(old) => Err(GitError::CasRejected {
                    reference: reference.to_string(),
                    expected_old: old.to_string(),
                }),
                None => Err(e),
            },
        }
    }
}
