//! Property tests: arbitrary action sequences cannot violate task lifecycle invariants.
//!
//! Authority: `SPEC.md` §7, §8, §16 (`MT-004`, `ADR-0002`), §18.

use meshtree_core::task::{
    Task, TaskId, TaskIdAllocator, TaskSpec, TaskStatus, ERROR_CODE_ILLEGAL_TRANSITION,
};
use proptest::prelude::*;

/// Independent oracle: the nine edges of the `SPEC.md` §8 state diagram.
fn spec_edge(from: TaskStatus, to: TaskStatus) -> bool {
    use TaskStatus::*;
    const EDGES: [(TaskStatus, TaskStatus); 9] = [
        (Spawning, Active),
        (Spawning, Failed),
        (Active, Collecting),
        (Active, Abandoned),
        (Collecting, Merging),
        (Collecting, Active),
        (Collecting, Failed),
        (Merging, Merged),
        (Merging, Active),
    ];
    EDGES.contains(&(from, to))
}

fn any_status() -> impl Strategy<Value = TaskStatus> {
    prop::sample::select(TaskStatus::ALL.to_vec())
}

fn new_task(id: TaskId) -> Task {
    Task::new(
        id,
        TaskSpec {
            name: "prop-task".to_string(),
            agent: None,
            machine: "mc-prop".to_string(),
            base: "main".to_string(),
            created_at: "2026-11-03T14:02:11Z".to_string(),
            claims: vec!["src/**".to_string()],
        },
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Every attempted transition succeeds iff it is a §8 edge; rejected ones
    /// return `MT-004` and leave the task untouched; the accepted history is
    /// always a valid §8 path starting at `spawning`.
    #[test]
    fn arbitrary_transition_sequences_preserve_lifecycle_invariants(
        ulid in any::<u128>(),
        attempts in prop::collection::vec(any_status(), 0..64),
    ) {
        let mut task = new_task(TaskId::from_ulid(ulid));
        prop_assert_eq!(task.status, TaskStatus::Spawning);
        let mut history = vec![task.status];

        for next in attempts {
            let before = task.clone();
            match task.transition(next) {
                Ok(()) => {
                    prop_assert!(spec_edge(before.status, next));
                    prop_assert!(!before.status.is_terminal());
                    prop_assert_eq!(task.status, next);
                    history.push(next);
                }
                Err(err) => {
                    prop_assert!(!spec_edge(before.status, next));
                    prop_assert_eq!(err.code(), ERROR_CODE_ILLEGAL_TRANSITION);
                    prop_assert_eq!(&task, &before);
                }
            }
            // Only the status ever changes; identity fields are immutable.
            prop_assert_eq!(&task.id, &before.id);
            prop_assert_eq!(&task.branch, &format!("meshtree/{}", task.id));
            prop_assert_eq!(&task.worktree, &format!(".meshtree/worktrees/{}", task.id));
        }

        // History invariants.
        prop_assert_eq!(history[0], TaskStatus::Spawning);
        for pair in history.windows(2) {
            prop_assert!(spec_edge(pair[0], pair[1]), "{} -> {}", pair[0], pair[1]);
        }
        // At most one terminal status, and only as the final entry.
        let terminal_positions: Vec<usize> = history
            .iter()
            .enumerate()
            .filter(|(_, s)| s.is_terminal())
            .map(|(i, _)| i)
            .collect();
        prop_assert!(terminal_positions.len() <= 1);
        if let Some(&pos) = terminal_positions.first() {
            prop_assert_eq!(pos, history.len() - 1);
        }
        // `merged` is only ever entered from `merging`.
        for pair in history.windows(2) {
            if pair[1] == TaskStatus::Merged {
                prop_assert_eq!(pair[0], TaskStatus::Merging);
            }
        }

        // The record round-trips through `task.json` in whatever state it reached.
        let json = serde_json::to_string(&task).unwrap();
        let parsed: Task = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(parsed, task);
    }

    /// Once terminal, no sequence of attempts can move the task again.
    #[test]
    fn terminal_states_absorb_all_transitions(
        terminal in prop::sample::select(vec![
            TaskStatus::Merged,
            TaskStatus::Failed,
            TaskStatus::Abandoned,
        ]),
        attempts in prop::collection::vec(any_status(), 1..32),
    ) {
        let mut task = new_task(TaskId::from_ulid(0));
        task.status = terminal;
        for next in attempts {
            let err = task.transition(next).expect_err("terminal state must reject");
            prop_assert_eq!(err.code(), ERROR_CODE_ILLEGAL_TRANSITION);
            prop_assert_eq!(task.status, terminal);
        }
    }

    /// Allocated IDs are canonical and strictly increasing (as ULIDs and as
    /// strings) under arbitrary clock readings, including stalls and rewinds.
    #[test]
    fn allocator_ids_canonical_and_strictly_increasing(
        draws in prop::collection::vec((0u64..(1u64 << 48), any::<u128>()), 1..64),
    ) {
        let mut alloc = TaskIdAllocator::new();
        let mut prev: Option<TaskId> = None;
        for (ms, entropy) in draws {
            let id = alloc.allocate(ms, entropy);
            prop_assert_eq!(TaskId::parse(id.as_str()), Some(id.clone()));
            prop_assert_eq!(TaskId::from_ulid(id.ulid()), id.clone());
            if let Some(p) = &prev {
                prop_assert!(id.ulid() > p.ulid());
                prop_assert!(id.as_str() > p.as_str());
                prop_assert!(id.timestamp_ms() >= p.timestamp_ms());
            }
            prev = Some(id);
        }
    }

    /// Encoding is a bijection between u128 and canonical IDs.
    #[test]
    fn task_id_encoding_roundtrips(ulid in any::<u128>()) {
        let id = TaskId::from_ulid(ulid);
        prop_assert_eq!(id.ulid(), ulid);
        prop_assert_eq!(TaskId::parse(id.as_str()), Some(id));
    }
}
