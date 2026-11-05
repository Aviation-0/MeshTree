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
use std::io::{BufRead, BufReader, Read, Write};
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
    /// Every blob under `treeish` as `(path, oid, bytes)`, in `ls-tree -r`
    /// order. The default is [`GitOps::ls_tree`] plus [`GitOps::cat_blobs`];
    /// [`CliGitOps`] walks the tree in one `git cat-file --batch` (`T-057`).
    fn tree_blobs(&self, treeish: &str) -> Result<Vec<(String, String, Vec<u8>)>, GitError> {
        let entries: Vec<TreeEntry> = self
            .ls_tree(treeish)?
            .into_iter()
            .filter(|e| e.kind == "blob")
            .collect();
        let oids: Vec<String> = entries.iter().map(|e| e.oid.clone()).collect();
        let blobs = self.cat_blobs(&oids)?;
        Ok(entries
            .into_iter()
            .zip(blobs)
            .map(|(e, bytes)| (e.path, e.oid, bytes))
            .collect())
    }
    /// Writes several single-level trees in order and returns their OIDs; a
    /// later tree may reference an earlier one. The default runs
    /// [`GitOps::mktree`] per tree; [`CliGitOps`] uses one
    /// `git mktree --batch` (`T-057`).
    fn mktrees(&self, trees: &[Vec<TreeEntry>]) -> Result<Vec<String>, GitError> {
        trees.iter().map(|t| self.mktree(t)).collect()
    }
    /// Reads several refs at once (`None` for an absent ref). The default
    /// runs [`GitOps::ref_read`] per ref; [`CliGitOps`] uses one
    /// `git for-each-ref` (`T-057`).
    fn refs_read(&self, references: &[&str]) -> Result<Vec<Option<String>>, GitError> {
        references.iter().map(|r| self.ref_read(r)).collect()
    }
    /// [`GitOps::fetch_ref`] without reading `dst` back: callers that read
    /// several refs afterwards use [`GitOps::refs_read`] (`T-057`).
    fn fetch_into(&self, remote: &str, src: &str, dst: &str) -> Result<(), GitError> {
        self.fetch_ref(remote, src, dst).map(drop)
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

    /// `true` if remote `name` has a URL. Answers without a git process when
    /// the repository's own config file plainly sets `remote.<name>.url`;
    /// otherwise (no such line, `[include]`s, old-style `[remote.x]`
    /// headers, `GIT_DIR` / `GIT_CONFIG*` overrides) asks `git config`, so
    /// the result always matches git's (`T-057`).
    pub fn remote_configured(&self, name: &str) -> Result<bool, GitError> {
        if local_config_sets_remote_url(&self.repo, name) {
            return Ok(true);
        }
        Ok(self.remote_url(name)?.is_some())
    }
}

/// The common git directory's `config` plainly contains `[remote "<name>"]`
/// with a `url` key. `false` means "not sure", never "absent".
fn local_config_sets_remote_url(repo: &Path, name: &str) -> bool {
    let overridden = std::env::vars_os().any(|(k, _)| {
        let k = k.to_string_lossy();
        k == "GIT_DIR" || k == "GIT_COMMON_DIR" || k.starts_with("GIT_CONFIG")
    });
    if overridden {
        return false;
    }
    let dot_git = repo.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else {
        // A linked worktree: `.git` is a file `gitdir: <path>`.
        let Ok(text) = std::fs::read_to_string(&dot_git) else {
            return false;
        };
        let Some(path) = text.trim().strip_prefix("gitdir:") else {
            return false;
        };
        repo.join(path.trim())
    };
    let common = match std::fs::read_to_string(git_dir.join("commondir")) {
        Ok(rel) => git_dir.join(rel.trim()),
        Err(_) => git_dir,
    };
    let Ok(config) = std::fs::read_to_string(common.join("config")) else {
        return false;
    };
    let mut in_remote = false;
    for line in config.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.starts_with(';') || line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            let lower = line.to_ascii_lowercase();
            if lower.starts_with("[include") || lower.starts_with("[remote.") {
                return false;
            }
            // Section names are case-insensitive, subsection names are not.
            in_remote = lower.starts_with("[remote ")
                && line.ends_with(']')
                && line
                    .find('"')
                    .zip(line.rfind('"'))
                    .is_some_and(|(open, close)| close > open && line[open + 1..close] == *name);
            continue;
        }
        if in_remote {
            let key = line.split(['=', ' ', '\t']).next().unwrap_or("");
            if key.eq_ignore_ascii_case("url") {
                return true;
            }
        }
    }
    false
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
        self.fetch_into(remote, src, dst)?;
        self.ref_read(dst)
    }

    fn fetch_into(&self, remote: &str, src: &str, dst: &str) -> Result<(), GitError> {
        let refspec = format!("+{src}*:{dst}*");
        self.run_ok(&[
            OsStr::new("fetch"),
            OsStr::new("--quiet"),
            OsStr::new("--no-tags"),
            OsStr::new("--prune"),
            OsStr::new(remote),
            OsStr::new(&refspec),
        ])
    }

    fn refs_read(&self, references: &[&str]) -> Result<Vec<Option<String>>, GitError> {
        let mut args = vec![
            OsStr::new("for-each-ref"),
            OsStr::new("--format=%(refname) %(objectname)"),
        ];
        args.extend(references.iter().map(OsStr::new));
        let output = self.run(&args)?;
        let found: Vec<(String, String)> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|l| l.split_once(' '))
            .map(|(name, oid)| (name.to_string(), oid.to_string()))
            .collect();
        // Patterns also match refs below them; only exact names count.
        Ok(references
            .iter()
            .map(|r| {
                found
                    .iter()
                    .find(|(name, _)| name == r)
                    .map(|(_, oid)| oid.clone())
            })
            .collect())
    }

    fn mktrees(&self, trees: &[Vec<TreeEntry>]) -> Result<Vec<String>, GitError> {
        // `--batch` separates trees with a blank line, which only works in
        // newline mode (with `-z` git misreads the separator), so names that
        // need quoting fall back to one `mktree -z` per tree.
        let plain = |e: &TreeEntry| {
            !e.path.is_empty()
                && !e
                    .path
                    .bytes()
                    .any(|b| b < 0x20 || b == b'"' || b == b'\\' || b >= 0x7f)
        };
        if trees.len() <= 1 || !trees.iter().flatten().all(plain) {
            return trees.iter().map(|t| self.mktree(t)).collect();
        }
        let mut input = String::new();
        for (i, tree) in trees.iter().enumerate() {
            if i > 0 {
                input.push('\n');
            }
            for e in tree {
                input.push_str(&format!("{} {} {}\t{}\n", e.mode, e.kind, e.oid, e.path));
            }
        }
        let args = [OsStr::new("mktree"), OsStr::new("--batch")];
        let output = self.run_with(&args, Some(input.as_bytes()), &[])?;
        let oids: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_string)
            .collect();
        if oids.len() == trees.len() {
            Ok(oids)
        } else {
            Err(GitError::NonZeroExit {
                context: self.context(&args),
                code: None,
                stderr: format!("expected {} tree OIDs, got {}", trees.len(), oids.len()),
            })
        }
    }

    fn tree_blobs(&self, treeish: &str) -> Result<Vec<(String, String, Vec<u8>)>, GitError> {
        let args = [OsStr::new("cat-file"), OsStr::new("--batch")];
        let context = self.context(&args);
        let bad = |why: String| GitError::NonZeroExit {
            context: context.clone(),
            code: None,
            stderr: why,
        };
        let mut child = Command::new("git")
            .arg("-C")
            .arg(&self.repo)
            .args(args)
            .env("LC_ALL", "C")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|source| GitError::Spawn {
                context: context.clone(),
                source,
            })?;
        let mut stdin = child.stdin.take();
        let mut stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
        // One object from cat-file's output: "<oid> <type> <size>" LF data LF.
        let read_one = |stdout: &mut BufReader<std::process::ChildStdout>,
                        name: &str|
         -> Result<(String, String, Vec<u8>), GitError> {
            let mut header = String::new();
            stdout
                .read_line(&mut header)
                .map_err(|e| bad(format!("read `{name}`: {e}")))?;
            let fields: Vec<&str> = header.trim_end().split(' ').collect();
            let [oid, kind, size] = fields[..] else {
                return Err(bad(format!("`{name}`: {}", header.trim_end())));
            };
            let size: usize = size
                .parse()
                .map_err(|_| bad(format!("bad header `{}`", header.trim_end())))?;
            let mut data = vec![0; size + 1];
            stdout
                .read_exact(&mut data)
                .map_err(|e| bad(format!("read `{name}`: {e}")))?;
            data.pop();
            Ok((oid.to_string(), kind.to_string(), data))
        };
        let result = (|| {
            // Trees are walked one request at a time: without `--buffer`,
            // cat-file flushes after every object, so each answer can be read
            // before the next request (the walk needs it to continue).
            let mut ask = |name: &str| -> Result<(String, String, Vec<u8>), GitError> {
                let pipe = stdin.as_mut().expect("stdin open during the walk");
                writeln!(pipe, "{name}")
                    .and_then(|()| pipe.flush())
                    .map_err(|e| bad(format!("write `{name}`: {e}")))?;
                read_one(&mut stdout, name)
            };
            let (root, kind, data) = ask(&format!("{treeish}^{{tree}}"))?;
            if kind != "tree" {
                return Err(bad(format!("`{treeish}` is a {kind}")));
            }
            let raw = root.len() / 2;
            // Depth-first in tree order, as `ls-tree -r` lists blobs.
            type Entries = std::vec::IntoIter<(String, String, String)>;
            let mut blobs: Vec<(String, String)> = Vec::new();
            let mut stack: Vec<(String, Entries)> = vec![(
                String::new(),
                parse_tree(&data, raw).map_err(bad)?.into_iter(),
            )];
            while let Some((prefix, entries)) = stack.last_mut() {
                let Some((mode, name, oid)) = entries.next() else {
                    stack.pop();
                    continue;
                };
                let path = format!("{prefix}{name}");
                if mode.starts_with('4') {
                    let (_, _, sub) = ask(&oid)?;
                    let sub = parse_tree(&sub, raw).map_err(bad)?;
                    stack.push((format!("{path}/"), sub.into_iter()));
                } else if mode.starts_with('1') {
                    blobs.push((path, oid));
                }
            }
            // Blobs need no answer before the next request: a writer thread
            // sends them all while this thread reads the answers in order,
            // so neither side waits on a full pipe.
            let requests: String = blobs.iter().map(|(_, oid)| format!("{oid}\n")).collect();
            let mut pipe = stdin.take().expect("stdin open after the walk");
            std::thread::scope(|scope| {
                scope.spawn(move || {
                    let _ = pipe.write_all(requests.as_bytes());
                    // Dropping `pipe` closes cat-file's stdin.
                });
                let mut files = Vec::with_capacity(blobs.len());
                let mut failure = None;
                for (path, oid) in blobs {
                    match read_one(&mut stdout, &oid) {
                        Ok((_, kind, bytes)) if kind == "blob" => files.push((path, oid, bytes)),
                        Ok((_, kind, _)) => {
                            failure = Some(bad(format!("{path}: expected a blob, got {kind}")));
                            break;
                        }
                        Err(err) => {
                            failure = Some(err);
                            break;
                        }
                    }
                }
                match failure {
                    None => Ok(files),
                    Some(err) => {
                        // Drain the rest so cat-file keeps reading and the
                        // writer can finish; otherwise joining it could block
                        // on a full pipe.
                        let _ = std::io::copy(&mut stdout, &mut std::io::sink());
                        Err(err)
                    }
                }
            })
        })();
        drop(stdin);
        let status = child.wait().map_err(|source| GitError::Spawn {
            context: context.clone(),
            source,
        })?;
        match result {
            Ok(files) if status.success() => Ok(files),
            Ok(_) => Err(bad(format!("exited with {status}"))),
            Err(err) => Err(err),
        }
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

/// Entries of a raw tree object: `(mode, name, hex oid)`; `raw` is the
/// binary OID length (20 for SHA-1, 32 for SHA-256).
fn parse_tree(data: &[u8], raw: usize) -> Result<Vec<(String, String, String)>, String> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < data.len() {
        let space = data[at..]
            .iter()
            .position(|&b| b == b' ')
            .ok_or("tree entry without a mode")?;
        let nul = data[at + space..]
            .iter()
            .position(|&b| b == 0)
            .ok_or("tree entry without a name")?;
        let mode = String::from_utf8_lossy(&data[at..at + space]).into_owned();
        let name = String::from_utf8_lossy(&data[at + space + 1..at + space + nul]).into_owned();
        let start = at + space + nul + 1;
        let oid = data
            .get(start..start + raw)
            .ok_or("truncated tree entry")?
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        out.push((mode, name, oid));
        at = start + raw;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{local_config_sets_remote_url, lost_push_race, parse_tree};
    use std::fs;
    use std::path::PathBuf;

    fn repo_with_config(name: &str, config: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mt-gitcfg-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(".git")).unwrap();
        fs::write(dir.join(".git/config"), config).unwrap();
        dir
    }

    #[test]
    fn local_config_remote_fast_path_is_only_ever_a_safe_yes() {
        let cases = [
            (
                "plain",
                "[core]\n\tbare = false\n[remote \"origin\"]\n\turl = https://x/y.git\n",
                true,
            ),
            ("case", "[Remote \"origin\"]\nURL=file:///r.git\n", true),
            (
                "other",
                "[remote \"upstream\"]\n\turl = https://x/y.git\n",
                false,
            ),
            (
                "subsection-case",
                "[remote \"Origin\"]\n\turl = https://x/y.git\n",
                false,
            ),
            (
                "no-url",
                "[remote \"origin\"]\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n",
                false,
            ),
            (
                "include",
                "[include]\n\tpath = other\n[remote \"origin\"]\n\turl = x\n",
                false,
            ),
            ("old-style", "[remote.origin]\n\turl = x\n", false),
            ("comment", "# [remote \"origin\"]\n# url = x\n", false),
        ];
        for (name, config, expected) in cases {
            let dir = repo_with_config(name, config);
            assert_eq!(
                local_config_sets_remote_url(&dir, "origin"),
                expected,
                "{name}"
            );
            let _ = fs::remove_dir_all(&dir);
        }

        // A linked worktree reads the common directory's config.
        let main = repo_with_config("main", "[remote \"origin\"]\n\turl = x\n");
        let gitdir = main.join(".git/worktrees/wt");
        fs::create_dir_all(&gitdir).unwrap();
        fs::write(gitdir.join("commondir"), "../..\n").unwrap();
        let wt = main.join("wt");
        fs::create_dir_all(&wt).unwrap();
        fs::write(wt.join(".git"), format!("gitdir: {}\n", gitdir.display())).unwrap();
        assert!(local_config_sets_remote_url(&wt, "origin"));
        let _ = fs::remove_dir_all(&main);
    }

    #[test]
    fn parse_tree_reads_mode_name_and_oid() {
        let mut data = b"100644 a.json\0".to_vec();
        data.extend([0xab; 20]);
        data.extend_from_slice(b"40000 log\0");
        data.extend([0x01; 20]);
        let entries = parse_tree(&data, 20).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, "100644");
        assert_eq!(entries[0].1, "a.json");
        assert_eq!(entries[0].2, "ab".repeat(20));
        assert_eq!(entries[1].1, "log");
        assert!(parse_tree(&data[..data.len() - 1], 20).is_err());
    }

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
