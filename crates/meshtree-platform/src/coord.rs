//! Coordination store: `refs/meshtree/coord` sync with CAS and bounded retry.
//!
//! Authoritative specification: `SPEC.md` §9 (atomic update via CAS, event
//! log union, deterministic replay), §16 (`MT-001`, `MT-003`), Appendix A.
//!
//! One [`CoordStore::sync`] round: fetch the remote tip → union its log with
//! the local ref's log and the new events → replay → write the canonical tree
//! → advance the local ref (local CAS) → push with `--force-with-lease`
//! (cross-machine CAS). A lost push race is retried from the fetch, bounded
//! ×5 with jitter. The coordination ref is linear on the remote tip and never
//! gets merge commits. Network failures never block local progress: the
//! local ref still advances and the outcome reports the push as deferred.
//!
//! A round's git subprocess count does not depend on the log's length
//! (`ADR-0009` Part 1): each tip is read with one `ls-tree` and one batched
//! `cat-file`, blobs whose bytes a tip already holds keep their OIDs, and only
//! new or changed files are written, in one batch. The resulting tree is
//! byte-identical to writing every file afresh.
//!
//! The ref also carries the cross-machine task registry, `tasks/<id>.json`
//! (§5, §8 `spawn` step 4). A record is written only by the machine that
//! owns the task and carries an integer `revision` (the owner's clock in
//! nanoseconds). Each round keeps every record of both tips; when the tips
//! disagree on a task, the higher revision wins (ties: the greater bytes, so
//! every machine picks the same record), and records passed to
//! [`CoordStore::sync_with`] replace both.

use crate::gitops::{GitError, GitOps, TreeEntry, ERROR_CODE_CAS_REJECTED};
use meshtree_core::lease::coord::{decode_log, encode, DecodeIssue};
use meshtree_core::lease::{replay, LeaseEvent, Replay, ReplayConfig};
use meshtree_core::pattern::conflicts_raw;
use std::collections::hash_map::RandomState;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::hash::{BuildHasher, Hasher};
use std::time::Duration;

/// The orphan coordination ref (`SPEC.md` §5).
pub const COORD_REF: &str = "refs/meshtree/coord";

/// Bound on fetch → replay → push rounds (`SPEC.md` §9 "bounded ×5").
pub const MAX_ATTEMPTS: u32 = 5;

/// "Ref must not exist" for a local `update-ref` CAS.
const ZERO_OID: &str = "0000000000000000000000000000000000000000";

/// Tree directory of the cross-machine task registry.
pub const TASKS_DIR: &str = "tasks";

/// Local ref holding the last fetched remote tip.
pub fn tracking_ref(remote: &str) -> String {
    format!("refs/meshtree/remotes/{remote}/coord")
}

/// Coordination-store failure.
#[derive(Debug)]
pub enum CoordError {
    /// Git plumbing failed (not a lost race).
    Git(GitError),
    /// Every attempt lost the CAS race (`MT-003`).
    Contention {
        /// Attempts made (always [`MAX_ATTEMPTS`]).
        attempts: u32,
    },
}

impl CoordError {
    /// MeshTree error code (`SPEC.md` §16), if any.
    pub fn code(&self) -> Option<&'static str> {
        match self {
            CoordError::Git(err) => err.code(),
            CoordError::Contention { .. } => Some(ERROR_CODE_CAS_REJECTED),
        }
    }
}

impl fmt::Display for CoordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CoordError::Git(err) => write!(f, "{err}"),
            CoordError::Contention { attempts } => write!(
                f,
                "[{ERROR_CODE_CAS_REJECTED}] coordination ref CAS lost {attempts} times; retry later"
            ),
        }
    }
}

impl std::error::Error for CoordError {}

impl From<GitError> for CoordError {
    fn from(err: GitError) -> Self {
        CoordError::Git(err)
    }
}

/// Result of one [`CoordStore::sync`].
#[derive(Debug, Clone)]
pub struct SyncOutcome {
    /// Local `refs/meshtree/coord` after the sync.
    pub oid: String,
    /// Lease table and rejections derived from the unioned log. Callers check
    /// `replay.rejected` for their own event ids (`MT-001`: revoke the claim).
    pub replay: Replay,
    /// Rounds used (1..=[`MAX_ATTEMPTS`]).
    pub attempts: u32,
    /// `true` if the remote now holds `oid`.
    pub pushed: bool,
    /// Why the remote was not updated, when a remote is configured and the
    /// push did not happen (network/auth failure); the local ref still advanced.
    pub deferred: Option<String>,
    /// Log entries skipped as malformed.
    pub issues: Vec<DecodeIssue>,
}

/// Reads and writes the coordination ref through [`GitOps`].
pub struct CoordStore<G> {
    git: G,
    remote: Option<String>,
    config: ReplayConfig,
}

impl<G: GitOps> CoordStore<G> {
    /// A store over `git`; `remote` = `None` for local-only operation.
    pub fn new(git: G, remote: Option<String>, config: ReplayConfig) -> Self {
        Self {
            git,
            remote,
            config,
        }
    }

    /// The underlying git backend.
    pub fn git(&self) -> &G {
        &self.git
    }

    /// Every blob of commit `oid`: one `ls-tree` and one batched read,
    /// however long the log is (`ADR-0009` Part 1).
    fn snapshot(&self, oid: &str) -> Result<Snapshot, GitError> {
        let entries: Vec<TreeEntry> = self
            .git
            .ls_tree(oid)?
            .into_iter()
            .filter(|e| e.kind == "blob")
            .collect();
        let oids: Vec<String> = entries.iter().map(|e| e.oid.clone()).collect();
        let blobs = self.git.cat_blobs(&oids)?;
        Ok(Snapshot {
            files: entries
                .into_iter()
                .zip(blobs)
                .map(|(e, bytes)| (e.path, e.oid, bytes))
                .collect(),
        })
    }

    /// Decodes the event log stored in commit `oid`.
    pub fn read_log(&self, oid: &str) -> Result<(Vec<LeaseEvent>, Vec<DecodeIssue>), GitError> {
        Ok(self.snapshot(oid)?.log())
    }

    /// Appends `new_events` (may be empty: a pure fetch/flush) and syncs.
    pub fn sync(&self, new_events: &[LeaseEvent]) -> Result<SyncOutcome, CoordError> {
        self.sync_with(new_events, &BTreeMap::new())
    }

    /// [`CoordStore::sync`] that also writes task registry records
    /// (`task id → tasks/<id>.json` bytes, each with a `revision`).
    pub fn sync_with(
        &self,
        new_events: &[LeaseEvent],
        tasks: &BTreeMap<String, Vec<u8>>,
    ) -> Result<SyncOutcome, CoordError> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let local = self.git.ref_read(COORD_REF)?;
            let (remote_tip, mut deferred) = match &self.remote {
                None => (None, None),
                Some(remote) => {
                    match self.git.fetch_ref(remote, COORD_REF, &tracking_ref(remote)) {
                        Ok(tip) => (tip, None),
                        Err(err) => (None, Some(format!("fetch failed: {err}"))),
                    }
                }
            };
            let remote_known = self.remote.is_some() && deferred.is_none();

            let mut events = Vec::new();
            let mut issues = Vec::new();
            let mut tips: Vec<&str> = Vec::new();
            for oid in [local.as_deref(), remote_tip.as_deref()]
                .into_iter()
                .flatten()
            {
                if !tips.contains(&oid) {
                    tips.push(oid);
                }
            }
            let snapshots = tips
                .iter()
                .map(|oid| self.snapshot(oid))
                .collect::<Result<Vec<_>, _>>()?;
            for snapshot in &snapshots {
                let (log, bad) = snapshot.log();
                events.extend(log);
                issues.extend(bad);
            }
            events.extend_from_slice(new_events);
            events.sort_by(|a, b| a.total_cmp(b));
            let mut seen = BTreeSet::new();
            events.retain(|e| seen.insert(e.id.clone()));

            let derived = replay(&events, &self.config, conflicts_raw);
            let mut files = encode(&events, &derived);
            let mut registry: BTreeMap<String, Vec<u8>> = BTreeMap::new();
            for snapshot in &snapshots {
                for (id, bytes) in snapshot.tasks() {
                    match registry.get(&id) {
                        Some(kept) if !newer(&bytes, kept) => {}
                        _ => {
                            registry.insert(id, bytes);
                        }
                    }
                }
            }
            registry.extend(tasks.iter().map(|(id, b)| (id.clone(), b.clone())));
            for (id, bytes) in registry {
                files.insert(format!("{TASKS_DIR}/{id}.json"), bytes);
            }
            // Blobs already in a tip are reused by content: only new or
            // changed files are written.
            let known: HashMap<&[u8], &str> = snapshots
                .iter()
                .flat_map(|s| s.files.iter())
                .map(|(_, oid, bytes)| (bytes.as_slice(), oid.as_str()))
                .collect();
            let tree = self.write_tree(&files, &known)?;

            // Linear history on the remote tip (else the local tip); skip the
            // commit entirely when the tree is unchanged.
            let parent = remote_tip.clone().or_else(|| local.clone());
            let new_oid = match &parent {
                Some(p) if self.git.rev_parse(&format!("{p}^{{tree}}"))? == tree => p.clone(),
                Some(p) => self
                    .git
                    .commit_tree(&tree, &[p.as_str()], "meshtree: coord sync")?,
                None => self.git.commit_tree(&tree, &[], "meshtree: coord sync")?,
            };

            if local.as_deref() != Some(new_oid.as_str()) {
                let expected = local.as_deref().unwrap_or(ZERO_OID);
                match self.git.update_ref(COORD_REF, &new_oid, Some(expected)) {
                    Ok(()) => {}
                    // Another local writer moved the ref: redo the round.
                    Err(GitError::CasRejected { .. }) if attempt < MAX_ATTEMPTS => {
                        backoff(attempt);
                        continue;
                    }
                    Err(GitError::CasRejected { .. }) => {
                        return Err(CoordError::Contention { attempts: attempt })
                    }
                    Err(err) => return Err(err.into()),
                }
            }

            let mut pushed = remote_known && remote_tip.as_deref() == Some(new_oid.as_str());
            if let (Some(remote), true, false) = (&self.remote, remote_known, pushed) {
                match self
                    .git
                    .push_ref_cas(remote, COORD_REF, &new_oid, remote_tip.as_deref())
                {
                    Ok(()) => pushed = true,
                    Err(GitError::RemoteCasRejected { .. }) if attempt < MAX_ATTEMPTS => {
                        backoff(attempt);
                        continue;
                    }
                    Err(GitError::RemoteCasRejected { .. }) => {
                        return Err(CoordError::Contention { attempts: attempt })
                    }
                    Err(err) => deferred = Some(format!("push failed: {err}")),
                }
            }

            return Ok(SyncOutcome {
                oid: new_oid,
                replay: derived,
                attempts: attempt,
                pushed,
                deferred,
                issues,
            });
        }
    }

    /// The task registry stored in commit `oid`: task id → record bytes.
    pub fn read_tasks(&self, oid: &str) -> Result<BTreeMap<String, Vec<u8>>, GitError> {
        Ok(self.snapshot(oid)?.tasks())
    }

    /// Writes `dir/name → bytes` files (one directory level, as in the coord
    /// layout) as git trees; returns the root tree OID. Blobs whose bytes are
    /// in `known` reuse that OID; the rest are written in one batch.
    fn write_tree(
        &self,
        files: &BTreeMap<String, Vec<u8>>,
        known: &HashMap<&[u8], &str>,
    ) -> Result<String, GitError> {
        let fresh: Vec<&[u8]> = files
            .values()
            .map(Vec::as_slice)
            .filter(|bytes| !known.contains_key(bytes))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let written: HashMap<&[u8], String> = fresh
            .iter()
            .copied()
            .zip(self.git.hash_objects(&fresh)?)
            .collect();
        let mut dirs: BTreeMap<&str, Vec<TreeEntry>> = BTreeMap::new();
        for (path, bytes) in files {
            let (dir, name) = path
                .split_once('/')
                .expect("coord files live one level below the root");
            let blob = match known.get(bytes.as_slice()) {
                Some(oid) => oid.to_string(),
                None => written[bytes.as_slice()].clone(),
            };
            dirs.entry(dir)
                .or_default()
                .push(TreeEntry::blob(name, blob));
        }
        let mut root = Vec::new();
        for (dir, entries) in dirs {
            root.push(TreeEntry::tree(dir, self.git.mktree(&entries)?));
        }
        self.git.mktree(&root)
    }
}

/// The blobs of one coordination commit: `(path, oid, bytes)`.
struct Snapshot {
    files: Vec<(String, String, Vec<u8>)>,
}

impl Snapshot {
    /// The decoded `log/` events.
    fn log(&self) -> (Vec<LeaseEvent>, Vec<DecodeIssue>) {
        decode_log(
            self.files
                .iter()
                .filter(|(path, _, _)| path.starts_with("log/"))
                .map(|(path, _, bytes)| (path.as_str(), bytes.as_slice())),
        )
    }

    /// The task registry: task id → record bytes.
    fn tasks(&self) -> BTreeMap<String, Vec<u8>> {
        self.files
            .iter()
            .filter_map(|(path, _, bytes)| {
                let id = path
                    .strip_prefix(TASKS_DIR)?
                    .strip_prefix('/')?
                    .strip_suffix(".json")?;
                (!id.contains('/')).then(|| (id.to_string(), bytes.clone()))
            })
            .collect()
    }
}

/// `true` if task record `a` supersedes `b`: higher `revision` (absent or
/// malformed counts as 0), then the greater bytes, so the choice is the same
/// on every machine.
fn newer(a: &[u8], b: &[u8]) -> bool {
    let revision = |bytes: &[u8]| {
        serde_json::from_slice::<serde_json::Value>(bytes)
            .ok()
            .and_then(|v| v.get("revision").and_then(serde_json::Value::as_u64))
            .unwrap_or(0)
    };
    (revision(a), a) > (revision(b), b)
}

/// Jittered linear backoff between CAS rounds: 10–60 ms × attempt.
fn backoff(attempt: u32) {
    let jitter = RandomState::new().build_hasher().finish() % 50;
    std::thread::sleep(Duration::from_millis((10 + jitter) * u64::from(attempt)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_record_merge_prefers_higher_revision_then_bytes() {
        let a = br#"{"revision":2,"status":"active"}"#;
        let b = br#"{"revision":10,"status":"abandoned"}"#;
        assert!(newer(b, a) && !newer(a, b), "numeric, not lexicographic");
        let x = br#"{"revision":5,"status":"active"}"#;
        let y = br#"{"revision":5,"status":"merged"}"#;
        assert!(newer(y, x) && !newer(x, y), "ties break on bytes");
        assert!(!newer(x, x));
        assert!(newer(a, b"not json"), "malformed counts as revision 0");
    }
}
