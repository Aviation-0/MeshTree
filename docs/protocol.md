# MeshTree Protocol Reference

This document specifies MeshTree's wire formats: the `refs/meshtree/coord` coordination ref, the replay rules that turn its event log into a lease table, the daemon's NDJSON IPC protocol, and the `MT-xxx` error catalog. It describes what the code does as of `v0.x`; `SPEC.md` remains authoritative, and where this document and the SPEC disagree, the SPEC wins and this document is a bug.

- **Authority:** `SPEC.md` §4, §5, §7, §9, §10, §14–§17; `ADR-0003`, `ADR-0005`, `ADR-0007`, `ADR-0008`, `ADR-0009`, `ADR-0010`.
- **Companion:** [`architecture.md`](architecture.md) covers the daemon internals (WAL, merge gate, teardown) that implement this protocol.
- **Examples are tested.** Every ` ```json <kind> ` block below is parsed by the test `docs_json_examples_parse_against_core_types` (`crates/meshtree-platform/tests/docs_schema.rs`) against the Rust type named in its tag, and must serialize back to the same JSON. A field that is renamed in code fails that test until this document is updated.

## Contents

1. [Conventions](#1-conventions)
2. [The coordination ref](#2-the-coordination-ref)
3. [Records](#3-records)
4. [Replay](#4-replay)
5. [Compaction (`ADR-0010`)](#5-compaction-adr-0010)
6. [Sync rounds and CAS](#6-sync-rounds-and-cas)
7. [Daemon claim path (`ADR-0009`)](#7-daemon-claim-path-adr-0009)
8. [Participants: daemons, CLI-only, cloud and unmanaged agents (`ADR-0008`)](#8-participants-adr-0008)
9. [IPC](#9-ipc)
10. [Error catalog](#10-error-catalog)

---

## 1. Conventions

| Item | Format |
|---|---|
| Timestamp | RFC 3339 UTC with a `Z` suffix. Writers emit exactly millisecond precision, `YYYY-MM-DDTHH:MM:SS.mmmZ`. Readers accept 0–9 fractional digits and truncate to milliseconds. |
| ULID | 26 characters of uppercase Crockford base32 (no `I`, `L`, `O`, `U`); the first character is `0`–`7`. |
| Task id | `"t-" + ULID`, e.g. `t-01JAXGZK7Q4D3RT2WFH8MB3VQY`. |
| Event id | A bare ULID (no prefix). It is also the event's file name in `log/`. |
| Ticket id | `"q-" + ULID`. |
| Machine id | `[machine] id` from the config. If absent: `mc-` + the first 16 hex digits of SHA-1 over the OS machine identifier, falling back to `mc-<hostname>` (`ADR-0011`). |
| Path pattern | Normalized: forward slashes, no leading `./`, no `..`, `**` = any depth, `*` = within one segment (§7). Patterns that reach `.meshtree/` in any letter case are refused (§17). |
| Canonical JSON | What MeshTree writes into the coordination ref: `serde_json` pretty-printing (two-space indent), struct fields in declaration order, map keys sorted, a trailing LF. Two machines holding the same data write byte-identical blobs, so they write byte-identical trees. |

## 2. The coordination ref

`refs/meshtree/coord` is an **orphan ref**: its commits share no history with any branch, it is never checked out, and it never touches a working tree (§9, Trap 1). Each commit's tree has this layout:

```text
refs/meshtree/coord
├── log/<ULID>.json                       # event log above the snapshot watermark (source of truth)
├── snapshot/<W>.json                     # replay state at watermark W (ADR-0010; at most one)
├── leases/<sha1(normalized-path)>.json   # derived lease table (fast remote reads)
└── tasks/<task-id>.json                  # cross-machine task registry
```

| Path | Content | Written by | Authoritative? |
|---|---|---|---|
| `log/<ULID>.json` | One [lease event](#31-lease-event). The file name must equal `<event id>.json`. | The machine that emitted the event | **Yes.** The lease table is a pure function of the snapshot plus the log. |
| `snapshot/<W>.json` | [Snapshot](#35-snapshot-adr-0010) of the replay state at watermark `W`. | Any daemon's background writer (`ADR-0010`) | Yes, for everything at or below `W`. |
| `leases/<sha1>.json` | One [lease](#32-lease) per live path, named by the lowercase hex SHA-1 of the normalized path. | Rewritten on every sync round | **No.** Derived; readers that replay ignore it. |
| `tasks/<task-id>.json` | A [task registry record](#34-task-registry-record). | Only the machine that owns the task | Yes, for that task's cross-machine status. |

Rules for every writer:

- **No merge commits.** Each commit's parent is the tip it was built on; the ref is linear on the remote.
- **Undecodable log entries are skipped, identically everywhere.** A `log/` file that is not valid event JSON, or whose name does not match its `id`, is ignored and reported as a decode issue. It is never deleted, so every machine sees and skips the same file.
- Local tracking of the last fetched remote tip lives at `refs/meshtree/remotes/<remote>/coord`. The remote is `origin` when configured; without a remote the ref is local-only.

### Reading the ref by hand

```bash
git fetch origin refs/meshtree/coord:refs/meshtree/remotes/origin/coord
git ls-tree -r --name-only refs/meshtree/coord
git show refs/meshtree/coord:log/01JAY7K2N8QWERTZ5XC4V6B9MD.json
git log --oneline refs/meshtree/coord          # every round, including compactions, is a commit
```

## 3. Records

### 3.1 Lease event

`log/<ULID>.json`. Rust type: `meshtree_core::lease::LeaseEvent`.

```json lease-event
{
  "id": "01JAY7K2N8QWERTZ5XC4V6B9MD",
  "type": "ACQUIRE",
  "path": "src/auth/**",
  "task": "t-01JAXGZK7Q4D3RT2WFH8MB3VQY",
  "machine": "mc-lucas-mac",
  "ts": "2026-11-03T14:05:00.500Z",
  "counter": 1793801100500000000,
  "ttl_secs": 900
}
```

| Field | Type | Meaning |
|---|---|---|
| `id` | ULID | Unique event id. Duplicates (same id) are the same event. |
| `type` | `ACQUIRE` \| `RELEASE` \| `HEARTBEAT` \| `EXPIRE_SWEEP` | See [replay](#4-replay). |
| `path` | pattern | Normalized path pattern. |
| `task` | task id | Holder task. For `EXPIRE_SWEEP`, the task whose lease is being swept. |
| `machine` | machine id | Emitting machine. |
| `ts` | timestamp | Emitter's wall clock. |
| `counter` | u64 | Emitter's monotonic tie-breaker. MeshTree writes the emitter's clock in nanoseconds, plus the event's index within a batch. |
| `ttl_secs` | u64, optional | Only on `ACQUIRE`. Absent means `[lease] ttl_secs`. |

A `RELEASE` carries no `ttl_secs`:

```json lease-event
{
  "id": "01JAY8B3QH6CMV0X2N4R7T9WKE",
  "type": "RELEASE",
  "path": "src/auth/**",
  "task": "t-01JAXGZK7Q4D3RT2WFH8MB3VQY",
  "machine": "mc-lucas-mac",
  "ts": "2026-11-03T14:40:12.003Z",
  "counter": 1793803212003000000
}
```

An `EXPIRE_SWEEP` names the *holder's* task, and its `machine` is the *sweeper* (here a Windows machine reaping a lease a vanished cloud sandbox left behind):

```json lease-event
{
  "id": "01JAYC4D7GF2H9K3M5N8P0Q6RS",
  "type": "EXPIRE_SWEEP",
  "path": "docs/**",
  "task": "t-01JAXH2M3N4P5Q6R7S8T9V0W1X",
  "machine": "mc-lucas-win",
  "ts": "2026-11-03T16:00:00.000Z",
  "counter": 1793808000000000000
}
```

### 3.2 Lease

`leases/<sha1(path)>.json`, and the entries of a snapshot. Rust type: `meshtree_core::lease::Lease`.

```json lease
{
  "path": "src/auth/**",
  "task": "t-01JAXGZK7Q4D3RT2WFH8MB3VQY",
  "machine": "mc-lucas-mac",
  "acquired_at": "2026-11-03T14:05:00.500Z",
  "ttl_secs": 900,
  "last_heartbeat": "2026-11-03T14:15:00.500Z"
}
```

`acquired_at` is the timestamp of the winning `ACQUIRE`. A re-`ACQUIRE` by the same task while the lease is live keeps it. `last_heartbeat` is the holder's latest `ACQUIRE` or `HEARTBEAT` timestamp.

**Expiry** is evaluated against the holder's timestamps only:

```text
expires_at = last_heartbeat + ttl_secs + skew_grace_secs
expired(now) ⇔ now > expires_at
```

An observer whose clock runs fast by up to `skew_grace_secs` therefore never sweeps a live lease early (`T-030` tests ±30 min of skew).

### 3.3 Rejection

Not stored in the ref. A rejected `ACQUIRE` stays in `log/` for audit, and every machine re-derives the same rejection from it during replay. Daemons surface rejections when they revoke a claim. Rust type: `meshtree_core::lease::Rejection`.

```json rejection
{
  "code": "MT-001",
  "event": "01JAY7K2NARS0T1V2W3X4Y5Z6A",
  "path": "src/auth/login.rs",
  "task": "t-01JAXGZM0B1C2D3E4F5G6H7J8K",
  "machine": "mc-lucas-win",
  "holder": "t-01JAXGZK7Q4D3RT2WFH8MB3VQY",
  "holder_path": "src/auth/**"
}
```

### 3.4 Task registry record

`tasks/<task-id>.json`. This is the local `task.json` record (Rust type `meshtree_core::Task`) plus one extra field, `revision`: the owner's clock in nanoseconds when it published the record.

```json task-record
{
  "id": "t-01JAXGZK7Q4D3RT2WFH8MB3VQY",
  "name": "fix-auth-middleware",
  "agent": "vibe-cli",
  "machine": "mc-lucas-mac",
  "branch": "meshtree/t-01JAXGZK7Q4D3RT2WFH8MB3VQY",
  "worktree": ".meshtree/worktrees/t-01JAXGZK7Q4D3RT2WFH8MB3VQY",
  "base": "main",
  "created_at": "2026-11-03T14:02:11Z",
  "claims": ["src/auth/**", "tests/auth/**"],
  "status": "active",
  "revision": 1793800931000000000
}
```

`status` is one of `spawning`, `active`, `collecting`, `merging`, `merged`, `failed`, `abandoned` (§8). `agent` is omitted when unknown. The owner republishes on `spawn`, failed spawns, `abort`, merges and daemon sweeps (`T-043`, `T-047`). When two tips disagree about a task, the record with the higher `revision` wins; on a tie, the greater bytes win, so every machine picks the same record.

### 3.5 Snapshot (`ADR-0010`)

`snapshot/<W>.json`. Written by daemons since `T-050` (Rust type: `meshtree_core::lease::Snapshot`). `W` in the file name is the watermark rendered as `<ts as unix ms>-<counter>-<machine>-<id>`, which sorts like the ordering key for equal-width timestamps. Readers must use the `watermark` field, not the file name.

```json snapshot
{
  "watermark": {
    "ts": "2026-11-01T09:00:00.000Z",
    "counter": 1793610000000000000,
    "machine": "mc-lucas-win",
    "id": "01JAQZ7X1Y2Z3A4B5C6D7E8F9G"
  },
  "leases": [
    {
      "path": "src/auth/**",
      "task": "t-01JAXGZK7Q4D3RT2WFH8MB3VQY",
      "machine": "mc-lucas-mac",
      "acquired_at": "2026-10-31T22:05:00.500Z",
      "ttl_secs": 900,
      "last_heartbeat": "2026-11-01T08:55:00.500Z"
    }
  ],
  "rejected": ["01JAQY0A1B2C3D4E5F6G7H8J9K"]
}
```

| Field | Meaning |
|---|---|
| `watermark` | Ordering key `(ts, counter, machine, id)` of the newest event folded into the snapshot. |
| `leases` | The replay state's lease table at `W`, in path order, with holder timestamps unchanged. Leases that are expired at `W` but not yet swept are kept, because expiry is decided at the next event's timestamp, not at `W`. |
| `rejected` | Ids of folded `ACQUIRE` events that were rejected, in replay order, kept for audit. |

## 4. Replay

Replay derives the lease table from the coordination tree. It is a pure function of the **set** of events: input order and duplicates never change the output. Any two machines replaying the same set derive byte-identical lease tables (`replay_determinism_200_random_streams`). Rust: `meshtree_core::lease::replay`.

1. **Load.** Read `snapshot/<W>.json` if present and use its leases as the initial table. Decode every `log/*.json`, skipping undecodable entries (§2), and drop events whose ordering key is ≤ `W`.
2. **Union and sort.** De-duplicate by `id`. Sort by the ordering key `(ts, counter, machine, id)`, then by `type`, `path`, `task`, `ttl_secs`, so even malformed logs with two different events sharing one key sort identically everywhere.
3. **Fold** each event onto the table:

| Event | Effect |
|---|---|
| `ACQUIRE` | Consider every lease held by **another** task whose path is identical or [conflicts](#conflict-rule). If any of them is live at the event's `ts`, the event is **rejected** (`MT-001`, recorded against the first such lease in path order) and nothing changes. Otherwise expired conflicting leases are dropped and the event's lease is inserted. A live lease of the same task on the same path keeps its `acquired_at`. `ttl_secs` defaults to `[lease] ttl_secs`. |
| `RELEASE` | Removes the lease on `path` if `task` holds it; otherwise ignored. |
| `HEARTBEAT` | If `task` holds a lease on `path` that is live at the event's `ts`, sets `last_heartbeat = max(last_heartbeat, ts)`. A heartbeat after expiry does not revive the lease: the holder must re-acquire (`MT-002`). |
| `EXPIRE_SWEEP` | Removes the lease on `path` only if `task` holds it **and** it is expired at the sweep's own `ts`. Otherwise it is a no-op. That makes sweeps idempotent and harmless when early. |

4. **Derive.** Rewrite `leases/` from the resulting table.

Only `ACQUIRE` and `RELEASE` are written by claim operations. `HEARTBEAT` is written by the daemon supervisor for the claims of tasks still in use. `EXPIRE_SWEEP` may be written by any machine for a lease that `sweep_candidates(table, now, skew_grace_secs)` returns. Since `T-053`, every daemon supervisor pass emits one `EXPIRE_SWEEP` per lease expired at its own clock (any machine's), journaled and committed like a claim.

### Conflict rule

Two patterns conflict when either matches the other's canonical literal (wildcards replaced by a reserved probe segment), or either is a directory prefix of the other (§7, `ADR-0003`). The rule is a conservative over-approximation: false positives (over-blocking) are allowed, false negatives are not (`pattern_conflict_exhaustive_case_table`).

## 5. Compaction (`ADR-0010`)

Implemented by `T-050` (`meshtree_core::lease::compact`, `CoordStore::with_compaction`). Compaction keeps the cost of a sync round proportional to live state instead of to the repository's age.

- **Horizon.** `COMPACT_HORIZON = max(24 h, 4 × (ttl_secs + 2 × skew_grace_secs))`. An event `e` is foldable iff `e.ts ≤ newest.ts − COMPACT_HORIZON`, where `newest` is the newest event **in the log**, never the wall clock.
- **Watermark.** `W` is the greatest ordering key among foldable events. The snapshot is the replay state after folding every event with key ≤ `W` (onto the previous snapshot, if any).
- **Trigger.** A daemon's background writer compacts when more than `COMPACT_THRESHOLD = 1,000` events are foldable. The CLI without a daemon never compacts; it only reads snapshots.
- **Commit.** A compaction is an ordinary sync round (§6): it replaces `snapshot/` and removes the folded `log/` files in one commit.
- **Concurrent snapshots.** When two tips carry different snapshots, the larger `W` wins and events with key ≤ that `W` are dropped from the union.
- **Late events.** An event that reaches the ref after a compaction passed it (its machine was offline for longer than the horizon) is dropped. The originating daemon detects on its next sync that its WAL-journaled event is absent from the ref and below `W`, and re-issues it as a fresh event with a new id and key. A re-issued `ACQUIRE` wins or is rejected (`MT-001`) exactly like a fresh claim.
- **Invariant.** `replay(compact(L)) == replay(L)` for every log `L`: the lease table is byte-identical; rejections of folded events are kept as ids in the snapshot's `rejected`, and replay reports full rejections for the events above `W` (`compaction_replay_equivalence_props`).
- **History.** Folded events remain reachable in git history (`git log -p refs/meshtree/coord`); only the current tree forgets them.

## 6. Sync rounds and CAS

One round, `CoordStore::sync`:

1. `git fetch origin refs/meshtree/coord` into the tracking ref (skipped without a remote).
2. Union the remote tip's log, the local tip's log, and the new events. Keep every task record of both tips, resolved by `revision` (§3.4); records passed in by the caller replace both.
3. Replay (§4) and build the canonical tree. Blobs a tip already holds keep their object ids. Only new or changed files are written, in one batch. Unchanged subtrees are reused (`ADR-0009` Part 1), so a round's git process count does not depend on the log's length.
4. Commit with the remote tip as parent, and advance the local ref with a CAS:
   `git update-ref refs/meshtree/coord <new> <expected-old>`.
5. Push with the cross-machine CAS:
   `git push origin refs/meshtree/coord --force-with-lease=refs/meshtree/coord:<expected-old>`.
6. A rejected push means another machine won the race. Go back to step 1, at most **5** attempts with jittered backoff, then fail with `MT-003`.

A network or auth failure that is not a lost race never blocks local progress: the local ref still advances, the round reports the push as **deferred**, and a later round (or `meshtree sync`) publishes it. Push rejections are classified independently of the git locale (`T-044`).

The sequence diagram in `SPEC.md` §9 shows two daemons racing for `src/auth/**`: the loser's push is rejected, it re-fetches, its replay records its own `ACQUIRE` as rejected, and it revokes the local claim and notifies the agent (`MT-001`).

## 7. Daemon claim path (`ADR-0009`)

With a daemon running, `claim_paths` and `release_paths` never wait for git:

1. **Decide in memory.** The daemon keeps the replay of the log as of the local ref, plus the events it has acknowledged but not yet committed (*pending*). Before each decision it compares the ref's tip with the one it read, by reading the ref file without running git, and reloads the log only if the tip moved. New events are folded onto the cached replay when they sort after every logged event, which holds for events created now; otherwise it runs a full replay. A local conflict is an immediate `MT-001`. A batch is all-or-nothing.
2. **Journal, then answer.** The batch's events (ULID, timestamp, counter) are appended to the WAL with `fsync` **before** the response is sent (see [`architecture.md` §3](architecture.md#3-write-ahead-log)). An acknowledged claim survives a daemon crash.
3. **Respond.** `{"paths": […], "coord": "pending", "deferred": null}`.
4. **Commit in the background.** A writer thread coalesces pending events into one sync round within **250 ms** of the oldest acknowledgement (currently after 25 ms). It flushes immediately on `sync`, `collect`, heartbeat and shutdown, fetches every `heartbeat_secs / 4` (at least 1 s, at most 30 s), and retries a deferred push every 5 s.
5. **Late conflicts.** If a remote claim that won the race is discovered after acknowledgement, replay rejects the local event and the daemon revokes the claim: it removes the path from the task's `claims` and reports it in the next `sync` result's `revoked` list.

`sync: true` (`meshtree claim --sync`) answers only after the commit and push. It returns `coord: "committed"` (no remote, or push deferred) or `coord: "pushed"`, and returns `MT-001` if the round revoked any of the requested paths. Without a daemon the CLI performs the whole round synchronously.

## 8. Participants (`ADR-0008`)

| Participant | Writes to the coordination ref | Enforcement it gets |
|---|---|---|
| Local agent via the daemon (MCP or CLI) | Through the daemon (§7) | **Hard** on this machine: the daemon is the single arbiter. |
| CLI without a daemon | Synchronous rounds (§6) | Same rule, evaluated per command. |
| Another machine's daemon | Its own rounds | **Advisory** across machines. The losing machine keeps working in its isolated worktree, but its `collect` is rejected at the merge gate (`MT-032`) if its diff intersects a live claim. |
| Cloud or ephemeral sandbox running MeshTree | Its own rounds, like any remote machine | Advisory plus the merge gate. The local-resource layers (teardown, sharing, shared-op queue) have no work for it. |
| Unmanaged agent (no MeshTree) | Nothing; it holds no claims | Claim-checked only if its work enters through a collected task branch (`MT-032`). Direct pushes to the base branch get git's own conflict detection (`MT-031`). |

A machine that vanishes mid-task (for example a killed sandbox) leaves its leases behind. They expire by the holder-timestamp rule (§3.2), and any machine may reap them with an `EXPIRE_SWEEP`. A sandbox restored from an image finds a `daemon.lock` written on a previous boot of the same machine id; it is detected as stale and adopted (`MT-011`). A lock record from a different machine id *and* a different boot cannot be probed and is refused, so a restored sandbox needs a stable machine id (`[machine] id`, or the OS machine-id derivation of `ADR-0011`). No cloud service is involved.

## 9. IPC

### 9.1 Transport

| | macOS / Linux | Windows |
|---|---|---|
| Endpoint | `<repo>/.meshtree/daemon.sock`. If that path is longer than `sun_path` allows (103 bytes on macOS, 107 on Linux), `/tmp/meshtree-<uid>/<hash16>.sock` instead (`ADR-0007`). | `\\.\pipe\meshtree-<hash16>` |
| `<hash16>` | First 16 hex digits of SHA-1 over the canonical repository root | Same, over the lower-cased canonical root (NTFS is case-insensitive) |
| Access | Socket mode `0600`. The socket's directory must be owned by the daemon user and not group- or world-writable; the fallback directory must be a real `0700` directory owned by that user. | DACL `D:P(A;;GA;;;OW)(A;;GA;;;SY)`: protected, owner and SYSTEM only. `PIPE_REJECT_REMOTE_CLIENTS`; the first instance is created with `FILE_FLAG_FIRST_PIPE_INSTANCE`. |
| TCP | Never | Never |

### 9.2 Framing

Newline-delimited JSON (NDJSON): one request per line and one response per line, in order, per connection. A connection may carry any number of requests. A line that is not a valid request closes the connection. Rust types: `meshtree_platform::ipc::{Request, Response, ErrorBody}`.

```json ipc-request
{"id": 7, "method": "claim_paths", "params": {"task_id": "t-01JAXGZK7Q4D3RT2WFH8MB3VQY", "paths": ["src/auth/**"]}}
```

```json ipc-response
{"id": 7, "ok": true, "result": {"paths": ["src/auth/**"], "coord": "pending", "deferred": null}}
```

```json ipc-response
{"id": 7, "ok": false, "error": {"code": "MT-001", "message": "[MT-001] claim conflict: src/auth/** held by t-01JAXGZM0B1C2D3E4F5G6H7J8K (src/**)"}}
```

`id` is chosen by the caller and echoed back. `params` may be omitted (it defaults to `null`). Exactly one of `result` (when `ok` is true) or `error` is present.

### 9.3 Error codes on the wire

`error.code` is the `MT-xxx` code named in the failure, or, for failures without one, the category of the CLI exit code (§14):

| `error.code` | CLI exit code |
|---|---|
| `usage`, `MT-050` | `10` |
| `task`, `MT-00x`, `MT-03x` | `20` |
| `daemon`, `MT-01x` | `30` |
| `platform`, any other code | `40` |

Unknown methods return `usage`.

### 9.4 Methods

The MCP server (`meshtree mcp`, §15) is a stateless proxy: each MCP tool forwards to the method of the same name. Task-scoped methods count as a sign of life for that task (the §8 inactivity sweeper).

| Method | Params | Result |
|---|---|---|
| `ping` | — | `pid`, `recovery_ms`, `pending_events`, `last_flush_wait_ms`, `last_flush_ms` |
| `spawn_task` | `name`, `base?`, `agent?`, `claims?[]` | `task_id`, `worktree_path`, `branch`, `claims`, `shares` |
| `task_status` | `task_id?` | One [task](#task-record), or all tasks when `task_id` is omitted |
| `list_tasks` | `active_only?` | Task records (`active_only`: only `active`, `collecting`, `merging`) |
| `claim_paths` | `task_id`, `paths[]`, `ttl_secs?`, `sync?` | `paths`, `coord` (`pending` \| `committed` \| `pushed`), `deferred` (reason or `null`) |
| `release_paths` | `task_id`, `paths[]`, `sync?` | Same shape as `claim_paths` |
| `heartbeat` | `task_id` | `task_id`, `status` |
| `sync` | — | `coord` (summary string), `leases` (live count), `revoked[]` of `{task_id, path, holder}` |
| `request_shared_op` | `task_id`, `queue`, `command_override?[]`, `pid`, `boot_id` | A [queue ticket](#queue-ticket) plus `command` (the full command line) and `worktree` |
| `ticket_status` | `ticket_id` | A queue ticket |
| `release_shared_op` | `ticket_id` | `ticket_id`, `released` |
| `queue_status` | `queue?` | Queue tickets, holders first |
| `collect_task` | `task_id`, `message?` | `task_id`, `status`, `integration_branch`, `integration` (merge commit), `attempts`, `pushed`, `push_deferred`, `cleanup` |
| `abort_task` | `task_id` | `task_id`, `status`, `note` |
| `commit_handoff` | `task_id` plus [handoff](#handoff-record) fields | `task_id`, `branch`, `commit`, `handoff_md`, `agents_md` |
| `render_agents_block` | — | `path`, `changed`, `sessions`, `block`, `warnings` |
| `shutdown` | — | `stopping: true` |

Paths in `claim_paths` and `release_paths` are normalized before use, and any pattern that reaches `.meshtree/` is refused with `usage` (§17). `command_override` arguments must each appear verbatim in the queue's `allow_args` (`ADR-0005`); anything else is refused with `usage` before a ticket is issued.

#### Task record

Rust type: `meshtree_core::Task`. Returned by `task_status` and `list_tasks`, and stored locally as `.meshtree/tasks/<id>/task.json`.

```json task
{
  "id": "t-01JAXGZK7Q4D3RT2WFH8MB3VQY",
  "name": "fix-auth-middleware",
  "machine": "mc-lucas-mac",
  "branch": "meshtree/t-01JAXGZK7Q4D3RT2WFH8MB3VQY",
  "worktree": ".meshtree/worktrees/t-01JAXGZK7Q4D3RT2WFH8MB3VQY",
  "base": "main",
  "created_at": "2026-11-03T14:02:11Z",
  "claims": [],
  "status": "spawning"
}
```

#### Queue ticket

Rust type: `meshtree_core::queue::QueueTicket`. `position` counts tickets ahead of this one, granted holders first. `holder` identifies the waiting process; a ticket whose holder process is gone (by pid and boot id), or whose slot has been held longer than the queue's ticket TTL, is evicted.

```json queue-ticket
{
  "id": "q-01JAY9C4RJ7DNW1Y3P5S8V0XMF",
  "queue": "test",
  "task": "t-01JAXGZK7Q4D3RT2WFH8MB3VQY",
  "position": 0,
  "granted": true,
  "granted_at": "2026-11-03T14:20:00.250Z",
  "holder": {"pid": 48211, "boot_id": "boot-1793790000"},
  "args": ["--verbose"]
}
```

#### Handoff record

Rust type: `meshtree_core::handoff::HandoffRecord`. Stored as `handoff.json` and rendered to `HANDOFF.md` on the task branch (§13). Every field is optional on input; omitted fields default to empty.

```json handoff
{
  "objective": "Fix token refresh in the auth middleware",
  "state": "Refresh path fixed; expiry edge case open",
  "done": ["Reproduced the double-refresh race", "Added a mutex around refresh"],
  "next": ["Cover clock rollback in tests"],
  "failing_tests": ["auth::refresh::rollback"],
  "decisions": [{"what": "Mutex, not a channel", "why": "One waiter at a time; simpler to test"}],
  "ruled_out": ["Retrying on 401"]
}
```

## 10. Error catalog

Every MeshTree failure that an agent can act on carries one of these codes (§16). CLI messages start with `[MT-xxx]`, and IPC and MCP errors carry the code in `error.code`.

| Code | Meaning | Raised by | Agent guidance |
|---|---|---|---|
| `MT-001` | Claim conflict: the path is held by another task | `claim`, `spawn --claim`, replay rejection, `sync` revocation | Pick different paths or wait. |
| `MT-002` | Lease expired mid-use | Reserved: no code path returns it yet. Replay silently drops a heartbeat after expiry (§4). | Re-acquire before editing. |
| `MT-003` | Coordination-ref CAS rejected 5 times (retryable) | Sync round (§6) | Internal; the daemon retries on its next round. |
| `MT-004` | Illegal task state transition (not an edge of the §8 diagram; `ADR-0002`) | Any lifecycle command | Re-read `task_status` and act on the current state. |
| `MT-010` | Daemon unreachable | IPC clients, MCP | Start `meshtree daemon`. |
| `MT-011` | Stale daemon lock | Daemon startup | Auto-recovered; informational. |
| `MT-020` | File locked by another process (Windows) | Teardown, `prune` | Retry or wait for GC. |
| `MT-021` | Worktree quarantined | Teardown | Cleanup deferred; the work is safe. |
| `MT-030` | Validation failed | `collect` (`validate`, `promote_validate`) | Fix the tests and collect again. |
| `MT-031` | Merge conflict with integration, or integration push retries exhausted | `collect` | Resolve on the task branch and retry. |
| `MT-032` | Claim overlap at the merge gate | `collect` | Coordinate with the named task. |
| `MT-040` | Share link failed; the strategy fell back to `none` | `spawn` | Informational; dependencies are not shared in this worktree. |
| `MT-050` | Config invalid | Config load | Fix `.meshtree/config.toml`. |
