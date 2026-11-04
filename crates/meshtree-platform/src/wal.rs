//! Write-ahead log of daemon state transitions.
//!
//! Authoritative specification: `SPEC.md` §10 (Crash safety: the daemon
//! journals spawn/claim/collect/merge transitions to a WAL inside
//! `.meshtree/` and replays it on restart), §8 (lifecycle edges), §18
//! (crash recovery).
//!
//! # File format
//!
//! `.meshtree/wal.log` is a sequence of frames, one per line:
//!
//! ```text
//! <len: 8 lowercase hex> SP <crc32(json): 8 lowercase hex> SP <json> LF
//! ```
//!
//! `json` is the compact serialization of one [`WalEntry`] (it never
//! contains a raw LF). Every [`Wal::append`] writes one whole frame and
//! `fsync`s before returning, so a crash can only tear the **last** frame.
//!
//! On [`Wal::open`] the longest prefix of valid frames is replayed. An invalid
//! frame with nothing valid after it is a torn tail: it is truncated away
//! (and reported) so later appends follow a clean frame boundary. An invalid
//! frame *followed by* a valid one is corruption, not a crash artefact, and a
//! frame whose checksum matches but whose JSON is not a known entry is from a
//! newer or broken writer; both refuse to open (`InvalidData`) rather than
//! discard journaled state.
//!
//! Files are opened through `std`, whose Windows default share mode is
//! `FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE` (§10 pre-open
//! discipline).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use meshtree_core::{TaskId, TaskStatus};
use serde::{Deserialize, Serialize};

/// WAL file name inside `.meshtree/` (`SPEC.md` §10).
pub const WAL_FILE: &str = "wal.log";

/// Bytes before the JSON payload: two 8-digit hex fields and two spaces.
const HEADER_LEN: usize = 18;

/// One journaled operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum WalOp {
    /// `spawn`: the task enters `spawning` (`SPEC.md` §8 `spawn` step 1–2).
    Spawn {
        task: TaskId,
        branch: String,
        worktree: String,
        base: String,
    },
    /// Any other §8 edge, including `collect` (`active → collecting`) and
    /// merge (`collecting → merging → merged`).
    Transition {
        task: TaskId,
        from: TaskStatus,
        to: TaskStatus,
    },
    /// Claims added to a task.
    Claim { task: TaskId, paths: Vec<String> },
    /// Claims released by a task.
    Release { task: TaskId, paths: Vec<String> },
    /// The lease events of an acknowledged claim or release, or the daemon's
    /// `EXPIRE_SWEEP`s, journaled before they are acted on (`SPEC.md` §10,
    /// `ADR-0009`): recovery re-applies those missing from
    /// `refs/meshtree/coord`. Does not change the replayed task state
    /// ([`WalOp::Claim`] / [`WalOp::Release`] do), so `task` (the lease
    /// holder) need not be a task this WAL spawned.
    Events {
        task: TaskId,
        events: Vec<meshtree_core::lease::LeaseEvent>,
    },
}

impl WalOp {
    /// The task this operation concerns.
    pub fn task(&self) -> &TaskId {
        match self {
            WalOp::Spawn { task, .. }
            | WalOp::Transition { task, .. }
            | WalOp::Claim { task, .. }
            | WalOp::Release { task, .. }
            | WalOp::Events { task, .. } => task,
        }
    }
}

/// A journaled operation with its sequence number and wall-clock time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalEntry {
    /// Strictly increasing from 1 within one WAL file.
    pub seq: u64,
    /// Unix milliseconds supplied by the caller (injected clock).
    pub at_ms: u64,
    #[serde(flatten)]
    pub op: WalOp,
}

/// Encodes `entry` as one frame, including the trailing LF.
pub fn encode_frame(entry: &WalEntry) -> Vec<u8> {
    let json = serde_json::to_vec(entry).expect("WalEntry serializes");
    let mut frame = format!("{:08x} {:08x} ", json.len(), crc32(&json)).into_bytes();
    frame.extend_from_slice(&json);
    frame.push(b'\n');
    frame
}

/// Outcome of decoding one frame at the start of a buffer.
enum Frame {
    /// A valid entry and the frame's length in bytes.
    Entry(WalEntry, usize),
    /// Length/checksum/framing failure (torn or corrupt bytes).
    Invalid,
    /// Checksum matched but the payload is not a [`WalEntry`].
    Undecodable(serde_json::Error),
}

fn parse_hex(field: &[u8]) -> Option<u32> {
    if field.len() != 8 || !field.iter().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    u32::from_str_radix(std::str::from_utf8(field).ok()?, 16).ok()
}

fn decode_frame(buf: &[u8]) -> Frame {
    if buf.len() < HEADER_LEN || buf[8] != b' ' || buf[17] != b' ' {
        return Frame::Invalid;
    }
    let (Some(len), Some(crc)) = (parse_hex(&buf[..8]), parse_hex(&buf[9..17])) else {
        return Frame::Invalid;
    };
    let end = HEADER_LEN + len as usize;
    if buf.len() <= end || buf[end] != b'\n' {
        return Frame::Invalid;
    }
    let json = &buf[HEADER_LEN..end];
    if crc32(json) != crc {
        return Frame::Invalid;
    }
    match serde_json::from_slice(json) {
        Ok(entry) => Frame::Entry(entry, end + 1),
        Err(e) => Frame::Undecodable(e),
    }
}

/// The valid frames of a WAL image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decoded {
    pub entries: Vec<WalEntry>,
    /// Length of the valid prefix; bytes after it are a torn tail.
    pub valid_len: usize,
}

/// Decodes a whole WAL image (see the module docs for the tail rules).
pub fn decode(bytes: &[u8]) -> io::Result<Decoded> {
    let mut entries = Vec::new();
    let mut pos = 0;
    while pos < bytes.len() {
        match decode_frame(&bytes[pos..]) {
            Frame::Entry(entry, len) => {
                entries.push(entry);
                pos += len;
            }
            Frame::Undecodable(e) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("WAL frame at byte {pos} is not a known entry: {e}"),
                ));
            }
            Frame::Invalid => {
                if let Some(at) = valid_frame_after(bytes, pos) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "WAL corrupt: invalid frame at byte {pos} is followed by a \
                             valid frame at byte {at}"
                        ),
                    ));
                }
                break;
            }
        }
    }
    Ok(Decoded {
        entries,
        valid_len: pos,
    })
}

/// Offset of the first decodable frame that starts after an LF past `pos`.
fn valid_frame_after(bytes: &[u8], pos: usize) -> Option<usize> {
    let mut at = pos;
    while let Some(nl) = bytes[at..].iter().position(|&b| b == b'\n') {
        at += nl + 1;
        if !matches!(decode_frame(&bytes[at..]), Frame::Invalid) {
            return Some(at);
        }
    }
    None
}

/// A torn tail discarded by [`Wal::open`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TornTail {
    /// Where the valid prefix ends (the new file length).
    pub offset: u64,
    /// How many bytes were discarded.
    pub discarded: u64,
}

impl fmt::Display for TornTail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "discarded {} byte(s) of torn WAL tail at offset {}",
            self.discarded, self.offset
        )
    }
}

/// An opened WAL and the entries recovered from it.
#[derive(Debug)]
pub struct Opened {
    pub wal: Wal,
    pub entries: Vec<WalEntry>,
    pub torn: Option<TornTail>,
}

/// Append handle on `.meshtree/wal.log`. The daemon is the single writer
/// (`daemon.lock`), so no inter-process locking is done here.
#[derive(Debug)]
pub struct Wal {
    file: File,
    path: PathBuf,
    next_seq: u64,
    /// Set when an append failed part-way; the file may end in a partial
    /// frame, so further appends are refused until the WAL is reopened.
    poisoned: bool,
}

impl Wal {
    /// Opens (creating if absent) `<repo_root>/.meshtree/wal.log`, recovers
    /// its entries, and truncates any torn tail.
    pub fn open(repo_root: &Path) -> io::Result<Opened> {
        let dir = repo_root.join(".meshtree");
        let path = dir.join(WAL_FILE);
        let created = !path.exists();

        // Recover and truncate through a read/write handle: Windows append
        // handles lack FILE_WRITE_DATA, which SetEndOfFile requires.
        let mut bytes = Vec::new();
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        file.read_to_end(&mut bytes)?;
        let decoded = decode(&bytes)?;
        let torn = (decoded.valid_len < bytes.len()).then(|| TornTail {
            offset: decoded.valid_len as u64,
            discarded: (bytes.len() - decoded.valid_len) as u64,
        });
        if let Some(t) = torn {
            file.set_len(t.offset)?;
        }
        file.sync_all()?;
        drop(file);
        if created {
            sync_dir(&dir)?;
        }

        let file = OpenOptions::new().append(true).open(&path)?;
        let next_seq = decoded.entries.last().map_or(1, |e| e.seq + 1);
        Ok(Opened {
            wal: Wal {
                file,
                path,
                next_seq,
                poisoned: false,
            },
            entries: decoded.entries,
            torn,
        })
    }

    /// Journals `op` at `at_ms`, durably (`fsync`) before returning.
    pub fn append(&mut self, at_ms: u64, op: WalOp) -> io::Result<WalEntry> {
        let mut entries = self.append_all(at_ms, vec![op])?;
        Ok(entries.pop().expect("one entry per op"))
    }

    /// Journals `ops` in order at `at_ms` with one write and one `fsync`.
    /// A crash leaves a prefix of them (a torn last frame is truncated on
    /// open), the same as appending them one by one.
    pub fn append_all(&mut self, at_ms: u64, ops: Vec<WalOp>) -> io::Result<Vec<WalEntry>> {
        if self.poisoned {
            return Err(io::Error::other(
                "WAL append refused after an earlier failed write; reopen the WAL",
            ));
        }
        let entries: Vec<WalEntry> = ops
            .into_iter()
            .enumerate()
            .map(|(i, op)| WalEntry {
                seq: self.next_seq + i as u64,
                at_ms,
                op,
            })
            .collect();
        let frames: Vec<u8> = entries.iter().flat_map(encode_frame).collect();
        if let Err(e) = self
            .file
            .write_all(&frames)
            .and_then(|()| self.file.sync_data())
        {
            self.poisoned = true;
            return Err(e);
        }
        self.next_seq += entries.len() as u64;
        Ok(entries)
    }

    /// Sequence number the next append will use.
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// Path of the WAL file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Makes a new directory entry durable (Unix); a no-op on Windows, where
/// directories cannot be opened through `std` and NTFS journals metadata.
fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(dir)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

/// Reads and decodes a WAL file without opening it for writing (tooling and
/// tests). A missing file is an empty log.
pub fn read(repo_root: &Path) -> io::Result<Decoded> {
    match fs::read(repo_root.join(".meshtree").join(WAL_FILE)) {
        Ok(bytes) => decode(&bytes),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Decoded {
            entries: Vec::new(),
            valid_len: 0,
        }),
        Err(e) => Err(e),
    }
}

/// A task as reconstructed from the WAL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TaskState {
    pub status: TaskStatus,
    pub branch: String,
    pub worktree: String,
    pub base: String,
    /// Held claims; cleared when the task reaches a terminal status (every
    /// §8 path into one releases claims).
    pub claims: BTreeSet<String>,
    /// `seq` of the last entry applied to this task.
    pub last_seq: u64,
    /// `at_ms` of the last entry applied to this task.
    pub updated_ms: u64,
}

/// An entry that replay skipped, with the reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Anomaly {
    pub seq: u64,
    pub message: String,
}

/// State derived by [`replay`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct WalState {
    pub tasks: BTreeMap<TaskId, TaskState>,
    /// Entries that could not be applied, in log order.
    pub anomalies: Vec<Anomaly>,
    /// Highest `seq` seen (0 for an empty log).
    pub last_seq: u64,
}

impl WalState {
    /// Tasks not in a terminal status (candidates for adoption on restart).
    pub fn live_tasks(&self) -> impl Iterator<Item = (&TaskId, &TaskState)> {
        self.tasks.iter().filter(|(_, t)| !t.status.is_terminal())
    }
}

/// Folds `entries` (in log order) into task states. Pure and deterministic:
/// the same entries always yield the same state. Entries that do not apply
/// (non-increasing `seq`, duplicate spawn, unknown task, `from` not the
/// current status, or a non-§8 edge, `MT-004`) are skipped and recorded as
/// anomalies.
pub fn replay(entries: &[WalEntry]) -> WalState {
    let mut state = WalState::default();
    for entry in entries {
        if let Err(message) = apply(&mut state, entry) {
            state.anomalies.push(Anomaly {
                seq: entry.seq,
                message,
            });
        }
        state.last_seq = state.last_seq.max(entry.seq);
    }
    state
}

fn apply(state: &mut WalState, entry: &WalEntry) -> Result<(), String> {
    if entry.seq <= state.last_seq {
        return Err(format!(
            "seq {} is not greater than {}",
            entry.seq, state.last_seq
        ));
    }
    let id = entry.op.task();
    if let WalOp::Spawn {
        branch,
        worktree,
        base,
        ..
    } = &entry.op
    {
        if state.tasks.contains_key(id) {
            return Err(format!("{id} spawned twice"));
        }
        state.tasks.insert(
            id.clone(),
            TaskState {
                status: TaskStatus::Spawning,
                branch: branch.clone(),
                worktree: worktree.clone(),
                base: base.clone(),
                claims: BTreeSet::new(),
                last_seq: entry.seq,
                updated_ms: entry.at_ms,
            },
        );
        return Ok(());
    }

    if let WalOp::Events { .. } = &entry.op {
        // Lease events change no task state, and a sweep names the expired
        // lease's holder, often another machine's task (T-053).
        return Ok(());
    }
    let task = state
        .tasks
        .get_mut(id)
        .ok_or_else(|| format!("{id} was never spawned"))?;
    match &entry.op {
        WalOp::Spawn { .. } => unreachable!("handled above"),
        WalOp::Transition { from, to, .. } => {
            if task.status != *from {
                return Err(format!("{id} is {}, not {from}", task.status));
            }
            if !from.can_transition_to(*to) {
                return Err(format!(
                    "[{}] {from} -> {to} is not a lifecycle edge",
                    meshtree_core::ERROR_CODE_ILLEGAL_TRANSITION
                ));
            }
            task.status = *to;
            if to.is_terminal() {
                task.claims.clear();
            }
        }
        WalOp::Claim { paths, .. } => {
            if task.status.is_terminal() {
                return Err(format!("{id} is {} and cannot claim", task.status));
            }
            task.claims.extend(paths.iter().cloned());
        }
        WalOp::Release { paths, .. } => {
            for p in paths {
                task.claims.remove(p);
            }
        }
        WalOp::Events { .. } => {}
    }
    task.last_seq = entry.seq;
    task.updated_ms = entry.at_ms;
    Ok(())
}

/// CRC-32 (IEEE 802.3, reflected, polynomial `0xEDB88320`).
pub fn crc32(bytes: &[u8]) -> u32 {
    const TABLE: [u32; 256] = {
        let mut table = [0u32; 256];
        let mut i = 0;
        while i < 256 {
            let mut c = i as u32;
            let mut k = 0;
            while k < 8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
                k += 1;
            }
            table[i] = c;
            i += 1;
        }
        table
    };
    !bytes.iter().fold(!0u32, |c, &b| {
        TABLE[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u128) -> TaskId {
        TaskId::from_ulid(n)
    }

    fn entry(seq: u64, op: WalOp) -> WalEntry {
        WalEntry {
            seq,
            at_ms: 1_000 + seq,
            op,
        }
    }

    fn spawn(task: TaskId) -> WalOp {
        WalOp::Spawn {
            branch: format!("meshtree/{task}"),
            worktree: format!(".meshtree/worktrees/{task}"),
            base: "main".into(),
            task,
        }
    }

    fn transition(task: TaskId, from: TaskStatus, to: TaskStatus) -> WalOp {
        WalOp::Transition { task, from, to }
    }

    #[test]
    fn crc32_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn frame_roundtrip_and_wire_shape() {
        let e = entry(
            3,
            WalOp::Claim {
                task: id(7),
                paths: vec!["src/**".into()],
            },
        );
        let frame = encode_frame(&e);
        let text = String::from_utf8(frame.clone()).unwrap();
        assert!(text.ends_with('\n'));
        assert_eq!(text.matches('\n').count(), 1);
        let json = &text[HEADER_LEN..text.len() - 1];
        assert_eq!(&text[..8], format!("{:08x}", json.len()));
        assert!(json.contains(r#""op":"claim""#), "{json}");
        let decoded = decode(&frame).unwrap();
        assert_eq!(decoded.entries, vec![e]);
        assert_eq!(decoded.valid_len, frame.len());
    }

    #[test]
    fn decode_rejects_mid_log_corruption_but_not_torn_tail() {
        let a = encode_frame(&entry(1, spawn(id(1))));
        let b = encode_frame(&entry(2, spawn(id(2))));

        // Flip a payload byte of the first frame: a valid frame follows.
        let mut corrupt = [a.clone(), b.clone()].concat();
        corrupt[HEADER_LEN + 2] ^= 0x01;
        let err = decode(&corrupt).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{err}");

        // The same flip in the last frame is a torn tail.
        let mut torn = [a.clone(), b].concat();
        let last = torn.len() - 3;
        torn[last] ^= 0x01;
        let decoded = decode(&torn).unwrap();
        assert_eq!(decoded.entries.len(), 1);
        assert_eq!(decoded.valid_len, a.len());
    }

    #[test]
    fn decode_refuses_checksummed_unknown_entry() {
        let json = br#"{"seq":1,"at_ms":0,"op":"teleport","task":"t-0"}"#;
        let mut frame = format!("{:08x} {:08x} ", json.len(), crc32(json)).into_bytes();
        frame.extend_from_slice(json);
        frame.push(b'\n');
        assert_eq!(
            decode(&frame).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn replay_follows_lifecycle_and_records_anomalies() {
        use TaskStatus::*;
        let (a, b) = (id(1), id(2));
        let entries = vec![
            entry(1, spawn(a.clone())),
            entry(2, transition(a.clone(), Spawning, Active)),
            entry(
                3,
                WalOp::Claim {
                    task: a.clone(),
                    paths: vec!["src/a/**".into(), "docs/**".into()],
                },
            ),
            entry(
                4,
                WalOp::Release {
                    task: a.clone(),
                    paths: vec!["docs/**".into()],
                },
            ),
            entry(5, transition(a.clone(), Active, Collecting)),
            entry(6, transition(a.clone(), Collecting, Merging)),
            // Anomalies: illegal edge, stale `from`, unknown task, duplicate
            // spawn, non-increasing seq.
            entry(7, transition(a.clone(), Merging, Spawning)),
            entry(8, transition(a.clone(), Active, Collecting)),
            entry(9, transition(b.clone(), Spawning, Active)),
            entry(10, spawn(a.clone())),
            entry(10, spawn(b.clone())),
            entry(11, spawn(b.clone())),
        ];
        let state = replay(&entries);

        let ta = &state.tasks[&a];
        assert_eq!(ta.status, Merging);
        assert_eq!(
            ta.claims.iter().collect::<Vec<_>>(),
            vec!["src/a/**"],
            "release removed docs/**"
        );
        assert_eq!(ta.last_seq, 6);
        assert_eq!(state.tasks[&b].status, Spawning);
        assert_eq!(state.last_seq, 11);
        let seqs: Vec<u64> = state.anomalies.iter().map(|a| a.seq).collect();
        assert_eq!(seqs, vec![7, 8, 9, 10, 10]);
        assert!(state.anomalies[0].message.contains("MT-004"));
        assert_eq!(state.live_tasks().count(), 2);

        // Terminal status clears claims and leaves the live set.
        let mut more = entries.clone();
        more.push(entry(12, transition(a.clone(), Merging, Merged)));
        let done = replay(&more);
        assert_eq!(done.tasks[&a].status, Merged);
        assert!(done.tasks[&a].claims.is_empty());
        assert_eq!(done.live_tasks().count(), 1);
    }
}
