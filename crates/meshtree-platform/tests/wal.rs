//! Write-ahead log journaling and replay (`SPEC.md` §10 Crash safety, §18).

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use meshtree_core::{TaskId, TaskStatus};
use meshtree_platform::wal::{self, encode_frame, replay, Wal, WalEntry, WalOp, WAL_FILE};

struct TempRepo(PathBuf);

static COUNTER: AtomicU64 = AtomicU64::new(0);

impl TempRepo {
    fn new() -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "mt-wal-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(path.join(".meshtree")).expect("create temp repo");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn wal_file(&self) -> PathBuf {
        self.0.join(".meshtree").join(WAL_FILE)
    }
}

impl Drop for TempRepo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn id(n: u128) -> TaskId {
    TaskId::from_ulid(n)
}

fn spawn(task: &TaskId) -> WalOp {
    WalOp::Spawn {
        task: task.clone(),
        branch: format!("meshtree/{task}"),
        worktree: format!(".meshtree/worktrees/{task}"),
        base: "main".into(),
    }
}

fn transition(task: &TaskId, from: TaskStatus, to: TaskStatus) -> WalOp {
    WalOp::Transition {
        task: task.clone(),
        from,
        to,
    }
}

/// spawn → claim → collect → merge for `a`, spawn → claim → abort for `b`,
/// and `c` left mid-merge (the crash case §10 recovery must adopt).
fn lifecycle_ops() -> Vec<WalOp> {
    use TaskStatus::*;
    let (a, b, c) = (id(0xA), id(0xB), id(0xC));
    vec![
        spawn(&a),
        transition(&a, Spawning, Active),
        WalOp::Claim {
            task: a.clone(),
            paths: vec!["src/auth/**".into()],
        },
        spawn(&b),
        transition(&b, Spawning, Active),
        WalOp::Claim {
            task: b.clone(),
            paths: vec!["docs/**".into()],
        },
        transition(&a, Active, Collecting),
        transition(&a, Collecting, Merging),
        transition(&b, Active, Abandoned),
        transition(&a, Merging, Merged),
        spawn(&c),
        transition(&c, Spawning, Active),
        WalOp::Claim {
            task: c.clone(),
            paths: vec!["src/db/**".into(), "tests/db/**".into()],
        },
        transition(&c, Active, Collecting),
        transition(&c, Collecting, Merging),
    ]
}

#[test]
fn wal_append_and_deterministic_replay() {
    let repo = TempRepo::new();
    let ops = lifecycle_ops();

    // Fresh WAL; append across two sessions to check seq continuity.
    let mut appended = Vec::new();
    {
        let opened = Wal::open(repo.path()).expect("open");
        assert!(opened.entries.is_empty());
        assert!(opened.torn.is_none());
        let mut wal = opened.wal;
        assert_eq!(wal.path(), repo.wal_file());
        for (i, op) in ops[..6].iter().enumerate() {
            appended.push(wal.append(10_000 + i as u64, op.clone()).unwrap());
        }
    }
    let opened = Wal::open(repo.path()).expect("reopen");
    assert_eq!(opened.entries, appended);
    let mut wal = opened.wal;
    assert_eq!(wal.next_seq(), 7);
    for (i, op) in ops[6..].iter().enumerate() {
        appended.push(wal.append(20_000 + i as u64, op.clone()).unwrap());
    }
    drop(wal);

    let seqs: Vec<u64> = appended.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, (1..=ops.len() as u64).collect::<Vec<_>>());

    // The file is exactly the concatenated frames (every append durable).
    let expected: Vec<u8> = appended.iter().flat_map(encode_frame).collect();
    assert_eq!(fs::read(repo.wal_file()).unwrap(), expected);

    // Replay from disk == replay of what was appended, every time.
    let on_disk = wal::read(repo.path()).unwrap();
    assert_eq!(on_disk.entries, appended);
    let state = replay(&on_disk.entries);
    for _ in 0..3 {
        let again = replay(&Wal::open(repo.path()).unwrap().entries);
        assert_eq!(again, state);
        assert_eq!(
            serde_json::to_vec(&again).unwrap(),
            serde_json::to_vec(&state).unwrap()
        );
    }

    assert!(state.anomalies.is_empty(), "{:?}", state.anomalies);
    assert_eq!(state.last_seq, ops.len() as u64);
    let (a, b, c) = (
        &state.tasks[&id(0xA)],
        &state.tasks[&id(0xB)],
        &state.tasks[&id(0xC)],
    );
    assert_eq!(a.status, TaskStatus::Merged);
    assert!(a.claims.is_empty());
    assert_eq!(b.status, TaskStatus::Abandoned);
    assert!(b.claims.is_empty());
    assert_eq!(c.status, TaskStatus::Merging);
    assert_eq!(
        c.claims.iter().map(String::as_str).collect::<Vec<_>>(),
        ["src/db/**", "tests/db/**"]
    );
    assert_eq!(c.worktree, format!(".meshtree/worktrees/{}", id(0xC)));
    let live: Vec<&TaskId> = state.live_tasks().map(|(id, _)| id).collect();
    assert_eq!(live, [&id(0xC)]);
}

#[test]
fn wal_tolerates_truncated_tail_frame() {
    let repo = TempRepo::new();
    let ops = lifecycle_ops();
    let mut wal = Wal::open(repo.path()).unwrap().wal;
    let entries: Vec<WalEntry> = ops[..3]
        .iter()
        .enumerate()
        .map(|(i, op)| wal.append(i as u64, op.clone()).unwrap())
        .collect();
    drop(wal);

    let full = fs::read(repo.wal_file()).unwrap();
    let last_frame = encode_frame(&entries[2]);
    let prefix_len = full.len() - last_frame.len();

    // Every possible tear inside the last frame (a crash mid-write), plus a
    // same-length frame with a flipped checksum digit.
    let mut images: Vec<Vec<u8>> = (prefix_len..full.len())
        .map(|cut| full[..cut].to_vec())
        .collect();
    let mut bad_crc = full.clone();
    bad_crc[prefix_len + 9] = if bad_crc[prefix_len + 9] == b'0' {
        b'1'
    } else {
        b'0'
    };
    images.push(bad_crc);

    for image in images {
        fs::write(repo.wal_file(), &image).unwrap();
        let opened = Wal::open(repo.path()).expect("torn tail tolerated");
        assert_eq!(opened.entries, entries[..2], "cut at {}", image.len());
        let discarded = (image.len() - prefix_len) as u64;
        assert_eq!(
            opened.torn.map(|t| (t.offset, t.discarded)),
            (discarded > 0).then_some((prefix_len as u64, discarded)),
            "cut at {}",
            image.len()
        );
        assert_eq!(
            fs::metadata(repo.wal_file()).unwrap().len(),
            prefix_len as u64,
            "torn tail truncated"
        );

        // Appending after recovery starts on a clean frame boundary and
        // reuses the lost seq.
        let mut wal = opened.wal;
        let redo = wal.append(99, ops[2].clone()).unwrap();
        assert_eq!(redo.seq, 3);
        drop(wal);
        let reopened = Wal::open(repo.path()).unwrap();
        assert!(reopened.torn.is_none());
        assert_eq!(reopened.entries.len(), 3);
        assert_eq!(reopened.entries[2], redo);
        assert!(replay(&reopened.entries).anomalies.is_empty());
    }

    // Corruption before a valid frame is not a torn tail: refuse to open
    // rather than silently drop journaled state.
    let mut corrupt = full.clone();
    corrupt[20] ^= 0x01;
    fs::write(repo.wal_file(), &corrupt).unwrap();
    let err = Wal::open(repo.path()).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData, "{err}");
    assert_eq!(
        fs::read(repo.wal_file()).unwrap(),
        corrupt,
        "left untouched"
    );

    // Garbage appended by a foreign writer after valid frames is a tail.
    fs::write(repo.wal_file(), &full).unwrap();
    OpenOptions::new()
        .append(true)
        .open(repo.wal_file())
        .unwrap()
        .write_all(b"0000")
        .unwrap();
    let opened = Wal::open(repo.path()).unwrap();
    assert_eq!(opened.entries, entries);
    assert_eq!(opened.torn.unwrap().discarded, 4);
}
