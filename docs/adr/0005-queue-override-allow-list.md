# ADR-0005: Per-Queue Argument Override Allow-List (`allow_args`)

- **Status:** Proposed (implemented by `T-019`; pending human acceptance)
- **Date:** 2026-10-31
- **Author:** Claude Opus 5.5 (Agent)
- **Authority:** `SPEC.md` §6, §12, §15 (`request_shared_op.command_override`), §17; `AGENTS.md` Standing Order #1

## Context

§17 states that agents "cannot inject arbitrary commands via queue requests (overrides limited to arguments allow-listed per queue)", and §15's `request_shared_op` accepts a `command_override`. §6's `[queue.<name>]` table, however, only defines `concurrency` and `command`: there is no key holding the allow-list, so §17 cannot be implemented without extending the committed config. The 2026-10-30 backlog audit recorded this as blocking `T-019`.

## Decision

1. `[queue.<name>]` gains an optional key `allow_args` (array of strings, default empty). An empty list means **no overrides are accepted** for that queue.
2. `command_override` is a list of extra arguments appended to the queue's configured `command`; it never replaces the command. Each argument must match an allow-list entry:
   - an exact entry (e.g. `"--verbose"`) matches only that argument;
   - an entry ending in `=*` (e.g. `"--filter=*"`) matches `--filter=<value>` where `<value>` is non-empty and uses only `[A-Za-z0-9._/:@+-]`, so no value can carry shell syntax.
3. Allow-list entries must be non-empty and contain no whitespace (validated as `MT-050`).
4. A request with any non-matching argument is refused before a ticket is issued; nothing is queued or run.

Example:

```toml
[queue.test]
concurrency = 1
command = "npm test"
allow_args = ["--verbose", "--filter=*"]
```

## Consequences

- `T-019` adds `allow_args` to `meshtree_core::config::QueueConfig` (the one change outside its `queue` module scope fence) and enforces it in `meshtree_core::queue::check_overrides`.
- `T-031`'s `security_rejects_unlisted_queue_override` tests this rule.
- Existing configs are unaffected: the key is optional and its default allows nothing.
