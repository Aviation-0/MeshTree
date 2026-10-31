# ADR-0004: Placement of §20 Week-3 and Week-4 Exit Criteria in the Task Queue

- **Status:** Accepted
- **Date:** 2026-10-31
- **Author:** Claude Opus 5.5 (Agent), approved by Lucas Pan (2026-10-31 trajectory audit)
- **Authority:** `SPEC.md` §8, §13, §18, §20; `AGENTS.md` Standing Order #1

## Context

The 2026-10-31 trajectory audit compared `docs/tasks/backlog.md` against `SPEC.md` §20 and found two roadmap exit criteria that had moved without a record:

- §20 Week 3 exit: "Full lifecycle integration test green". The §18 full-lifecycle test is `spawn → claim → handoff → collect → merged` with the generated `AGENTS.md` asserted, but handoff records and `AGENTS.md` rendering are §20 **Week 4** deliverables. The test therefore lives in `T-029` (M3), not M2.
- §20 Week 4 exit: "Two real agents (Antigravity + Vibe CLI) run the §18 benchmark scenario end-to-end". M3's only coverage was the automated `T-029` test `two_agents_mcp_and_cli_parallel_session_zero_overwrites`; the real two-agent run appeared only in the M5 dogfood study (`T-036`).

## Decision

1. The §20 Week-3 "full lifecycle" exit is satisfied by `T-029`'s `full_lifecycle_spawn_claim_handoff_collect_merged`. M2's exit is the daemon/merge-gate/chaos subset (`T-018`, `T-020`, `T-021`), which exercises `spawn → claim → collect → merged` without handoffs. `SPEC.md` §20 text is unchanged; this records the one-week shift forced by its own Week-4 placement of handoffs.
2. The §20 Week-4 exit is restored to M3 as a human-in-the-loop task, `T-045`: two real agents (Antigravity + Vibe CLI) run the §18 two-agent scenario end-to-end on the MVP build, with the session log and coordination-ref trace committed. `T-045` is a dependency of every M4 task's milestone gate via `T-030`. The automated `T-029` test remains as the regression guard.

## Consequences

- `T-045` cannot be completed by an agent alone; it is marked as requiring the human operator.
- `T-036` (M5 dogfood) remains the two-machine published case study; `T-045` is the single-machine MVP acceptance run.
