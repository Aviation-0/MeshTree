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
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

/// Error code for a rejected coordination-ref CAS per `SPEC.md` §16.
pub const ERROR_CODE_CAS_REJECTED: &str = "MT-003";

/// Fixed identity for machine-written coordination commits (`commit_tree`),
/// so they never depend on the host's git config.
const COORD_IDENTITY: (&str, &str) = ("MeshTree", "meshtree@localhost");

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
    /// A `push --force-with-lease` was rejected because the remote ref no
    /// longer matched the expected old OID (`None` = expected absent) —
    /// another machine won the race (`SPEC.md` §9, `MT-003`).
    RemoteCasRejected {
        remote: String,
        reference: String,
        expected_old: Option<String>,
    },
}

impl GitError {
    /// MeshTree error code (`SPEC.md` §16), if this error has one.
    pub fn code(&self) -> Option<&'static str> {
        match self {
            GitError::CasRejected { .. } | GitError::RemoteCasRejected { .. } => {
                Some(ERROR_CODE_CAS_REJECTED)
            }
            _ => None,
        }
    }
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
            GitError::RemoteCasRejected {
                remote,
                reference,
                expected_old,
            } => write!(
                f,
                "[{ERROR_CODE_CAS_REJECTED}] push of `{reference}` to `{remote}` rejected: \
                 expected old OID {} is stale",
                expected_old.as_deref().unwrap_or("(absent)")
            ),
        }
    }
}

/// One entry of a git tree, as listed by `ls-tree` and consumed by `mktree`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    /// File mode, e.g. `100644` (blob) or `040000` (tree).
    pub mode: String,
    /// Object type: `blob` or `tree`.
    pub kind: String,
    /// Object ID.
    pub oid: String,
    /// Name (for `mktree`, a single path component) or full path (`ls_tree`).
    pub path: String,
}

impl TreeEntry {
    /// A regular-file blob entry.
    pub fn blob(name: impl Into<String>, oid: impl Into<String>) -> Self {
        Self {
            mode: "100644".to_string(),
            kind: "blob".to_string(),
            oid: oid.into(),
            path: name.into(),
        }
    }

    /// A subtree entry.
    pub fn tree(name: impl Into<String>, oid: impl Into<String>) -> Self {
        Self {
            mode: "040000".to_string(),
            kind: "tree".to_string(),
            oid: oid.into(),
            path: name.into(),
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
    /// `git rev-parse --verify --quiet <reference>` → OID, or `None` when
    /// the ref does not exist.
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
    /// `git config --get remote.<name>.url`, or `None` if no such remote.
    fn remote_url(&self, name: &str) -> Result<Option<String>, GitError>;
    /// `git hash-object -w --stdin` → blob OID.
    fn hash_object(&self, contents: &[u8]) -> Result<String, GitError>;
    /// `git mktree -z` over single-level `entries` → tree OID.
    fn mktree(&self, entries: &[TreeEntry]) -> Result<String, GitError>;
    /// `git commit-tree <tree> [-p <parent>]… -m <message>` → commit OID,
    /// authored by a fixed MeshTree identity.
    fn commit_tree(&self, tree: &str, parents: &[&str], message: &str) -> Result<String, GitError>;
    /// `git ls-tree -r -z <treeish>` → every blob, with full paths.
    fn ls_tree(&self, treeish: &str) -> Result<Vec<TreeEntry>, GitError>;
    /// `git cat-file blob <oid>` → raw bytes.
    fn cat_blob(&self, oid: &str) -> Result<Vec<u8>, GitError>;
    /// The contents of several blobs, in order. The default reads them one
    /// by one; [`CliGitOps`] uses one `git cat-file --batch`.
    fn cat_blobs(&self, oids: &[String]) -> Result<Vec<Vec<u8>>, GitError> {
        oids.iter().map(|oid| self.cat_blob(oid)).collect()
    }
    /// Writes several blobs, returning their OIDs in order. The default
    /// writes them one by one; [`CliGitOps`] uses one
    /// `git hash-object -w --stdin-paths`.
    fn hash_objects(&self, contents: &[&[u8]]) -> Result<Vec<String>, GitError> {
        contents.iter().map(|c| self.hash_object(c)).collect()
    }
    /// Fetches `src` from `remote` into local ref `dst` (forced) in one
    /// network round trip. Returns the fetched OID, or `None` if `remote` has
    /// no `src` (a `dst` left from an earlier fetch is then pruned).
    fn fetch_ref(&self, remote: &str, src: &str, dst: &str) -> Result<Option<String>, GitError>;
    /// Cross-machine CAS (Appendix A): `git push <remote> <new-oid>:<reference>
    /// --force-with-lease=<reference>:<expected-old>`; `None` requires the
    /// remote ref to be absent. A lost race is [`GitError::RemoteCasRejected`].
    fn push_ref_cas(
        &self,
        remote: &str,
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
        self.run_with(args, None, &[])
    }

    /// Runs `git -C <repo> <args>` with optional stdin bytes and extra env.
    fn run_with(
        &self,
        args: &[&OsStr],
        stdin: Option<&[u8]>,
        envs: &[(&str, &str)],
    ) -> Result<Output, GitError> {
        let context = self.context(args);
        let spawn_err = |source| GitError::Spawn {
            context: context.clone(),
            source,
        };
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(&self.repo)
            .args(args)
            // Untranslated messages: stderr is classified by content
            // (`lost_push_race`), which a localized git would defeat.
            .env("LC_ALL", "C")
            .envs(envs.iter().copied())
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().map_err(spawn_err)?;
        // Feed stdin from a second thread: batch commands (`cat-file
        // --batch`) write output while still reading input, so writing
        // everything first could block on a full stdout pipe. A write error
        // surfaces as git's exit status.
        let pipe = child.stdin.take();
        let output = std::thread::scope(|s| {
            if let (Some(bytes), Some(mut pipe)) = (stdin, pipe) {
                s.spawn(move || {
                    let _ = pipe.write_all(bytes);
                });
            }
            child.wait_with_output()
        })
        .map_err(spawn_err)?;
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
            OsStr::new("rev-parse"),
            OsStr::new("--verify"),
            OsStr::new("--quiet"),
            OsStr::new(reference),
        ]) {
            Ok(output) => Ok(Some(
                String::from_utf8_lossy(&output.stdout).trim().to_string(),
            )),
            // `rev-parse --verify --quiet` exits 1 (silently) when the ref
            // does not exist. (`show-ref --verify` exits 128 there, which is
            // why rev-parse is used instead.)
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
                // A CAS failure is a lost race only if the ref no longer holds
                // the expected value; anything else (bad object, lock file,
                // permissions) is reported as the underlying error.
                Some(old) => {
                    let current = self.ref_read(reference)?;
                    let unchanged = match current.as_deref() {
                        None => old.bytes().all(|b| b == b'0'),
                        Some(oid) => oid == old,
                    };
                    if unchanged {
                        Err(e)
                    } else {
                        Err(GitError::CasRejected {
                            reference: reference.to_string(),
                            expected_old: old.to_string(),
                        })
                    }
                }
                None => Err(e),
            },
        }
    }

    fn remote_url(&self, name: &str) -> Result<Option<String>, GitError> {
        let key = format!("remote.{name}.url");
        match self.run(&[OsStr::new("config"), OsStr::new("--get"), OsStr::new(&key)]) {
            Ok(output) => Ok(Some(stdout_line(&output))),
            // `git config --get` exits 1 when the key is unset.
            Err(GitError::NonZeroExit { code: Some(1), .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn hash_object(&self, contents: &[u8]) -> Result<String, GitError> {
        let output = self.run_with(
            &[
                OsStr::new("hash-object"),
                OsStr::new("-w"),
                OsStr::new("--no-filters"),
                OsStr::new("--stdin"),
            ],
            Some(contents),
            &[],
        )?;
        Ok(stdout_line(&output))
    }

    fn mktree(&self, entries: &[TreeEntry]) -> Result<String, GitError> {
        let mut input = Vec::new();
        for e in entries {
            input.extend_from_slice(
                format!("{} {} {}\t{}", e.mode, e.kind, e.oid, e.path).as_bytes(),
            );
            input.push(0);
        }
        let output = self.run_with(&[OsStr::new("mktree"), OsStr::new("-z")], Some(&input), &[])?;
        Ok(stdout_line(&output))
    }

    fn commit_tree(&self, tree: &str, parents: &[&str], message: &str) -> Result<String, GitError> {
        let mut args: Vec<&OsStr> = vec![
            OsStr::new("commit-tree"),
            OsStr::new("--no-gpg-sign"),
            OsStr::new(tree),
        ];
        for parent in parents {
            args.push(OsStr::new("-p"));
            args.push(OsStr::new(parent));
        }
        args.push(OsStr::new("-m"));
        args.push(OsStr::new(message));
        let (name, email) = COORD_IDENTITY;
        let output = self.run_with(
            &args,
            None,
            &[
                ("GIT_AUTHOR_NAME", name),
                ("GIT_AUTHOR_EMAIL", email),
                ("GIT_COMMITTER_NAME", name),
                ("GIT_COMMITTER_EMAIL", email),
            ],
        )?;
        Ok(stdout_line(&output))
    }

    fn ls_tree(&self, treeish: &str) -> Result<Vec<TreeEntry>, GitError> {
        let output = self.run(&[
            OsStr::new("ls-tree"),
            OsStr::new("-r"),
            OsStr::new("-z"),
            OsStr::new(treeish),
        ])?;
        let mut entries = Vec::new();
        // Records: "<mode> SP <type> SP <oid> TAB <path>" NUL-terminated.
        for record in output.stdout.split(|&b| b == 0).filter(|r| !r.is_empty()) {
            let record = String::from_utf8_lossy(record);
            let (meta, path) = record.split_once('\t').unwrap_or((&*record, ""));
            let mut fields = meta.split(' ');
            entries.push(TreeEntry {
                mode: fields.next().unwrap_or_default().to_string(),
                kind: fields.next().unwrap_or_default().to_string(),
                oid: fields.next().unwrap_or_default().to_string(),
                path: path.to_string(),
            });
        }
        Ok(entries)
    }

    fn cat_blob(&self, oid: &str) -> Result<Vec<u8>, GitError> {
        let output = self.run(&[OsStr::new("cat-file"), OsStr::new("blob"), OsStr::new(oid)])?;
        Ok(output.stdout)
    }

    fn cat_blobs(&self, oids: &[String]) -> Result<Vec<Vec<u8>>, GitError> {
        if oids.len() <= 1 {
            return oids.iter().map(|oid| self.cat_blob(oid)).collect();
        }
        let mut input = String::new();
        for oid in oids {
            input.push_str(oid);
            input.push('\n');
        }
        let args = [OsStr::new("cat-file"), OsStr::new("--batch")];
        let output = self.run_with(&args, Some(input.as_bytes()), &[])?;
        let bad = |why: String| GitError::NonZeroExit {
            context: self.context(&args),
            code: None,
            stderr: why,
        };
        // Records: "<oid> <type> <size>" LF <content> LF ("<oid> missing" LF).
        let out = &output.stdout;
        let mut at = 0;
        let mut blobs = Vec::with_capacity(oids.len());
        for oid in oids {
            let end = out[at..]
                .iter()
                .position(|&b| b == b'\n')
                .map(|i| at + i)
                .ok_or_else(|| bad(format!("truncated output at {oid}")))?;
            let header = String::from_utf8_lossy(&out[at..end]).into_owned();
            let size: usize = match header.split(' ').collect::<Vec<_>>()[..] {
                [_, "blob", size] => size
                    .parse()
                    .map_err(|_| bad(format!("bad header `{header}`")))?,
                _ => return Err(bad(format!("`{oid}`: {header}"))),
            };
            let start = end + 1;
            let stop = start + size;
            if stop > out.len() {
                return Err(bad(format!("truncated blob {oid}")));
            }
            blobs.push(out[start..stop].to_vec());
            at = stop + 1;
        }
        Ok(blobs)
    }

    fn hash_objects(&self, contents: &[&[u8]]) -> Result<Vec<String>, GitError> {
        if contents.len() <= 1 {
            return contents.iter().map(|c| self.hash_object(c)).collect();
        }
        static BATCH: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "mt-blobs-{}-{}",
            std::process::id(),
            BATCH.fetch_add(1, Ordering::Relaxed)
        ));
        let io_err = |source| GitError::Spawn {
            context: format!("stage blobs in {}", dir.display()),
            source,
        };
        std::fs::create_dir_all(&dir).map_err(io_err)?;
        let result = (|| {
            let mut paths = String::new();
            for (i, bytes) in contents.iter().enumerate() {
                let path = dir.join(i.to_string());
                std::fs::write(&path, bytes).map_err(io_err)?;
                paths.push_str(&path.to_string_lossy());
                paths.push('\n');
            }
            let output = self.run_with(
                &[
                    OsStr::new("hash-object"),
                    OsStr::new("-w"),
                    OsStr::new("--no-filters"),
                    OsStr::new("--stdin-paths"),
                ],
                Some(paths.as_bytes()),
                &[],
            )?;
            let oids: Vec<String> = String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(str::to_string)
                .collect();
            if oids.len() == contents.len() {
                Ok(oids)
            } else {
                Err(GitError::NonZeroExit {
                    context: "git hash-object --stdin-paths".into(),
                    code: None,
                    stderr: format!("expected {} OIDs, got {}", contents.len(), oids.len()),
                })
            }
        })();
        let _ = std::fs::remove_dir_all(&dir);
        result
    }

    fn fetch_ref(&self, remote: &str, src: &str, dst: &str) -> Result<Option<String>, GitError> {
        // One round trip (`T-055`). Fetching a missing ref by name is an
        // error, so `src*` is fetched as a glob instead: an absent ref is
        // simply nothing fetched, `--prune` drops a tracking ref whose remote
        // ref was deleted, and no (localized) error text is parsed. The glob
        // also matches longer names; only `dst` itself is read back.
        let refspec = format!("+{src}*:{dst}*");
        self.run(&[
            OsStr::new("fetch"),
            OsStr::new("--quiet"),
            OsStr::new("--no-tags"),
            OsStr::new("--prune"),
            OsStr::new(remote),
            OsStr::new(&refspec),
        ])?;
        self.ref_read(dst)
    }

    fn push_ref_cas(
        &self,
        remote: &str,
        reference: &str,
        new_oid: &str,
        expected_old: Option<&str>,
    ) -> Result<(), GitError> {
        let refspec = format!("{new_oid}:{reference}");
        let lease = format!(
            "--force-with-lease={reference}:{}",
            expected_old.unwrap_or("")
        );
        // Neither `--porcelain` nor `--quiet`: the per-ref rejection reason
        // must land on stderr, which `NonZeroExit` captures.
        let args = [
            OsStr::new("push"),
            OsStr::new(&lease),
            OsStr::new(remote),
            OsStr::new(&refspec),
        ];
        match self.run(&args) {
            Ok(_) => Ok(()),
            Err(GitError::NonZeroExit { stderr, .. }) if lost_push_race(&stderr) => {
                Err(GitError::RemoteCasRejected {
                    remote: remote.to_string(),
                    reference: reference.to_string(),
                    expected_old: expected_old.map(str::to_string),
                })
            }
            Err(e) => Err(e),
        }
    }
}

/// Result of [`MergeOps::merge_no_ff`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    /// The merge commit now at `HEAD`.
    Merged(String),
    /// Conflicting paths; the merge was aborted and `HEAD` is unchanged.
    Conflict(Vec<String>),
}

/// Plumbing for the serialized merge gate (`SPEC.md` §8 `collect`,
/// `meshtree promote`), kept apart from [`GitOps`] so that trait's test
/// doubles stay small. Each method runs in the repository or worktree the
/// [`CliGitOps`] was created for.
pub trait MergeOps {
    /// `git diff --name-only -z <base>...<branch>`: paths changed on `branch`
    /// since it diverged from `base`.
    fn diff_names(&self, base: &str, branch: &str) -> Result<Vec<String>, GitError>;
    /// `git merge-base --is-ancestor <ancestor> <descendant>`.
    fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool, GitError>;
    /// `git worktree add --detach <path> <rev>`.
    fn worktree_add_detached(&self, path: &Path, rev: &str) -> Result<(), GitError>;
    /// `git reset --hard <rev>` then `git clean -fdq` (a pristine checkout).
    fn reset_clean(&self, rev: &str) -> Result<(), GitError>;
    /// `git merge --no-ff --no-edit -m <message> <branch>` into the current
    /// `HEAD`. On conflict the merge is aborted (`git merge --abort`).
    fn merge_no_ff(&self, branch: &str, message: &str) -> Result<MergeOutcome, GitError>;
    /// `git merge --ff-only <rev>` into the checked-out branch.
    fn merge_ff_only(&self, rev: &str) -> Result<(), GitError>;
    /// The branch `HEAD` is attached to (`refs/heads/…`), or `None` if detached.
    fn head_branch(&self) -> Result<Option<String>, GitError>;
    /// `git worktree prune`: forget worktrees whose directory is gone.
    fn worktree_prune(&self) -> Result<(), GitError>;
}

impl CliGitOps {
    /// `-c user.name=… -c user.email=…` with MeshTree's identity, unless the
    /// repository already has one: merge commits must not fail on hosts
    /// (CI) with no git identity.
    fn identity_args(&self) -> Result<Vec<String>, GitError> {
        match self.run(&[OsStr::new("config"), OsStr::new("user.email")]) {
            Ok(_) => Ok(Vec::new()),
            Err(GitError::NonZeroExit { code: Some(1), .. }) => Ok(vec![
                "-c".into(),
                format!("user.name={}", COORD_IDENTITY.0),
                "-c".into(),
                format!("user.email={}", COORD_IDENTITY.1),
            ]),
            Err(e) => Err(e),
        }
    }
}

impl MergeOps for CliGitOps {
    fn diff_names(&self, base: &str, branch: &str) -> Result<Vec<String>, GitError> {
        let range = format!("{base}...{branch}");
        let output = self.run(&[
            OsStr::new("diff"),
            OsStr::new("--name-only"),
            OsStr::new("-z"),
            OsStr::new(&range),
            OsStr::new("--"),
        ])?;
        Ok(output
            .stdout
            .split(|&b| b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect())
    }

    fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool, GitError> {
        match self.run(&[
            OsStr::new("merge-base"),
            OsStr::new("--is-ancestor"),
            OsStr::new(ancestor),
            OsStr::new(descendant),
        ]) {
            Ok(_) => Ok(true),
            Err(GitError::NonZeroExit { code: Some(1), .. }) => Ok(false),
            Err(e) => Err(e),
        }
    }

    fn worktree_add_detached(&self, path: &Path, rev: &str) -> Result<(), GitError> {
        self.run_ok(&[
            OsStr::new("worktree"),
            OsStr::new("add"),
            OsStr::new("--detach"),
            path.as_os_str(),
            OsStr::new(rev),
        ])
    }

    fn reset_clean(&self, rev: &str) -> Result<(), GitError> {
        self.run_ok(&[
            OsStr::new("reset"),
            OsStr::new("--hard"),
            OsStr::new("-q"),
            OsStr::new(rev),
        ])?;
        self.run_ok(&[OsStr::new("clean"), OsStr::new("-fdq")])
    }

    fn merge_no_ff(&self, branch: &str, message: &str) -> Result<MergeOutcome, GitError> {
        let identity = self.identity_args()?;
        let mut args: Vec<&OsStr> = identity.iter().map(OsStr::new).collect();
        args.extend([
            OsStr::new("merge"),
            OsStr::new("--no-ff"),
            OsStr::new("--no-edit"),
            OsStr::new("-m"),
            OsStr::new(message),
            OsStr::new(branch),
        ]);
        match self.run(&args) {
            Ok(_) => Ok(MergeOutcome::Merged(self.rev_parse("HEAD")?)),
            Err(err) => {
                let unmerged = self.run(&[
                    OsStr::new("diff"),
                    OsStr::new("--name-only"),
                    OsStr::new("-z"),
                    OsStr::new("--diff-filter=U"),
                ])?;
                let paths: Vec<String> = unmerged
                    .stdout
                    .split(|&b| b == 0)
                    .filter(|p| !p.is_empty())
                    .map(|p| String::from_utf8_lossy(p).into_owned())
                    .collect();
                if paths.is_empty() {
                    return Err(err);
                }
                self.run_ok(&[OsStr::new("merge"), OsStr::new("--abort")])?;
                Ok(MergeOutcome::Conflict(paths))
            }
        }
    }

    fn merge_ff_only(&self, rev: &str) -> Result<(), GitError> {
        self.run_ok(&[
            OsStr::new("merge"),
            OsStr::new("--ff-only"),
            OsStr::new("-q"),
            OsStr::new(rev),
        ])
    }

    fn head_branch(&self) -> Result<Option<String>, GitError> {
        match self.run(&[
            OsStr::new("symbolic-ref"),
            OsStr::new("-q"),
            OsStr::new("HEAD"),
        ]) {
            Ok(output) => Ok(Some(stdout_line(&output))),
            Err(GitError::NonZeroExit { code: Some(1), .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn worktree_prune(&self) -> Result<(), GitError> {
        self.run_ok(&[OsStr::new("worktree"), OsStr::new("prune")])
    }
}

/// `true` if a failed `push --force-with-lease` lost a race rather than
/// failing for network/auth reasons. Two shapes exist:
/// - client side: the advertised remote value no longer matches the lease
///   (`[rejected] … (stale info)`, or `(fetch first)` for non-fast-forward);
/// - server side: both pushers passed the client check concurrently and the
///   remote's ref lock refused the loser (`[remote rejected] … (failed to
///   update ref | cannot lock ref | incorrect old value | reference already
///   exists)`).
fn lost_push_race(stderr: &str) -> bool {
    let client_side = stderr.contains("stale info")
        || stderr.contains("[rejected]")
        || stderr.contains("fetch first");
    let server_side = stderr.contains("[remote rejected]")
        && [
            "failed to update ref",
            "cannot lock ref",
            "incorrect old value",
            "already exists",
            "failed to lock",
        ]
        .iter()
        .any(|reason| stderr.contains(reason));
    client_side || server_side
}

/// First line of stdout, trimmed (an OID for the plumbing commands above).
fn stdout_line(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::lost_push_race;

    #[test]
    fn push_race_classification() {
        for lost in [
            " ! [rejected]        abc -> refs/meshtree/coord (stale info)\nerror: failed to push some refs",
            " ! [rejected]        abc -> refs/meshtree/coord (fetch first)",
            " ! [remote rejected] abc -> refs/meshtree/coord (failed to update ref)",
            " ! [remote rejected] abc -> refs/meshtree/coord (cannot lock ref 'refs/meshtree/coord': reference already exists)",
            " ! [remote rejected] abc -> refs/meshtree/coord (incorrect old value provided)",
        ] {
            assert!(lost_push_race(lost), "{lost}");
        }
        for other in [
            "fatal: '/nonexistent' does not appear to be a git repository\nfatal: Could not read from remote repository.",
            "fatal: Authentication failed for 'https://example.com/repo.git/'",
            " ! [remote rejected] abc -> refs/meshtree/coord (pre-receive hook declined)",
        ] {
            assert!(!lost_push_race(other), "{other}");
        }
    }
}
