# ADR-0008: Ephemeral Cloud Agents as Fleet Participants

- **Status:** Accepted (accepted by Lucas Pan, 2026-11-02; `SPEC.md` §9 amended; `T-036` scope extended)
- **Date:** 2026-11-02
- **Author:** Claude Opus 5.5 (Agent), recording a design decision by Lucas Pan
- **Authority:** `SPEC.md` §2 (G2), §3 (no cloud service; no hard cross-machine enforcement), §8 (`collect` claim check; `ttl_secs × 4` abandoned sweeper), §9 (event log, holder-timestamp expiry + `skew_grace_secs`, `EXPIRE_SWEEP`, CAS push, enforcement model), §10 (singleton `daemon.lock`), §11, §12, §16 (`MT-011`, `MT-031`, `MT-032`); `AGENTS.md` Standing Order #1

## Context

The fleet so far has been physical machines (macOS, Windows) running local agents. Agents now also run as **cloud-hosted sessions**: for example, a Claude Code cloud session in an ephemeral sandbox that clones the repository from GitHub, works, pushes, and disappears. Such a session may run the `meshtree` binary (managed) or work raw on a branch with no MeshTree installed (unmanaged).

`SPEC.md` never says whether such a session is in scope, which layers apply to it, or what happens when its machine vanishes mid-task. Left unstated, agents may either build cloud-specific machinery (contradicting §3) or treat cloud sessions as unsupported (ignoring a real participant).

## Decision

1. **The protocol layer applies fully.** A cloud sandbox is a machine with an ephemeral disk and no local resources worth sharing. Orphan-ref leases, CAS sync (`--force-with-lease`), claims, handoff records and the merge gate (§8, §9, §13) work for it unchanged: it has a machine id, a clone, and push access to the same remote.
2. **The local-resource layer has nothing to do for it.** The shared-op FIFO queue (§12, local per machine by design), dependency sharing (§11 junctions / clonefile / symlinks) and the Windows teardown pipeline (§10 Trap 2) are per-machine mechanisms. A cloud session brings its own compute and a fresh clone, so there is no cross-agent contention for these layers to manage. Nothing is disabled; they simply have no work.
3. **Unmanaged cloud agents degrade to advisory participants.** An agent without MeshTree holds no claims and is invisible to the lease table. Protection then comes from the merge path:
   - commits that reach integration through a collected task branch (e.g. a cloud agent continuing a task branch from its handoff) are checked by the §8 step 2 claim check, so an overlap is reported as `MT-032`, not merged;
   - commits pushed directly to the base branch bypass the gate entirely; textual overlap with managed work then surfaces as an ordinary merge conflict (`MT-031`) when integration syncs, never as a silent overwrite. Semantic overlap without a textual conflict is not detected. This is the existing §3 / §9 honesty clause applied to a participant that opted out of claims.
4. **Ephemeral machines are the aggressive case of existing machinery.** A sandbox that vanishes mid-task leaves a stale lease, which is exactly what holder-timestamp TTL expiry with `skew_grace_secs` and the idempotent `EXPIRE_SWEEP` emitted by any surviving machine (§9) exist for. Heartbeats stop, the lease expires, and another machine's sweep reaps it. A sandbox that is restored with its disk is covered by `daemon.lock` stale detection (§10: a changed `boot_id` or dead pid ⇒ `MT-011` adoption). No new mechanism is introduced.

## Consequences

- `SPEC.md` gains one short scope paragraph in §9 "Enforcement model" stating the above with citations. No new types, events, error codes, config keys or CLI surface.
- Out of scope, unchanged: no cloud service, no accounts or hosted coordinator, no telemetry (§3). Git remains the only network; local-first is unchanged. MeshTree does not provision, detect or manage cloud sandboxes.
- Hard enforcement stays local-daemon-only (§9). Every remote participant, cloud or physical, is advisory with merge-gate protection.
- Known limits made explicit:
  - a vanished machine's leases are reaped by other machines, but its *tasks* are recorded only in its own lost `task.json`; the `ttl_secs × 4` abandoned sweeper (§8) runs per machine and cannot transition them. The coord-ref task registry (`T-043`) is where a cross-machine view of such tasks would live;
  - unmanaged direct pushes to the base branch are outside MeshTree's guarantees beyond git's own conflict detection.
- `T-036` (dogfood case study) must exercise this: a real cloud session in the fleet, a deliberately killed sandbox whose lease is reaped by the sweep (trace parsed against the `T-033` schema), and honest notes on the layers that did not apply.
