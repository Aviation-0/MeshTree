# ADR-0010: Coordination-Log Compaction by Deterministic Watermark Snapshot

- **Status:** Accepted (accepted by Lucas Pan, 2026-11-03; `SPEC.md` §5, §9, §18 amended; implementation `T-050`, verification `T-051`)
- **Date:** 2026-11-03
- **Author:** Claude Opus 5.5 (Agent)
- **Authority:** `SPEC.md` §5 (`refs/meshtree/coord` layout), §9 (event log, deterministic replay, CAS, holder-timestamp expiry + `skew_grace_secs`), §10 (WAL crash safety), §18 (claim p95 < 50 ms; replay determinism), §22 #2; `ADR-0009`; `AGENTS.md` Standing Order #1

## Context

§5/§9 make `log/<ULID>.json` an append-only event log that is never pruned. `ADR-0009` removed history from the claim *decision* (T-048: constant git process count; T-049: in-memory decision, background commit), but every background commit still unions, replays and re-encodes the whole log and rewrites a `log/` tree with one entry per event ever written. `T-048`'s measurements: claim-round time 877 / 1247 / 2336 ms (Linux / macOS / Windows) at 2,000 events; `T-049` still misses p95 < 50 ms at 2,000 events (67 / 122 / 77 ms), with the background flush competing for CPU. A real repository accrues an `ACQUIRE`, `RELEASE` and periodic `HEARTBEAT`s per claim indefinitely, so §22 #2 ("claim p95 < 50 ms on the dogfood repo") degrades with age no matter how fast the claim path is.

Alternatives considered:

- **Sharded `log/` subtrees** (e.g. by day): bounds `mktree` work via tree reuse, but union and replay remain O(all events).
- **Wall-clock retention** (delete events older than N days): not deterministic across machines with skewed clocks or different fetch states; two machines could prune differently and fail to converge.
- **Snapshot at a watermark derived from the log itself** (this ADR): bounded by live state, deterministic.

## Decision

1. **Snapshot file.** The coordination tree gains `snapshot/<W>.json`, where `W` is the ordering key (§9: `ts, counter, machine, id`) of the newest event folded into it. It holds the replay state at `W`: every lease live at `W` (with holder timestamps, so expiry is still holder-authoritative), plus the ids of `REJECTED` acquires that are still within the audit horizon. At most one snapshot exists in a tree.
2. **Deterministic watermark.** A compaction may fold event `e` only if `e.ts ≤ newest_event.ts − COMPACT_HORIZON`, where `COMPACT_HORIZON = max(24 h, 4 × (ttl_secs + 2 × skew_grace_secs))`, and `newest_event` is taken from the log itself, not the wall clock. `W` is the greatest such key. Any machine computing compaction over the same log derives the same `W` and byte-identical snapshot.
3. **Replay starts from the snapshot.** Replay = load `snapshot/<W>.json`, then fold the remaining `log/` events with key > `W` in order (§9 rules unchanged). Property: for every log `L` and valid `W`, `replay(compact(L, W)) == replay(L)` byte-for-byte; this replaces §9's determinism test as the stronger invariant.
4. **Union across tips.** When two tips hold different snapshots, the larger `W` wins; events with key ≤ the winning `W` are dropped from the union (they are subsumed). Compaction is an ordinary sync round (local CAS + `--force-with-lease`), so concurrent compactions converge like any other write.
5. **Late events below the watermark.** An event a machine pushes after a compaction has passed it (offline longer than the horizon) is dropped by rule 4. The originating daemon detects this on its next sync (its WAL-journaled event is absent from the ref and below `W`) and re-issues it as a fresh event with a new key: a re-issued `ACQUIRE` either wins or is `REJECTED` → `MT-001` revocation, exactly as a fresh claim. `HEARTBEAT`/`RELEASE` below `W` are re-issued the same way. No acknowledged claim is silently lost; the horizon makes this rare (≥ 24 h offline).
6. **Who compacts.** Any daemon, inside its background writer, when the log holds more than `COMPACT_THRESHOLD = 1,000` foldable events. CLI without a daemon never compacts (it only reads snapshots).

## SPEC amendments on acceptance

- §5: add `snapshot/<W>.json` to the `refs/meshtree/coord` layout; `log/` becomes "append-only above the snapshot watermark".
- §9 "Deterministic replay": step 1 loads the snapshot, union drops events ≤ `W`; add the compaction rule and the late-event re-issue rule (a short subsection).
- §18: replay-determinism property extended to `replay(compact(L)) == replay(L)`; add "claim p95 < 50 ms at ≥ 10,000 lifetime events".

## Consequences

- Background commit and union cost become O(live leases + events within the horizon), independent of repository age. Combined with `ADR-0009`, local claim latency no longer degrades with history.
- The audit trail older than the horizon is reduced to the snapshot. Full history remains in git (`git log refs/meshtree/coord`), since every compaction is a commit.
- Wire-format change before v1.0: older binaries cannot read compacted refs. Acceptable pre-1.0; `docs/protocol.md` (`T-033`) documents the format.
- Implementation tracked as a new task (`T-050`); full-scale verification in the benchmark task (`T-051`).
