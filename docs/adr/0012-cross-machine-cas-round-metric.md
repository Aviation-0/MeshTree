# ADR-0012: Measuring the Cross-Machine Claim CAS Round Against the Git Floor

- **Status:** Proposed
- **Date:** 2026-11-04
- **Author:** Claude Opus 5.5 (Agent)
- **Authority:** `SPEC.md` §9 (atomic update via CAS: fetch, then `--force-with-lease` push; bounded ×5 retry), §18 (benchmark row "Cross-machine claim CAS round: p95 < 2 s (GitHub remote RTT)"); `ADR-0001` (git via the CLI); `ADR-0009`; `AGENTS.md` Standing Order #1

## Context

`T-051` ran the §18 suite at full scale on the Mac (Apple M1, macOS 26.4.1, git 2.50.1) against a private GitHub repository over HTTPS. Five rows met their targets; the CAS round did not: p95 6961 / 6952 ms in two runs. The bench (`T-034`) defines a round as `meshtree claim` on clone A **plus** `meshtree sync` on clone B until B sees the lease.

`T-055` removed the `ls-remote` probe from every coordination fetch (two of the five GitHub round trips per bench round), which brought the bench round to **p95 4898 ms**. It then measured on the same host and link, 20 rounds each:

| Measurement | median | p95 |
|---|---|---|
| Plain git: `fetch` of the coordination ref | 0.56 s | 0.72 s |
| Plain git: `--force-with-lease` push | 1.53 s | 2.16 s |
| **Plain git: fetch + push (the CAS floor)** | **2.05 s** | **2.70 s** |
| MeshTree: `meshtree claim` on A (fetch, replay, push) | 1.99 s | 3.00 s |
| MeshTree: `meshtree sync` on B (fetch, replay) | 0.80 s | 1.86 s |
| MeshTree: bench round (A + B) | 2.82 s | 4.76 s |

The git CAS itself, with no MeshTree code in the loop, has p95 2.70 s here. A pushes over HTTPS to GitHub cost 1.2–2.2 s from this link regardless of payload. So "p95 < 2 s" cannot be met by any implementation on this connection, and whether it can be met elsewhere depends on the network, not on MeshTree. The row as written measures the user's distance to GitHub.

What MeshTree controls is the work it adds around git's fetch and push: decoding, replay, tree building, and how many round trips it makes. Before `T-055` that overhead included two extra round trips; now a claim is one fetch and one push, the protocol's minimum (§9).

Also relevant: with a daemon running, agents never wait on this round. Claims are answered from memory (p95 20–34 ms at full scale) and committed in the background (`ADR-0009`). The CAS round is what bounds *cross-machine visibility*, not agent latency.

## Options

1. **Keep "p95 < 2 s" as an absolute target.** Unmeetable from this host; results would depend on whoever runs the bench. Rejected.
2. **Measure only A's CAS round, absolute.** Still bounded below by the git floor (2.70 s here). Rejected for the same reason.
3. **Measure MeshTree's overhead over the git floor, and report absolute times alongside (proposed).** The bench measures, interleaved in the same run against the same remote, the plain-git fetch + push floor and MeshTree's claim round, and holds MeshTree to a fixed overhead budget. Absolute numbers stay in the report so the network cost remains visible.

## Decision (proposed)

Replace the §18 row with:

| Scenario | Metric |
|---|---|
| Cross-machine claim CAS round | **overhead over the plain-git CAS floor: p95 < 500 ms**, and at most one fetch and one push per claim; absolute p95 of the claim round and of B's visibility (`sync` until B sees the lease) reported alongside the floor |

- **Floor:** plain `git fetch` of `refs/meshtree/coord` plus a `--force-with-lease` push of a new commit, measured by the bench in alternation with the MeshTree rounds (same remote, same minutes), so network drift affects both equally.
- **Overhead:** per-round `claim time − paired floor time`, p95 over the rounds. 500 ms covers the claim process start, replay and tree work for a realistic log, with margin; at full scale today it is about +0.3 s at p95 (3.00 s vs 2.70 s, measured in separate runs).
- **Round trips:** asserted from `GIT_TRACE` in a test (`T-055` already does this for `fetch_ref`), so a regression to extra network calls fails CI even where latency noise would hide it.
- **The "GitHub remote RTT" note** in §18 becomes "measured against a GitHub remote; absolute times depend on the link".

## SPEC amendments on acceptance

- §18 benchmark table: the row above replaces "Cross-machine claim CAS round | p95 < 2 s (GitHub remote RTT)".

## Consequences

- The row becomes reproducible on any host: it measures MeshTree, not the network.
- A follow-up task implements it in `xtask bench` (interleaved floor rounds, overhead p95, B visibility as its own number) and records a full-scale run; until then `docs/benchmarks.md` reports this ADR's hand measurements.
- Users on slow links still see slow cross-machine visibility. The report keeps the absolute numbers so that is never hidden, and the daemon path keeps agents from waiting on it.
