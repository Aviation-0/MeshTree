# MeshTree Benchmarks

Reproduce with `cargo run --release -p xtask -- bench` (`SPEC.md` §18); the run overwrites this file with a single-host report. Add `--smoke` for the CI sizes and `--remote <url>` to time scenario 3 against a real remote. Use a **throwaway** repository for `--remote`, because the bench pushes `main` and `refs/meshtree/coord` to it; the repository must be empty (no `main`) at the start of each run.

The run overwrites this file; the full-scale section below was assembled from two runs written elsewhere with `--out`.

## Current full-scale results (`T-056`, `ADR-0012`, 2026-11-04)

Same host as below (Apple M1, 8 GB, macOS 26.4.1, git 2.50.1, release build), scenario 3 against the private throwaway GitHub repository over HTTPS, emptied before each run. Since `ADR-0012` the CAS row measures MeshTree's **overhead over a plain-git CAS floor**: each of 30 pairs runs a plain `git fetch` + `--force-with-lease` push of a separate ref (`refs/meshtree-bench/floor`) from its own clone, and a `meshtree claim` on clone A, alternating which goes first; overhead = claim − floor per pair. Every claim runs under `GIT_TRACE` to count its fetches and pushes.

| Scenario | Target (§18) | Run 1 | Run 2 | Met |
|---|---|---|---|---|
| Spawn with shared `node_modules` (2048 MB) | < 5 s | 262 ms | 219 ms | yes / yes |
| Claim acquire (local), 100 / 2,000 / 10,000 lifetime events | p95 < 50 ms | 23.5 / 26.2 / 20.3 ms | 24.2 / 26.9 / 27.0 ms | yes / yes |
| Cross-machine claim CAS round: overhead over the git floor | p95 < 500 ms; ≤ 1 fetch + 1 push per claim | **p95 920 ms**; 1 fetch + 1 push | p95 312 ms, median 219 ms; 1 fetch + 1 push | **no** / yes |
| ↳ claim round vs git floor (absolute) | reported | claim p95 2807 ms vs floor p95 2154 ms | claim p95 1929 / median 1747 ms vs floor p95 1835 / median 1546 ms | — |
| ↳ B sees the lease (`sync` on clone B) | reported | p95 869 ms | p95 898 ms | — |
| Two agents, one repo: silent overwrites | 0 (baseline N > 0) | 0; baseline 19 of 200 | 0; baseline 86 of 200 | yes / yes |
| `index.lock` collisions | 0 with queue | 0; baseline 142 of 200 | 0; baseline 134 of 200 | yes / yes |
| Crash recovery (20 tasks) | < 2 s | 88 ms | 108 ms | yes / yes |

Run 2 met every row. Run 1 missed the overhead budget: its network was noisier (claim p95 2807 ms vs 1929 ms), and a p95 of per-pair differences carries the jitter of both sides. Run 1 predates the median columns, which run 2 added.

**Where the overhead comes from.** On a local `file://` remote (no network) the same row gives an overhead p95 of 184–413 ms (4 pairs, release build). `GIT_TRACE` of one `meshtree claim` shows about 18 git processes started on the client against the floor's 4: besides `fetch` and `push`, the coordination round runs `config` (remote lookup), `ls-tree`, `cat-file`, `hash-object`, one `mktree` per directory (4), `commit-tree`, `update-ref` and extra `rev-parse`s, at about 11 ms per process start on this Mac. That matches the ~220 ms median overhead, and it is where any further margin would come from (one batched `mktree`, fewer lookups). With a daemon running, agents do not wait on this round at all (`ADR-0009`).

## Full-scale results (`T-051`, 2026-11-04)

Host: Apple M1, 8 GB RAM, macOS 26.4.1, git 2.50.1, release build; scenario 3 against a private throwaway GitHub repository over HTTPS (`Aviation-0/meshtree-bench-scratch`). Two complete runs, the remote reset to empty in between (seed `0x5eedf00d3e577ee5`; parameters below).

| Scenario | Target (§18) | Run 1 | Run 2 | Met |
|---|---|---|---|---|
| Spawn with shared `node_modules` (2048 MB, 32,768 files) | < 5 s | 207 ms (max of 3) | 201 ms (max of 3) | yes |
| Claim acquire (local), 100 lifetime events | p95 < 50 ms | p95 20.4 ms | p95 34.0 ms | yes |
| Claim acquire (local), 2,000 lifetime events | p95 < 50 ms | p95 21.2 ms | p95 22.1 ms | yes |
| Claim acquire (local), 10,000 lifetime events | p95 < 50 ms | p95 20.3 ms | p95 21.9 ms | yes |
| Cross-machine claim CAS round (GitHub) | p95 < 2 s | **p95 6961 ms** | **p95 6952 ms** | **no** |
| Two agents, one repo: silent overwrites | 0 (baseline N > 0) | 0; baseline 19 of 200 | 0; baseline 20 of 200 | yes |
| `index.lock` collisions | 0 with queue | 0; baseline 147 of 200 | 0; baseline 143 of 200 | yes |
| Crash recovery (20 tasks) | < 2 s | 114 ms (WAL 91 ms) | 200 ms (WAL 169 ms) | yes |

Parameters: 3 spawns; 200 local claims per history size; 30 CAS rounds; session 20 rounds × 5 edits × 2 agents over 6 shared files (MeshTree reported 80 claim conflicts, none silent); 8 index writers × 25 `git add -A`; 5 recoveries. Total run time 284.5 s and 279.7 s.

Five of six rows meet their targets at full scale, and the silent-overwrite baseline is now N > 0 (19 and 20 lost updates of 200), which the smoke runs could not show reliably.

### The CAS round miss

**Update (`T-055`, 2026-11-04):** coordination fetches now take one network round trip instead of two (glob refspec + `--prune` in place of `ls-remote` + `fetch`). A third full run on the same host gave a **CAS round p95 of 4898 ms** (from 6961 / 6952 ms); every other row stayed met (spawn 222 ms; local claim p95 20.1–21.1 ms; 0 silent overwrites vs baseline 34 of 200; 0 `index.lock` collisions vs 140 of 200; recovery 91 ms). Measured by hand, 20 rounds each, same link:

| Measurement | median | p95 |
|---|---|---|
| Plain git: `fetch` of the coordination ref | 0.56 s | 0.72 s |
| Plain git: `--force-with-lease` push | 1.53 s | 2.16 s |
| **Plain git: fetch + push (the CAS floor)** | **2.05 s** | **2.70 s** |
| `meshtree claim` on A (fetch, replay, push) | 1.99 s | 3.00 s |
| `meshtree sync` on B | 0.80 s | 1.86 s |
| Bench round (A + B) | 2.82 s | 4.76 s |

The git CAS alone exceeds 2 s at p95 on this link, so the §18 row cannot be met here by any implementation; a MeshTree claim now sits at the git floor (one fetch, one push). `ADR-0012` (Proposed) redefines the row as MeshTree's overhead over the plain-git floor (p95 < 500 ms), with absolute times reported alongside.

The analysis below is from `T-051`, before that change.

A bench round is `meshtree claim` on clone A (no daemon) followed by `meshtree sync` on clone B, which must then see the lease. Measured by hand on the same host, 10 rounds: claim on A 1.9–2.3 s, sync on B 1.0–3.2 s, round median about 3.5 s with tails to 5–7 s; plain `git fetch` of the coordination ref from GitHub took 0.4–0.7 s and a push about 1.2 s. `GIT_TRACE` shows where the time goes: every sync round makes **two** network calls before any work, `git ls-remote` (to tell an absent remote ref from a failed fetch) and then `git fetch`, and the claim adds the `--force-with-lease` push. One bench round is therefore five GitHub round trips (A: ls-remote, fetch, push; B: ls-remote, fetch).

Follow-up `T-055`: fetch the ref in one call (a glob refspec with `--prune` reports an absent ref as nothing fetched, without parsing git's localized error text), then re-measure. That removes two of the five round trips. On this link the remaining fetch + push + fetch still sits near 2 s, so `T-055` also has to settle whether §18's row means A's CAS round alone (fetch + push) or A plus B's visibility, as `T-034` defined it, before tuning further.

With a daemon running, agents never wait on this: claims are answered from memory (local p95 above) and committed and pushed in the background (`ADR-0009`).

## Smoke and history (earlier runs)

The tables below are the **smoke-scale** results from the `xtask_bench_smoke_verifies_six_scenarios` test on the GitHub Actions 3-OS matrix (CI run 41557533637, debug build, shared runners), and the findings that led to `ADR-0009` and `ADR-0010`.

- Smoke parameters:
  - spawn: 4 MB `node_modules` (64 files), 2 spawns;
  - local claim: 20 claims;
  - CAS: 4 rounds against a local bare `file://` remote;
  - session: 2 rounds × 3 edits × 2 agents;
  - index writers: 4 workers × 6 writes;
  - recovery: 2 restarts with 3 tasks.

The smoke test asserts the correctness rows on every push (0 silent overwrites with MeshTree; 0 `index.lock` collisions through the queue; `node_modules` linked, not copied). Latency rows are recorded, not asserted.

## Smoke results (CI run 41557533637)

| Scenario | Target (§18) | Linux x86_64 | macOS aarch64 | Windows x86_64 |
|---|---|---|---|---|
| Spawn with shared `node_modules` | < 5 s | 23 ms | 148 ms | 386 ms |
| Claim acquire (local) | p95 < 50 ms | **p95 99 ms** | **p95 856 ms** | **p95 1804 ms** |
| Cross-machine claim CAS round | p95 < 2 s | p95 570 ms | p95 737 ms | p95 1671 ms |
| Two agents, one repo: silent overwrites | 0 (baseline N > 0) | 0; baseline 4 of 12 | 0; baseline 0 of 12 | 0; baseline 0 of 12 |
| `index.lock` collisions | 0 with queue | 0; baseline 11 of 24 | 0; baseline 9 of 24 | 0; baseline 16 of 24 |
| Crash recovery | < 2 s | 5 ms (WAL 2 ms) | 23 ms (WAL 12 ms) | 51 ms (WAL 37 ms) |

Bold cells miss the §18 target.

## Findings

- **After `T-050` (ADR-0010 compaction; CI run 41742438911):** the bench now seeds finished history (acquire/release pairs older than the compaction horizon), which the daemon folds into a snapshot. Local claims stay flat across history size; all six rows met on all three OSes:

  | Local claim p95 | Linux | macOS | Windows |
  |---|---|---|---|
  | 100 lifetime events | 1.0 ms | 9.3 ms | 8.0 ms |
  | 2,000 lifetime events | 1.2 ms | 10.7 ms | 5.9 ms |
  | 10,000 lifetime events | 1.1 ms | 10.3 ms | 5.5 ms |

  The earlier rows below were measured with 2,000 *live* claims timestamped "now", which no compaction can fold; that shape is not what a long-lived repository accumulates. `T-051` re-measured everything at full scale on the Mac (above).

- **Local claim misses `p95 < 50 ms` on every OS, and the cost grows with history.**
  - Each `claim_paths` runs a synchronous coordination-ref sync round. That round re-reads every `log/*.json` blob with one `git cat-file` process each and re-hashes every file of the tree with one `git hash-object` each: about 2N + L + 8 subprocesses for N logged events and L leases.
  - The log is append-only, so claims slow down as a repository is used. Process creation is expensive on macOS and Windows, which is why those numbers are so much higher.
  - `ADR-0009` (Accepted) addresses it in two parts: (1) make sync rounds independent of history (event cache, batched reads and writes, tree reuse); (2) have the daemon answer from its in-memory lease table after a WAL append and commit/push in the background within 250 ms.
- **After `T-048` (ADR-0009 Part 1; CI run 41561529557):** a claim round now runs a fixed ~10 git processes whatever the log length, and small-log claims got 2–4× faster. Claim **time** still grows with the log, though, because every round still moves and processes the whole log in-process: one batched read of every blob, decoding, replay, re-encoding, and a full `mktree` of every directory.

  | Local claim p95 | Linux | macOS | Windows |
  |---|---|---|---|
  | `T-034` (~80 logged events, before) | 99 ms | 856 ms | 1804 ms |
  | `T-048`, 100 logged events | 62 ms | 188 ms | 510 ms |
  | `T-048`, 2,000 logged events | 877 ms | 1247 ms | 2336 ms |

  `T-049` (Part 2) takes this work off the claim path: the daemon answers from memory, and the ref write happens in the background. The background write is still O(log) per flush. Bounding it (log compaction, or sharding `log/` into subdirectories so a round rewrites only a small subtree) would change the §5 layout and needs its own ADR.
- **The cross-machine CAS round meets the target here, but only against a local `file://` remote.** Most of each round is the same subprocess cost as above. A real GitHub remote adds network RTT on top, so the full-scale run with `--remote` decides this row.
- **The silent-overwrite baseline depends on scale.** In the baseline, two agents read, modify and write one shared checkout. With only 12 edits per smoke session, lost updates showed up on one runner (4 of 12) and not on the others. The §18 "baseline N > 0" needs the full session (20 rounds × 5 edits × 2 agents). MeshTree recorded 0 silent overwrites on every OS: every refused claim or merge was reported to the agent.
- **`index.lock`.** Unqueued concurrent `git add -A` in one worktree collided in 38–67 % of writes. Through a concurrency-1 queue, none collided.
- **Spawn and recovery** are far inside their budgets at smoke scale. Spawn time does not depend on `node_modules` size, because the directory is linked. The full run measures it with 2 GB anyway.

## Method

| # | Scenario | Measured |
|---|---|---|
| 1 | Spawn | Wall time of `meshtree spawn` (process start included) on a repository whose primary `node_modules` (gitignored) holds generated 64 KB files; checks the task's `node_modules` is a link. |
| 2 | Local claim | Round-trip time of `claim_paths` over the daemon's IPC endpoint, one new path pattern per claim (each released afterwards, untimed). |
| 3 | CAS round | `meshtree claim` on clone A (fetch, replay, `--force-with-lease` push), then `meshtree sync` on clone B, which must report the new lease count. |
| 4 | Two agents | Two concurrent agents follow seeded edit plans over 6 shared files. With MeshTree, each agent works as follows: spawn a task per round (based on the integration branch), claim before editing (on `MT-001`, try another file), commit, collect. An edit is a *silent overwrite* if its task merged and its line is missing from the integration branch. In the baseline, both agents read, think 0–3 ms, then write whole files in one shared checkout, and an edit whose line is missing at the end is a silent overwrite. |
| 5 | `index.lock` | N workers each write a new file and run `git add -A` in one task worktree, either directly or via `meshtree queue index request` (concurrency 1). A run whose output mentions `index.lock` is a collision. |
| 6 | Crash recovery | Start the daemon, journal claims for every task, kill it without warning (`SIGKILL` / `TerminateProcess`), restart it, and time until its first answered `ping`. The daemon's own WAL recovery time is reported alongside. Mid-merge kills are covered by the chaos test `chaos_sigkill_daemon_mid_merge_recovers_under_2s`. |
