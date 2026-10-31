# ADR-0002: Add `MT-004` for Illegal Task State Transitions

- **Status:** Accepted
- **Date:** 2026-10-30
- **Author:** Claude Opus 5.5 (Agent), approved by Lucas Pan
- **Authority:** `SPEC.md` §7, §8, §14, §16; `AGENTS.md` Standing Order #1

## Context

`T-003` implements the §8 task lifecycle state machine in `meshtree-core::task`. Every transition outside the §8 state diagram (e.g. `merged → active`, `spawning → merging`) must be rejected with a typed error code so CLI, daemon, and MCP surfaces can report it (§15: "All tools return MeshTree error codes (§16)").

`SPEC.md` §16 has no code for this case. The task brief proposed `MT-010` / `MT-011`, but those are already defined as "daemon unreachable" and "stale daemon lock"; reusing them would make agents misdiagnose a lifecycle violation as a daemon outage.

## Decision

Add one row to the `SPEC.md` §16 error table:

| Code | Meaning | Agent guidance |
|---|---|---|
| `MT-004` | illegal task state transition (not an edge of the §8 diagram) | re-read `task_status`; act on the current state |

`MT-004` sits in the `MT-00x` task/lease group, which maps to CLI exit code `20` ("task/lease error", §14). The state set and edges themselves are unchanged: exactly the seven §7 statuses and the nine edges of the §8 diagram.

## Consequences

- `meshtree-core::task` exposes `ERROR_CODE_ILLEGAL_TRANSITION = "MT-004"` and `TaskError`, mirroring `ConfigError` / `MT-050`.
- `SPEC.md` §16 gains the `MT-004` row (between `MT-003` and `MT-010`).
- `T-028` (MCP error mapping) must include `MT-004`.
