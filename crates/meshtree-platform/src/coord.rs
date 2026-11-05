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
//! Compaction (`SPEC.md` §9, `ADR-0010`): a tree may hold one
//! `snapshot/<W>.json`, the replay state at watermark `W`. Replay starts from
//! the larger of the tips' snapshots and folds the events above it; events
//! at or below it are dropped from the union (a dropped *new* event is
//! reported in [`SyncOutcome::dropped`] for re-issue). A store built
//! [`CoordStore::with_compaction`] folds old events into a new snapshot when
//! enough are foldable; the replayed state is unchanged.
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
use meshtree_core::lease::coord::{
    decode_log, decode_snapshot, encode_compacted, sha1_hex, DecodeIssue,
};
use meshtree_core::lease::{
    compact, replay_from, LeaseEvent, Replay, ReplayConfig, Snapshot, Watermark,
};
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
    /// Watermark of the snapshot in the tree after the sync (`ADR-0010`).
    pub watermark: Option<Watermark>,
    /// `true` if this round compacted the log into a new snapshot.
    pub compacted: bool,
    /// Ids of `new_events` dropped because a snapshot already covers their
    /// key (late events, §9): the caller re-issues them as fresh events.
    pub dropped: Vec<String>,
}

/// Reads and writes the coordination ref through [`GitOps`].
pub struct CoordStore<G> {
    git: G,
    remote: Option<String>,
    config: ReplayConfig,
    /// Compact past this many foldable events (`None`: never; `ADR-0010`
    /// reserves compaction for daemons).
    compact_threshold: Option<usize>,
}

impl<G: GitOps> CoordStore<G> {
    /// A store over `git`; `remote` = `None` for local-only operation.
    pub fn new(git: G, remote: Option<String>, config: ReplayConfig) -> Self {
        Self {
            git,
            remote,
            config,
            compact_threshold: None,
        }
    }

    /// Lets sync rounds compact the log once more than `threshold` events
    /// are foldable (`SPEC.md` §9 Compaction; the daemon passes
    /// `meshtree_core::lease::COMPACT_THRESHOLD`).
    pub fn with_compaction(mut self, threshold: usize) -> Self {
        self.compact_threshold = Some(threshold);
        self
    }

    /// The compaction snapshot and the `log/` events of commit `oid`.
    pub fn read_state(&self, oid: &str) -> Result<(Option<Snapshot>, Vec<LeaseEvent>), GitError> {
        let tip = self.tip(oid)?;
        Ok((tip.snapshot().0, tip.log().0))
    }

    /// The underlying git backend.
    pub fn git(&self) -> &G {
        &self.git
    }

    /// Every blob of commit `oid`, read in one pass however long the log is
    /// (`ADR-0009` Part 1; one git process since `T-057`).
    fn tip(&self, oid: &str) -> Result<TipFiles, GitError> {
        Ok(TipFiles {
            files: self.git.tree_blobs(oid)?,
        })
    }

    /// Decodes the event log stored in commit `oid`.
    pub fn read_log(&self, oid: &str) -> Result<(Vec<LeaseEvent>, Vec<DecodeIssue>), GitError> {
        Ok(self.tip(oid)?.log())
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
            // Fetch first, then read the local ref and the fetched tip in one
            // call (`T-057`); a failed fetch leaves only the local tip.
            let (local, remote_tip, mut deferred) = match &self.remote {
                None => (self.git.ref_read(COORD_REF)?, None, None),
                Some(remote) => {
                    let tracking = tracking_ref(remote);
                    let fetched = self.git.fetch_into(remote, COORD_REF, &tracking);
                    let refs = self.git.refs_read(&[COORD_REF, &tracking])?;
                    let local = refs[0].clone();
                    match fetched {
                        Ok(()) => (local, refs[1].clone(), None),
                        Err(err) => (local, None, Some(format!("fetch failed: {err}"))),
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
                .map(|oid| self.tip(oid))
                .collect::<Result<Vec<_>, _>>()?;
            // Between tips the larger watermark wins; events it covers are
            // subsumed and leave the union (§9, ADR-0010).
            let mut winner: Option<Snapshot> = None;
            for snapshot in &snapshots {
                let (log, bad) = snapshot.log();
                events.extend(log);
                issues.extend(bad);
                let (snap, bad) = snapshot.snapshot();
                issues.extend(bad);
                if let Some(snap) = snap {
                    if winner
                        .as_ref()
                        .map_or(true, |w| snap.watermark > w.watermark)
                    {
                        winner = Some(snap);
                    }
                }
            }
            let covered = |e: &LeaseEvent| winner.as_ref().is_some_and(|w| w.watermark.covers(e));
            let dropped: Vec<String> = new_events
                .iter()
                .filter(|e| covered(e))
                .map(|e| e.id.clone())
                .collect();
            events.extend_from_slice(new_events);
            events.retain(|e| !covered(e));
            events.sort_by(|a, b| a.total_cmp(b));
            let mut seen = BTreeSet::new();
            events.retain(|e| seen.insert(e.id.clone()));

            let derived = replay_from(winner.as_ref(), &events, &self.config, conflicts_raw);
            let compacted = self.compact_threshold.and_then(|threshold| {
                compact(
                    winner.as_ref(),
                    &events,
                    &self.config,
                    conflicts_raw,
                    threshold,
                )
            });
            let (snapshot, kept) = match compacted {
                // Same replay by construction (ADR-0010 invariant).
                Some((snap, rest)) => (Some(snap), rest),
                None => (winner.clone(), events.clone()),
            };
            let did_compact =
                snapshot.as_ref().map(|s| &s.watermark) != winner.as_ref().map(|w| &w.watermark);
            let mut files = encode_compacted(snapshot.as_ref(), &kept, &derived);
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
            // Linear history on the remote tip (else the local tip); skip the
            // write and the commit entirely when the files are unchanged (the
            // tree is canonical, so equal files mean an equal tree).
            let parent = remote_tip.clone().or_else(|| local.clone());
            let unchanged = parent.as_deref().is_some_and(|p| {
                tips.iter()
                    .position(|t| *t == p)
                    .is_some_and(|i| snapshots[i].same_files(&files))
            });
            let new_oid = match &parent {
                Some(p) if unchanged => p.clone(),
                _ => {
                    // Blobs already in a tip are reused by content: only new
                    // or changed files are written.
                    let known: HashMap<&[u8], &str> = snapshots
                        .iter()
                        .flat_map(|s| s.files.iter())
                        .map(|(_, oid, bytes)| (bytes.as_slice(), oid.as_str()))
                        .collect();
                    let tree = self.write_tree(&files, &known)?;
                    let parents: Vec<&str> = parent.iter().map(String::as_str).collect();
                    self.git
                        .commit_tree(&tree, &parents, "meshtree: coord sync")?
                }
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
                watermark: snapshot.map(|s| s.watermark),
                compacted: did_compact,
                dropped,
            });
        }
    }

    /// The task registry stored in commit `oid`: task id → record bytes.
    pub fn read_tasks(&self, oid: &str) -> Result<BTreeMap<String, Vec<u8>>, GitError> {
        Ok(self.tip(oid)?.tasks())
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
        // Subtree OIDs are computed here so the root can name them, and all
        // trees go to git in one batch (`T-057`); git's OIDs must agree.
        let subtrees: Vec<Vec<TreeEntry>> = dirs.values().cloned().collect();
        let Some(sub_oids) = subtrees
            .iter()
            .map(|t| tree_oid(t))
            .collect::<Option<Vec<String>>>()
        else {
            // Not SHA-1 object names: one mktree per tree, root last.
            let mut root = Vec::new();
            for (dir, entries) in &dirs {
                root.push(TreeEntry::tree(*dir, self.git.mktree(entries)?));
            }
            return self.git.mktree(&root);
        };
        let root: Vec<TreeEntry> = dirs
            .keys()
            .zip(&sub_oids)
            .map(|(dir, oid)| TreeEntry::tree(*dir, oid.clone()))
            .collect();
        let mut trees = subtrees;
        trees.push(root);
        let written = self.git.mktrees(&trees)?;
        if written[..written.len() - 1] != sub_oids[..] {
            return Err(GitError::NonZeroExit {
                context: "git mktree --batch".into(),
                code: None,
                stderr: format!("tree OIDs {written:?} differ from computed {sub_oids:?}"),
            });
        }
        Ok(written[written.len() - 1].clone())
    }
}

/// The SHA-1 OID git gives a tree of `entries` (`None` if an entry's OID is
/// not SHA-1 hex). Entries sort by name, a subtree's name compared as if it
/// ended in `/`, exactly as git orders them.
fn tree_oid(entries: &[TreeEntry]) -> Option<String> {
    let mut sorted: Vec<(Vec<u8>, &TreeEntry)> = entries
        .iter()
        .map(|e| {
            let mut key = e.path.as_bytes().to_vec();
            if e.kind == "tree" {
                key.push(b'/');
            }
            (key, e)
        })
        .collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let mut body = Vec::new();
    for (_, e) in sorted {
        if e.oid.len() != 40 {
            return None;
        }
        // Git stores a subtree's mode without the leading zero.
        body.extend_from_slice(e.mode.trim_start_matches('0').as_bytes());
        body.push(b' ');
        body.extend_from_slice(e.path.as_bytes());
        body.push(0);
        for i in (0..40).step_by(2) {
            body.push(u8::from_str_radix(e.oid.get(i..i + 2)?, 16).ok()?);
        }
    }
    let mut object = format!("tree {}\0", body.len()).into_bytes();
    object.extend_from_slice(&body);
    Some(sha1_hex(&object))
}

/// The blobs of one coordination commit: `(path, oid, bytes)`.
struct TipFiles {
    files: Vec<(String, String, Vec<u8>)>,
}

impl TipFiles {
    /// `true` when this tip holds exactly `files` (paths and bytes).
    fn same_files(&self, files: &BTreeMap<String, Vec<u8>>) -> bool {
        self.files.len() == files.len()
            && self
                .files
                .iter()
                .all(|(path, _, bytes)| files.get(path) == Some(bytes))
    }

    /// The decoded `log/` events.
    fn log(&self) -> (Vec<LeaseEvent>, Vec<DecodeIssue>) {
        decode_log(
            self.files
                .iter()
                .filter(|(path, _, _)| path.starts_with("log/"))
                .map(|(path, _, bytes)| (path.as_str(), bytes.as_slice())),
        )
    }

    /// The compaction snapshot (`ADR-0010`), if any.
    fn snapshot(&self) -> (Option<Snapshot>, Vec<DecodeIssue>) {
        decode_snapshot(
            self.files
                .iter()
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
