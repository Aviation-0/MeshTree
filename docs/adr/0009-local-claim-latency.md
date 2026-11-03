# ADR-0009: Meeting the Local Claim Budget (p95 < 50 ms)

- **Status:** Proposed (awaiting Lucas Pan)
- **Date:** 2026-11-02
- **Author:** Claude Opus 5.5 (Agent)
- **Authority:** `SPEC.md` §4 (git plumbing via the git CLI, not libgit2), §9 (event log, deterministic replay, CAS, "local operations are never blocked by network state"), §10 (daemon serializes clients; WAL crash safety), §18 (benchmark: claim acquire (local) p95 < 50 ms), §22; `AGENTS.md` Standing Order #1

## Context

`T-034` added the §18 benchmark suite. On the 3-OS CI smoke run (41557533637), local claims through the daemon miss the budget everywhere:

| OS | Claim acquire (local) p95 | Target |
|---|---|---|
| Linux x86_64 | 99 ms | < 50 ms |
| macOS aarch64 | 856 ms | < 50 ms |
| Windows x86_64 | 1804 ms | < 50 ms |

A daemon `claim_paths` runs one full `CoordStore::sync` round synchronously before answering. Without a remote, that round costs about **2N + L + 8 `git` subprocesses**, where N is the number of events in the log and L the number of live leases:

- `ref_read` and `ls_tree` of the coordination tip;
- `cat_blob` for **every** `log/*.json` (`CoordStore::read_log`): N processes;
- `hash_object` for **every** file of the rebuilt tree (`write_tree`): N + L processes;
- `mktree` per directory and for the root, the unchanged-tree check, `commit_tree`, `update_ref`;
- with a remote, `ls-remote` + `fetch` + `push` on top.

Two consequences:

1. **The cost grows with history.** The log is append-only and never compacted, so every claim gets slower as the repository is used. The smoke run reached only ~80 events. A day of real use reaches thousands, which would put claims at seconds or minutes.
2. **Process creation dominates.** One `git` invocation costs about 1 ms on Linux and 10–25 ms on macOS and Windows runners. Even a fixed handful of processes per claim exceeds 50 ms on Windows.

§4 fixes git access to the CLI (libgit2/gix rejected), so the answer cannot be "write objects in-process".

Alternatives considered:

- **Relax the §18 target.** This keeps a claim path whose cost grows without bound, and the budget exists so agents can claim freely before every edit. Rejected.
- **In-process git (gix/libgit2).** Contradicts §4's design decision. Rejected.
- **Make only the reads incremental and keep the synchronous ref write.** That fixes the growth (consequence 1) but still leaves about 8 processes per claim, about 100–200 ms on Windows. It fails the budget on its own, so it is part 1 of the decision, not the whole decision.

## Decision

### Part 1: a sync round's cost does not depend on history (no SPEC change)

Make `CoordStore::sync` O(changes) in git subprocesses, not O(log):

- **Event cache.** Event files are immutable and ULID-named, so a blob OID always decodes to the same event. The store keeps a decoded-event cache keyed by blob OID and reads only blobs it has not seen, in one `git cat-file --batch` process.
- **Tree reuse.** The `log/` subtree is rebuilt from the previous `log/` tree plus the new entries, and only new or changed files are hashed. Unchanged lease snapshots keep their blob OIDs. New blobs are written in one batched call.
- **Determinism unchanged.** The tree bytes, replay and merge rules are byte-identical to today's (§9 "any two machines replaying the same log derive byte-identical lease tables"). Only how they are computed changes. The existing property and two-clone tests guard it.

This alone stops the growth and helps every caller, including CLI claims without a daemon.

### Part 2: the daemon answers claims from memory; the ref write follows (SPEC §9 and §10 amended)

When a daemon is running:

1. The daemon keeps the replayed lease table (and the event-id set) in memory. It is the only local writer: CLI claims route through it (§10) and it serializes them under its operation mutex.
2. `claim_paths` / `release_paths` decide against that table **in memory**, using the same `replay` and conflict rule (§7, `ADR-0003`), so a local conflict is still an immediate `MT-001`.
3. Before answering, the daemon appends the **events** (with their ULIDs, timestamps and counters) to the WAL. The WAL is fsynced per append, which is about 1 ms on SSDs. `WalOp::Claim` / `WalOp::Release` gain the event records, so recovery can re-apply them idempotently, since event ids are unique.
4. A background writer coalesces pending events into one coordination-ref commit (local CAS) and push, using the existing dirty-flush path. It runs within at most **250 ms** of the first pending event, or immediately on `sync`, `collect`, `heartbeat` or shutdown.
5. **Crash safety.** On restart, recovery re-applies WAL events missing from the ref (§10 Crash safety). An acknowledged claim is never lost.
6. **Freshness for remote conflicts.** The table includes remote events from the last fetch. The background writer also fetches every `lease.heartbeat_secs / 4` (at most every 30 s) and on every flush. A remote winner discovered later revokes the local claim through the existing revocation path (§9 step 3, `MT-001` notification). This is the advisory model §9 already has whenever a push is deferred, now with a bounded window.
7. **Opt-in synchronous mode.** `claim_paths` gains an optional `sync: true` (CLI `meshtree claim --sync`) that waits for the commit and push before answering. That is today's behavior, for callers that want the remote check up front.

The CLI without a daemon is unchanged: it stays synchronous, with Part 1's speed-up.

### SPEC amendments on acceptance

- §9 "Atomic update via CAS": add that a running daemon decides local claims against its in-memory replay, journals the events to the WAL before acknowledging, and commits/pushes them asynchronously (≤ 250 ms coalescing window). Remote conflicts found afterwards revoke through the existing path.
- §10 "Crash safety": the WAL carries claim/release **events**; recovery re-applies those missing from `refs/meshtree/coord`.
- §15 `claim_paths`: optional `sync?` input; output gains `coord: "pending" | "committed" | "pushed"`.

## Consequences

- **Expected latency.** A local claim becomes IPC + in-memory replay + one WAL fsync, about 1–5 ms on every OS. That puts p95 well under 50 ms, independent of history length.
- **Faster everywhere.** Part 1 also speeds up `sync`, `collect`'s claim check, heartbeats, CLI claims without a daemon, and the cross-machine CAS round (§18 p95 < 2 s gains headroom for real network RTT).
- **Cost: a revocation window.** A claim acknowledged before the background fetch sees a conflicting remote claim can be revoked up to one flush/fetch interval later. Today the same happens whenever a push is deferred. The merge gate's `MT-032` check is unchanged, so no conflicting work merges silently.
- **Cost: more WAL complexity.** WAL entries grow (they carry event records) and recovery gains an "apply missing events" step, covered by the existing chaos test plus a new kill-between-ack-and-flush test.
- **Tasks, added on acceptance** (as with `ADR-0007` / `T-046`):
  - **T-048** (M4): Part 1 (event cache, batched reads and writes, tree reuse). Exit: the local claim p95 benchmark no longer grows with log size (bench rows at 100 and 2,000 events), determinism tests unchanged.
  - **T-049** (M4, deps `T-048`): Part 2 (in-memory claim decision, WAL events, ≤ 250 ms background commit and periodic fetch, `sync` opt-in). Exit: `xtask bench` local claim p95 < 50 ms on all 3 CI OSes, and a chaos test killing the daemon between acknowledgement and flush loses no claim.
- **If rejected:** §18's local-claim budget stays unmet. At minimum, Part 1 should still land as a plain performance fix (it needs no SPEC change), because the current cost grows without bound.
