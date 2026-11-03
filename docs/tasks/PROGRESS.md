# MeshTree Progress Dashboard

Updated at the start and end of every work session per `AGENTS.md`. Authoritative task definitions live in [`docs/tasks/backlog.md`](backlog.md).

## Current Focus

- **Active Milestone:** `M4 — Chaos Suite, Coverage Gate & Technical Docs` (M2 done; M3 awaits human `T-045`)
- **In-Progress Task (`doing`):** `T-049` — daemon in-memory claims, WAL events, background commit (ADR-0009 Part 2) (Claude Opus 5.5, Windows)
- **Next Up (`todo`, dependencies satisfied):**
  1. `T-031` — §17 security suite + CI-enforced `meshtree-core` coverage ≥ 85%
  2. `T-033` — `docs/protocol.md` + `docs/architecture.md`
  3. `T-052` — `ADR-0011` config keys, OS machine-id derivation & `.venv` policy
  4. `T-050` — Coordination-log compaction (`ADR-0010`; after `T-049`)
  5. `T-045` — MVP acceptance: two real agents run the §18 scenario (**human-in-the-loop**; now gates `T-037`)
- **Latest (2026-11-03, Claude Opus 5.5, Mac):** `T-030` done (39/52) — `sweep_candidates` + chaos tests for ±30 min skew (no premature sweep, audited idempotent sweeps) and four-clone CAS contention (bounded convergence, identical tables); CI run 41648065503 green on all 3 OSes. Gap: nothing emits `EXPIRE_SWEEP` in production yet (follow-up proposed).
- **2026-11-03 alignment audit (Claude Opus 5.5, Mac; approved by Lucas Pan):** `ADR-0010` (log compaction) and `ADR-0011` (config keys, OS machine-id, `.venv`) **Accepted**; `SPEC.md` §5/§6/§9/§11/§12/§13/§18 amended. Added `T-050` (compaction), `T-051` (full-scale benchmark, run on the Mac), `T-052` (`ADR-0011`). `T-030` no longer depends on `T-045`; `T-037` now depends on `T-045` and `T-051`; `T-031` gains the §17 IPC tests; `T-033` covers `ADR-0008`/`0009`/`0010`. Totals 38/52.
- **Latest (2026-11-02, Claude Opus 5.5, Windows):** `T-048` done (38/49) — coordination sync runs a constant ~10 git processes regardless of log length (batched reads/writes, blob reuse); small-log claims 2–4× faster, but claim time still grows with the log (in-process O(log) work), which `T-049` moves off the claim path; CI run 41561529557 green on all 3 OSes.
- **Earlier (2026-11-02, Claude Opus 5.5, Windows):** `T-047` done (37/49) — every daemon-driven status change (collect edges, recovery, `ttl × 4` sweeper) now reaches the coordination-ref task registry; recovery publishes wait until the daemon serves; CI run 41559983307 green on all 3 OSes.
- **Earlier (2026-11-02, Claude Opus 5.5, Windows):** `T-034` done (36/47) — `xtask bench` for all six §18 scenarios with baselines; `docs/benchmarks.md` holds 3-OS smoke results (CI run 41557533637); local claim p95 misses the 50 ms target on every OS (synchronous git subprocesses per claim) — needs a design decision; full-scale run pending on the Mac.
- **Earlier (2026-11-02, Claude Opus 5.5, Windows):** `T-029` done (35/46) — `full_lifecycle_spawn_claim_handoff_collect_merged` and `two_agents_mcp_and_cli_parallel_session_zero_overwrites` green on all 3 OSes (CI run 41552296766). This session also closed `T-032`, `T-039`, `T-040`, `T-043`; M2 is complete. MVP gate still needs the human-run `T-045`.
- **Earlier (2026-11-02, Claude Opus 5.5, Windows):** `T-043` done (34/46) — cross-machine task registry `tasks/<id>.json` in `refs/meshtree/coord` (revision-wins merge across tips); `spawn`, failed spawns and `abort` publish through the CAS sync; merge/sweeper publishing still open; CI run 41551357771 green on all 3 OSes.
- **Earlier (2026-11-02, Claude Opus 5.5, Windows):** `T-040` done (33/46) — a granted queue slot lapses `lease.ttl_secs` after its grant even with a live holder, and the next FIFO ticket is granted at that instant (injected-clock test + daemon test); CI run 41550700924 green on all 3 OSes.
- **Earlier (2026-11-02, Claude Opus 5.5, Windows):** `T-039` done (32/46) — `collect` publishes the integration branch to `origin` with a `--force-with-lease` push; a lost race refetches, replay-merges and retries (bounded ×5, then `MT-031`); network failures defer the push; two-clone race test; CI run 41549927497 green on all 3 OSes.
- **Earlier (2026-11-02, Claude Opus 5.5, Windows):** `T-032` done (31/46) — junction/symlink/clonefile realpath probe matrix (`share_junction_realpath_probe_matrix`) and `docs/compatibility.md` with per-tool `strategy = "none"` recipes; Windows resolves `<junction>/..` lexically, POSIX physically; CI run 41548903359 green on all 3 OSes.
- **Earlier (2026-11-02, Claude Opus 5.5, Windows):** `T-028` done (30/46) — `meshtree mcp` stateless stdio MCP proxy for all 11 §15 tools (`[MT-xxx]` tool errors, `MT-010` without a daemon, clean exit on client EOF leaving the daemon running); daemon gained `spawn_task` / `abort_task` / `list_tasks` / `commit_handoff` / `render_agents_block`, `claim_paths` takes `ttl_secs`; CI run 41540486618 green on all 3 OSes.
- **Earlier (2026-11-02, Claude Opus 5.5, Mac):** `T-027` done (29/46) — `meshtree handoff` (`--json-file` / `--set`) commits `handoff.json` + `HANDOFF.md` on the task branch via plumbing + worktree fast-forward (agent's uncommitted work kept), and `meshtree render-agents` regenerates the root `AGENTS.md` block; CI run 41509759232 green on all 3 OSes.
- **Earlier (2026-10-31, Claude Opus 5.5, Windows):** `T-026` done (28/46) — pure handoff model + `HANDOFF.md` / `AGENTS.md` block renderers and splice (byte-stable snapshots, marker-forgery defused); CI run 41231429375 green on all 3 OSes.
- **Earlier (2026-10-31, Claude Opus 5.5, Windows):** `T-046` done (27/46) — Unix daemon socket falls back to `/tmp/meshtree-<uid>/<repo-hash-16>.sock` (owner/mode/symlink-checked `0700` directory) when `.meshtree/daemon.sock` exceeds the socket path limit (`ADR-0007`); CI run 41230869170 green on all 3 OSes.
- **ADR review (2026-10-31, Lucas Pan):** `ADR-0003`, `ADR-0005`, `ADR-0006` **Accepted**; `SPEC.md` §10 step 3 amended. `ADR-0007` (Unix socket path-length fallback) **Accepted** later the same day; `SPEC.md` §5/§10 amended and `T-046` added (dep of `T-029`).
- **Latest (2026-10-31, Claude Opus 5.5, Windows):** `T-022`–`T-025` done (26/45). `T-022` share links: junction / clonefile / symlink, `MT-040` fallback (CI 41227678198); `T-023` `[[share]]` in `spawn`, links excluded from git, all deletions unlink shares first (CI 41227946875); `T-024` teardown backoff → quarantine (`MT-021`) → deferral (`MT-020`), quarantine GC, `meshtree prune` (CI 41228256318); `T-025` Windows exclusive-handle test — merge and abort never fail (`ADR-0006`: Windows cannot move a directory holding an exclusively opened file, so that case defers instead of quarantining) (CI 41228444158). Local clippy on this Windows host now covers the Linux and macOS targets.
- **Earlier (2026-10-31, Claude Opus 5.5, Windows):** `T-021` done (22/45) — startup WAL recovery (reconcile WAL ↔ `task.json`, clear orphaned git locks, reset integration worktree, complete landed merges / reactivate interrupted collects) + named chaos test `chaos_sigkill_daemon_mid_merge_recovers_under_2s`; CI run 41226059939 green on all 3 OSes. M2's daemon/merge-gate/chaos exit tests are all green; M2 still has `T-039`, `T-040`, `T-043`.
- **Last Session Summary (2026-10-31, Claude Opus 5.5, Windows):** `T-017`–`T-020` + `T-044` done (21/45). `T-017` WAL (CI 41222685141); `T-044` `LC_ALL=C` git + retry-exhaustion tests (CI 41222922141); `T-018` `meshtree daemon` with serialized IPC claims, heartbeats, `ttl × 4` sweeper (CI 41223643960, after a closed-stdout panic fix); `T-019` shared-op FIFO queue + `meshtree queue` (`ADR-0005` **Proposed**, CI 41223916867); `T-020` merge gate `collect`/`promote` (CI 41224611824). Order note: `T-044` was taken before the lower-numbered eligible `T-022` because the user directed work through `T-020` and `T-044` gates `T-018`. Windows host can now run `cargo clippy` locally via the GNU toolchain (tests still CI-only).
- **Earlier Session Summary (2026-10-31, Claude Opus 5.5, Mac):** `T-016` done — singleton `daemon.lock` (kernel guard + pid/boot_id/machine_id record, `MT-011` stale adoption; named chaos test `chaos_stale_daemon_lock_live_vs_dead_pid_adoption`), CI run 41215529894 green on all 3 OSes.
- **2026-10-31 audit (Claude Opus 5.5, approved by Lucas Pan):** Mac synced; integrity audit of `T-003`–`T-015` found no regressions, six debts. Queue restructured: `T-043` (§8 coord registry), `T-044` (locale-safe push-race classification + retry-exhaustion tests), `T-045` (human-in-the-loop §20 Week-4 two-agent run, `ADR-0004`); `T-031` coverage now CI-enforced; `T-024` gains idle quarantine retry; CI adds `clippy --all-targets` + `cargo build`; `ADR-0003` set to Proposed pending human acceptance.
- **Earlier Session Summary (2026-10-30, Claude Opus 5.5):** `T-015` done — NDJSON IPC over UDS / named pipes with §17 access control (CI run 41155843711). Session completed `T-003`–`T-015`. Earlier: `T-014` done — CLI `claim`/`release`/`sync` + `spawn --claim`; **M1 complete** (CI run 41155551527). Earlier: `T-013` done — coordination store sync + two-clone CAS race (CI run 41155358409, after a ref-lock rejection fix). Earlier: `T-012` done — TTL skew test + coord snapshot codec (CI run 41154798925). Earlier: `T-011` done — remote CAS push/fetch plumbing (CI run 41154570699). Earlier: `T-010` done — Week-1 walking skeleton test + README quickstart; **M0 complete** (CI run 41154334602). Earlier: `T-009` done — claim pattern normalization + exact zero-false-negative conflict rule (`ADR-0003`, CI run 41154122988). Earlier today: `T-008` (replay determinism proptest), `T-007` (lease events + replay), `T-006` (`status`/`abort`, template machine-id fix, backlog audit → 42 tasks), `T-005` (`spawn`), `T-004` (`init`), `T-003` (task model, `ADR-0002`). Per-task details and CI runs are in the `backlog.md` handoff log.

---

## Milestone Scorecard

| Milestone | Window | Tasks | Total | `done` | `doing` | `blocked` | `todo` | Status |
|---|---|---|---|---|---|---|---|---|
| **M0 — Walking Skeleton & Core Lease Foundations** | Week 1 (`→ 2026-11-08`) | `T-001`–`T-010` | 10 | 10 | 0 | 0 | 0 | `done` |
| **M1 — Cross-Machine Lease Protocol & CAS Sync** | Week 2 (`→ 2026-11-15`) | `T-011`–`T-014` | 4 | 4 | 0 | 0 | 0 | `done` |
| **M2 — Daemon, IPC, WAL, Shared Queue & Merge Gate** | Week 3 (`→ 2026-11-22`) | `T-015`–`T-021`, `T-039`–`T-040`, `T-043`–`T-044`, `T-046` | 12 | 12 | 0 | 0 | 0 | `done` |
| **M3 — Sharing, Windows Teardown, Handoffs, MCP & MVP** | Week 4 (`→ 2026-11-28`) | `T-022`–`T-029`, `T-045` | 9 | 8 | 0 | 0 | 1 | `doing` |
| **M4 — Chaos Suite, Coverage Gate & Technical Docs** | Weeks 5–6 (`→ 2026-12-12`) | `T-030`–`T-033`, `T-047`–`T-050`, `T-052` | 9 | 4 | 1 | 0 | 4 | `doing` |
| **M5 — Benchmark Suite, Packaging & Dogfood Study** | Weeks 7–8 (`→ 2026-12-26`) | `T-034`–`T-036`, `T-041`–`T-042`, `T-051` | 6 | 1 | 0 | 0 | 5 | `doing` |
| **M6 — Beta Triage & v1.0 Launch** | Weeks 9–10 (`→ 2027-01-08`) | `T-037`–`T-038` | 2 | 0 | 0 | 0 | 2 | `todo` |
| **Total** | **Weeks 1–10** | **`T-001`–`T-052`** | **52** | **39** | **1** | **0** | **12** | **75% (`39 / 52`)** |

---

## Quality & Launch Gates (`SPEC.md` §18 & §22)

- [x] **M0 Walking Skeleton:** `walking_skeleton_init_spawn_status_abort` green on 3-OS CI (`T-010`, run 41154334602)
- [x] **Cross-Machine CAS Sync:** `two_clones_file_remote_cas_race_converges` green (`T-013`, run 41155358409)
- [x] **Singleton daemon lock:** `chaos_stale_daemon_lock_live_vs_dead_pid_adoption` green on 3-OS CI (`T-016`, run 41215529894)
- [x] **Daemon & Crash Safety:** `chaos_sigkill_daemon_mid_merge_recovers_under_2s` green (`< 2 s`, 0 orphaned `index.lock`) (`T-021`, run 41226059939)
- [x] **Windows NTFS Teardown:** `windows_locked_handle_quarantines_never_fails_merge` green on Windows CI (`T-025`, run 41228444158; `ADR-0006`)
- [ ] **4-Week MVP Exit** (automated half green: `T-029`, CI run 41552296766; awaiting human `T-045`): `full_lifecycle_spawn_claim_handoff_collect_merged` + `two_agents_mcp_and_cli_parallel_session_zero_overwrites` (`T-029`) + real two-agent run (`T-045`, human-in-the-loop)
- [ ] **Core Coverage Bar:** CI-enforced `meshtree-core` coverage `≥ 85%` (`T-031`)
- [ ] **Benchmark Suite** (xtask + smoke results landed in `T-034`; full-scale run open): All 6 `SPEC.md` §18 metrics reproduced via `xtask bench` at full scale (`T-034`, `T-051`)
- [ ] **Claim latency independent of history:** local claim p95 < 50 ms at 2,000 (`T-049`) and 10,000 lifetime events (`T-050`, `ADR-0010`)
- [ ] **v1.0 Packaging:** 3-OS static binaries, Homebrew tap, Scoop/winget, and `npx meshtree-mcp` verified (`T-035`, `T-037`, `T-038`)
