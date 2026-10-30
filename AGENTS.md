# MeshTree — Agent Working Contract

All AI agents working in this repository (IDE orchestrator or terminal CLI) follow these rules without exception.

## Standing orders

1. `SPEC.md` is the authoritative specification. Never contradict it. If you believe the spec is wrong or incomplete, stop and propose a change in `docs/adr/` instead of silently deviating.
2. Work one task at a time from `docs/tasks/backlog.md`: pick the lowest-numbered task with status `todo`, and work only that task.
3. Never modify code outside the task's stated scope. Do not refactor, reformat, or "improve" unrelated modules.
4. A task is done only when `cargo fmt --check`, `cargo clippy --workspace -- -D warnings`, and `cargo test --workspace` all pass locally.
5. Commit per task with the message `T-00X: short summary`. Keep diffs reviewable by a human in one sitting.
6. Before ending any session, append a dated handoff entry under `## Handoff log` in `docs/tasks/backlog.md`: current task, state, what's done, what's next, failing tests if any.
7. CI runs on a 3-OS matrix (Linux, macOS, Windows). Never push with failing CI. Windows-specific behavior must be written for Windows, not ported blindly from Unix.
8. Prefer stdlib and small, well-maintained dependencies. Any new dependency requires a one-line justification in the commit message.
9. Update `docs/tasks/backlog.md` statuses (`todo` / `doing` / `done`) as you work.
10. When uncertain between two implementations, choose the one that is easier to test deterministically.
