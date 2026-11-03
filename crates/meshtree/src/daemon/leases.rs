//! The daemon's in-memory lease state (`SPEC.md` §9 "Daemon claim path",
//! `ADR-0009` Part 2).
//!
//! `claim_paths` / `release_paths` are decided here, against the replay of
//! the coordination log as of the local ref plus the events this daemon has
//! acknowledged but not yet committed (`pending`). The background writer in
//! [`super`] commits `pending` and reports the new log back
//! ([`Leases::flushed`]).
//!
//! Other writers (a CLI `spawn --claim`, the daemon's own heartbeats and
//! merge-gate releases, other processes) update the ref directly. Before
//! every decision the local ref's tip is compared with the one the log was
//! read at, by reading the ref file (no git process); the log is reloaded
//! only when it moved.

use crate::exit::{CliError, PLATFORM};
use meshtree_core::lease::{replay, LeaseEvent, ReplayConfig};
use meshtree_core::pattern::conflicts_raw;
use meshtree_core::Rejection;
use meshtree_platform::coord::{CoordStore, COORD_REF};
use meshtree_platform::gitops::{CliGitOps, GitOps};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

pub struct Leases {
    repo_root: PathBuf,
    config: ReplayConfig,
    /// The log as of `tip`.
    log: Vec<LeaseEvent>,
    ids: HashSet<String>,
    /// Local coordination ref the log was read at (`None`: not read yet;
    /// `Some(None)`: the ref did not exist).
    tip: Option<Option<String>>,
    /// Acknowledged events not yet in the ref, in acknowledgement order.
    pending: Vec<LeaseEvent>,
    /// When the oldest pending event was acknowledged.
    since: Option<Instant>,
}

impl Leases {
    pub fn new(repo_root: &Path, config: ReplayConfig) -> Self {
        Self {
            repo_root: repo_root.to_path_buf(),
            config,
            log: Vec::new(),
            ids: HashSet::new(),
            tip: None,
            pending: Vec::new(),
            since: None,
        }
    }

    /// Reloads the log if the local ref moved since it was read.
    pub fn refresh(&mut self, store: &CoordStore<CliGitOps>) -> Result<(), CliError> {
        let tip = current_tip(&self.repo_root, store.git())?;
        if self.tip.as_ref() == Some(&tip) {
            return Ok(());
        }
        let log = match &tip {
            Some(oid) => {
                store
                    .read_log(oid)
                    .map_err(|err| CliError::new(PLATFORM, err.to_string()))?
                    .0
            }
            None => Vec::new(),
        };
        self.set_log(tip, log);
        Ok(())
    }

    fn set_log(&mut self, tip: Option<String>, log: Vec<LeaseEvent>) {
        self.ids = log.iter().map(|e| e.id.clone()).collect();
        self.log = log;
        self.tip = Some(tip);
        let ids = &self.ids;
        self.pending.retain(|e| !ids.contains(&e.id));
        if self.pending.is_empty() {
            self.since = None;
        }
    }

    /// The rejections `candidates` would get if added now (empty: all
    /// granted). Same replay and conflict rule as the coordination store.
    pub fn decide(&self, candidates: &[LeaseEvent]) -> Vec<Rejection> {
        let mut all = self.log.clone();
        all.extend(self.pending.iter().cloned());
        all.extend(candidates.iter().cloned());
        let ids: HashSet<&str> = candidates.iter().map(|e| e.id.as_str()).collect();
        replay(&all, &self.config, conflicts_raw)
            .rejected
            .into_iter()
            .filter(|r| ids.contains(r.event.as_str()))
            .collect()
    }

    /// Records acknowledged events for the background writer.
    pub fn accept(&mut self, events: Vec<LeaseEvent>) {
        if events.is_empty() {
            return;
        }
        self.since.get_or_insert_with(Instant::now);
        self.pending.extend(events);
    }

    /// Events to commit next.
    pub fn pending(&self) -> Vec<LeaseEvent> {
        self.pending.clone()
    }

    /// How long the oldest pending event has waited (`None`: nothing pending).
    pub fn waiting(&self) -> Option<std::time::Duration> {
        self.since.map(|t| t.elapsed())
    }

    /// A commit of the ref to `tip` holding `log` succeeded.
    pub fn flushed(&mut self, tip: String, log: Vec<LeaseEvent>) {
        self.set_log(Some(tip), log);
    }

    /// Re-adds journaled events missing from the ref (crash recovery,
    /// §10). Call after [`Leases::refresh`]. Returns how many were missing.
    pub fn recover(&mut self, journaled: Vec<LeaseEvent>) -> usize {
        let missing: Vec<LeaseEvent> = journaled
            .into_iter()
            .filter(|e| !self.ids.contains(&e.id))
            .filter(|e| !self.pending.iter().any(|p| p.id == e.id))
            .collect();
        let n = missing.len();
        self.accept(missing);
        n
    }
}

/// The local coordination ref's tip, read from the ref files when the
/// repository has a plain `.git` directory (loose ref, then `packed-refs`),
/// else through git.
fn current_tip(repo_root: &Path, git: &CliGitOps) -> Result<Option<String>, CliError> {
    let git_dir = repo_root.join(".git");
    if git_dir.is_dir() {
        if let Ok(text) = fs::read_to_string(git_dir.join(COORD_REF)) {
            let oid = text.trim();
            if is_oid(oid) {
                return Ok(Some(oid.to_string()));
            }
        } else {
            match fs::read_to_string(git_dir.join("packed-refs")) {
                Ok(text) => {
                    let packed = text.lines().find_map(|line| {
                        let (oid, name) = line.split_once(' ')?;
                        (name == COORD_REF && is_oid(oid)).then(|| oid.to_string())
                    });
                    if packed.is_some() || !text.contains(COORD_REF) {
                        return Ok(packed);
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(_) => {}
            }
        }
    }
    git.ref_read(COORD_REF)
        .map_err(|err| CliError::new(PLATFORM, err.to_string()))
}

/// A full hex object id (SHA-1 or SHA-256).
fn is_oid(text: &str) -> bool {
    matches!(text.len(), 40 | 64) && text.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use meshtree_core::lease::EventType;
    use meshtree_core::{TaskId, Timestamp};

    fn ev(id: &str, kind: EventType, path: &str, task: u128, ms: u64) -> LeaseEvent {
        LeaseEvent {
            id: id.to_string(),
            kind,
            path: path.to_string(),
            task: TaskId::from_ulid(task),
            machine: "mc".into(),
            ts: Timestamp::from_unix_ms(ms),
            counter: 0,
            ttl_secs: (kind == EventType::Acquire).then_some(900),
        }
    }

    fn leases() -> Leases {
        Leases::new(
            Path::new("."),
            ReplayConfig {
                default_ttl_secs: 900,
                skew_grace_secs: 600,
            },
        )
    }

    #[test]
    fn decisions_see_log_and_pending_and_flush_prunes() {
        let mut l = leases();
        l.set_log(
            Some("a".repeat(40)),
            vec![ev("01A", EventType::Acquire, "src/**", 1, 1_000)],
        );
        // Conflicts with the committed log.
        let r = l.decide(&[ev("01B", EventType::Acquire, "src/x.rs", 2, 2_000)]);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].event, "01B");
        // Granted, then pending: a second claim on it conflicts in memory.
        let docs = ev("01C", EventType::Acquire, "docs/**", 2, 3_000);
        assert!(l.decide(std::slice::from_ref(&docs)).is_empty());
        l.accept(vec![docs.clone()]);
        assert!(l.waiting().is_some());
        let r = l.decide(&[ev("01D", EventType::Acquire, "docs/a.md", 3, 4_000)]);
        assert_eq!(r.len(), 1, "pending events are part of the decision");
        // The flush lands it in the log: pending empties.
        l.flushed(
            "b".repeat(40),
            vec![ev("01A", EventType::Acquire, "src/**", 1, 1_000), docs],
        );
        assert!(l.pending().is_empty() && l.waiting().is_none());
        // Recovery re-adds only events the ref lacks.
        let n = l.recover(vec![
            ev("01C", EventType::Acquire, "docs/**", 2, 3_000),
            ev("01E", EventType::Release, "docs/**", 2, 5_000),
        ]);
        assert_eq!(n, 1);
        assert_eq!(l.pending()[0].id, "01E");
    }

    #[test]
    fn oid_shape() {
        assert!(is_oid(&"0123456789abcdef".repeat(4)[..40]));
        assert!(is_oid(&"f".repeat(64)));
        assert!(!is_oid("ref: refs/heads/main"));
        assert!(!is_oid(&"g".repeat(40)));
    }
}
