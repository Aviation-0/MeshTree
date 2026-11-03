# MeshTree Benchmarks

Reproduce with `cargo run --release -p xtask -- bench` (`SPEC.md` §18); the run overwrites this file with a single-host report. Add `--smoke` for the CI sizes and `--remote <url>` to time scenario 3 against a real remote. Use a **throwaway** repository for `--remote`, because the bench pushes `main` and `refs/meshtree/coord` to it.

## Status

- **Full-scale results (2 GB `node_modules`, 20-round session, GitHub remote): not yet recorded.** The Windows development host cannot link Rust binaries, so the full run is pending on a host that can (the Mac).
- The tables below are the **smoke-scale** results from the `xtask_bench_smoke_verifies_six_scenarios` test on the GitHub Actions 3-OS matrix (CI run 41557533637, debug build, shared runners).
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

- **Local claim misses `p95 < 50 ms` on every OS.**
  - Each `claim_paths` writes a coordination-ref commit synchronously. One claim runs about a dozen `git` subprocesses (`hash-object`, `mktree`, `commit-tree`, `update-ref`, …) before the daemon answers.
  - That cost shows up as about 100 ms on Linux, and as much more on macOS and Windows, where process creation is expensive.
  - Meeting the target needs a change in the claim path, such as answering from the daemon's in-memory lease table and writing the ref asynchronously, or doing the git writes in-process. That is a design change to raise with the project owner, not a benchmark fix.
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
