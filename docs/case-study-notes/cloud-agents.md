# Case-study notes: cloud/ephemeral agents as fleet members

- **Recorded:** 2026-11-02 (design decision by Lucas Pan; written up by Claude Opus 5.5)
- **Decision record:** [`ADR-0008`](../adr/0008-ephemeral-cloud-agents-as-fleet-participants.md); `SPEC.md` §9 "Enforcement model"
- **Consumer:** `T-036` (`docs/case-study-dogfood.md`, weeks 7–8), section "Cloud/ephemeral agents as fleet members"

Source material, written while the rationale is fresh. The case study itself will replace these claims with real traces.

## The four decision points

1. **Protocol layer applies fully.** A cloud sandbox (e.g. a Claude Code cloud session that clones the repo from GitHub) is a machine with an ephemeral disk and no local resources worth sharing. It has a machine id, a clone and push access to the same remote, so orphan-ref leases (`refs/meshtree/coord`), CAS sync (`--force-with-lease`), claims, handoff records and the merge gate work for it unchanged.
   *Why:* the protocol was built on git alone (§2 G2, §9), so anything that can fetch and push the remote can take part. No cloud-specific path is needed.

2. **Local-resource layer has nothing to do.** The shared-op FIFO queue (§12, local per machine), dependency sharing (§11 junctions / clonefile / symlinks) and Windows teardown (§10 Trap 2) are per-machine. A cloud session brings its own compute and a fresh clone: no contention for builds, no `node_modules` to share, no NTFS locks.
   *Why:* these layers solve contention among agents on one host; a one-agent sandbox has none. Nothing is disabled.

3. **Unmanaged cloud agents degrade gracefully.** An agent working raw on a branch without MeshTree holds no claims and is invisible to the lease table. It is an advisory participant:
   - commits entering integration through a collected task branch (e.g. continuing a task branch from its `HANDOFF.md`) are claim-checked at `collect` (§8 step 2), so overlap becomes a reported `MT-032`, never a merge;
   - commits pushed straight to the base branch bypass the gate; textual overlap surfaces as a merge conflict (`MT-031`) when integration syncs. Semantic overlap with no textual conflict is not detected — the §3 / §9 honesty clause, applied to a participant that opted out of claims.
   *Why:* MeshTree's promise is "no *silent* overwrites", not "hard cross-machine locks" (§3).

4. **Ephemeral machines are the aggressive case of existing machinery.** A sandbox that vanishes mid-task leaves a stale lease: heartbeats stop, the lease expires on the holder's timestamps plus `skew_grace_secs`, and any surviving machine emits an idempotent `EXPIRE_SWEEP` (§9). A sandbox restored with its disk is handled by `daemon.lock` stale detection (§10: changed `boot_id` / dead pid ⇒ `MT-011` adoption).
   *Why:* TTL, heartbeat, sweep and stale-lock detection were designed for machines that disappear; a deleted VM is the most extreme form.

## Limits to report honestly

- Leases of a vanished machine are reaped by others; its **tasks** are not — they live only in its lost `task.json`, and the `ttl_secs × 4` abandoned sweeper (§8) is per machine. A cross-machine task view belongs to the coord-ref task registry (`T-043`).
- Unmanaged direct pushes to the base branch get only git's conflict detection.
- No cloud service, accounts or telemetry are involved (§3); MeshTree neither provisions nor detects sandboxes.

## What T-036 must capture

- (a) A real trace of a cloud session in the fleet (managed or unmanaged).
- (b) A deliberately killed sandbox whose lease an `EXPIRE_SWEEP` reaped; the trace must parse against the `T-033` schema.
- (c) Which layers did not apply and why (points 2 and 3 above, confirmed or corrected by observation).
