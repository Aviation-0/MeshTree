# MeshTree Progress Dashboard

Updated at the start and end of every work session per `AGENTS.md`. Authoritative task definitions live in [`docs/tasks/backlog.md`](backlog.md).

## Current Focus

- **Active Milestone:** `M2 — Daemon, IPC, WAL, Shared Queue & Merge Gate` (Week 3 · Target: `2026-11-22`)
- **In-Progress Task (`doing`):** None
- **Next Up (`todo`, dependencies satisfied):**
  1. `T-022` — Dependency sharing (`crates/meshtree-platform`)
  2. `T-024` — Windows teardown & quarantine (`crates/meshtree-platform`)
  3. `T-026` — Handoff record model & `AGENTS.md` snapshots (`crates/meshtree-core`)
  4. `T-039` integration-branch remote sync · `T-040` queue ticket TTL · `T-043` coord-ref task registry (remaining M2)
- **Latest (2026-10-31, Claude Opus 5.5, Windows):** `T-021` done (22/45) — startup WAL recovery (reconcile WAL ↔ `task.json`, clear orphaned git locks, reset integration worktree, complete landed merges / reactivate interrupted collects) + named chaos test `chaos_sigkill_daemon_mid_merge_recovers_under_2s`; CI run 41226059939 green on all 3 OSes. M2's daemon/merge-gate/chaos exit tests are all green; M2 still has `T-039`, `T-040`, `T-043`.
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
| **M2 — Daemon, IPC, WAL, Shared Queue & Merge Gate** | Week 3 (`→ 2026-11-22`) | `T-015`–`T-021`, `T-039`–`T-040`, `T-043`–`T-044` | 11 | 8 | 0 | 0 | 3 | `doing` |
| **M3 — Sharing, Windows Teardown, Handoffs, MCP & MVP** | Week 4 (`→ 2026-11-28`) | `T-022`–`T-029`, `T-045` | 9 | 0 | 0 | 0 | 9 | `todo` |
| **M4 — Chaos Suite, Coverage Gate & Technical Docs** | Weeks 5–6 (`→ 2026-12-12`) | `T-030`–`T-033` | 4 | 0 | 0 | 0 | 4 | `todo` |
| **M5 — Benchmark Suite, Packaging & Dogfood Study** | Weeks 7–8 (`→ 2026-12-26`) | `T-034`–`T-036`, `T-041`–`T-042` | 5 | 0 | 0 | 0 | 5 | `todo` |
| **M6 — Beta Triage & v1.0 Launch** | Weeks 9–10 (`→ 2027-01-08`) | `T-037`–`T-038` | 2 | 0 | 0 | 0 | 2 | `todo` |
| **Total** | **Weeks 1–10** | **`T-001`–`T-045`** | **45** | **22** | **0** | **0** | **23** | **49% (`22 / 45`)** |

---

## Quality & Launch Gates (`SPEC.md` §18 & §22)

- [x] **M0 Walking Skeleton:** `walking_skeleton_init_spawn_status_abort` green on 3-OS CI (`T-010`, run 41154334602)
- [x] **Cross-Machine CAS Sync:** `two_clones_file_remote_cas_race_converges` green (`T-013`, run 41155358409)
- [x] **Singleton daemon lock:** `chaos_stale_daemon_lock_live_vs_dead_pid_adoption` green on 3-OS CI (`T-016`, run 41215529894)
- [x] **Daemon & Crash Safety:** `chaos_sigkill_daemon_mid_merge_recovers_under_2s` green (`< 2 s`, 0 orphaned `index.lock`) (`T-021`, run 41226059939)
- [ ] **Windows NTFS Teardown:** `windows_locked_handle_quarantines_never_fails_merge` green on Windows CI (`T-025`)
- [ ] **4-Week MVP Exit:** `full_lifecycle_spawn_claim_handoff_collect_merged` + `two_agents_mcp_and_cli_parallel_session_zero_overwrites` (`T-029`) + real two-agent run (`T-045`, human-in-the-loop)
- [ ] **Core Coverage Bar:** CI-enforced `meshtree-core` coverage `≥ 85%` (`T-031`)
- [ ] **Benchmark Suite:** All 6 `SPEC.md` §18 metrics reproduced via `xtask bench` (`T-034`)
- [ ] **v1.0 Packaging:** 3-OS static binaries, Homebrew tap, Scoop/winget, and `npx meshtree-mcp` verified (`T-035`, `T-037`, `T-038`)
