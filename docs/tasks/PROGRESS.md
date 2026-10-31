# MeshTree Progress Dashboard

Updated at the start and end of every work session per `AGENTS.md`. Authoritative task definitions live in [`docs/tasks/backlog.md`](backlog.md).

## Current Focus

- **Active Milestone:** `M0 — Walking Skeleton & Core Lease Foundations` (Week 1 · Target: `2026-11-08`)
- **In-Progress Task (`doing`):** None
- **Next Up (`todo`, dependencies satisfied):**
  1. `T-004` — `meshtree init` CLI (`crates/meshtree`, unblocked by `T-002`)
  2. `T-007` — Lease event model + ordering key (`crates/meshtree-core`, independent pure core)
  3. `T-009` — Path-pattern normalization + conflict rule (`crates/meshtree-core`, independent pure core)
- **Last Session Summary (2026-10-30, Claude Opus 5.5):** `T-003` done — `meshtree-core::task` with `Task`, monotonic `"t-" + ULID` allocator, §8 state machine (7 statuses, 9 edges), `MT-004` illegal-transition errors (`ADR-0002`), named unit tests + proptest lifecycle invariants. 3-OS CI green (run 41151157057). Prior: `T-002` done (`config`, run 41149054292).

---

## Milestone Scorecard

| Milestone | Window | Tasks | Total | `done` | `doing` | `blocked` | `todo` | Status |
|---|---|---|---|---|---|---|---|---|
| **M0 — Walking Skeleton & Core Lease Foundations** | Week 1 (`→ 2026-11-08`) | `T-001`–`T-010` | 10 | 3 | 0 | 0 | 7 | `doing` |
| **M1 — Cross-Machine Lease Protocol & CAS Sync** | Week 2 (`→ 2026-11-15`) | `T-011`–`T-014` | 4 | 0 | 0 | 0 | 4 | `todo` |
| **M2 — Daemon, IPC, WAL, Shared Queue & Merge Gate** | Week 3 (`→ 2026-11-22`) | `T-015`–`T-021` | 7 | 0 | 0 | 0 | 7 | `todo` |
| **M3 — Sharing, Windows Teardown, Handoffs, MCP & MVP** | Week 4 (`→ 2026-11-28`) | `T-022`–`T-029` | 8 | 0 | 0 | 0 | 8 | `todo` |
| **M4 — Chaos Suite, Coverage Gate & Technical Docs** | Weeks 5–6 (`→ 2026-12-12`) | `T-030`–`T-033` | 4 | 0 | 0 | 0 | 4 | `todo` |
| **M5 — Benchmark Suite, Packaging & Dogfood Study** | Weeks 7–8 (`→ 2026-12-26`) | `T-034`–`T-036` | 3 | 0 | 0 | 0 | 3 | `todo` |
| **M6 — Beta Triage & v1.0 Launch** | Weeks 9–10 (`→ 2027-01-08`) | `T-037`–`T-038` | 2 | 0 | 0 | 0 | 2 | `todo` |
| **Total** | **Weeks 1–10** | **`T-001`–`T-038`** | **38** | **3** | **0** | **0** | **35** | **8% (`3 / 38`)** |

---

## Quality & Launch Gates (`SPEC.md` §18 & §22)

- [ ] **M0 Walking Skeleton:** `walking_skeleton_init_spawn_status_abort` green on 3-OS CI (`T-010`)
- [ ] **Cross-Machine CAS Sync:** `two_clones_file_remote_cas_race_converges` green (`T-013`)
- [ ] **Daemon & Crash Safety:** `chaos_sigkill_daemon_mid_merge_recovers_under_2s` (`< 2 s`, 0 orphaned `index.lock`) (`T-021`)
- [ ] **Windows NTFS Teardown:** `windows_locked_handle_quarantines_never_fails_merge` green on Windows CI (`T-025`)
- [ ] **4-Week MVP Exit:** `full_lifecycle_spawn_claim_handoff_collect_merged` + `two_agents_mcp_and_cli_parallel_session_zero_overwrites` (`T-029`)
- [ ] **Core Coverage Bar:** `meshtree-core` coverage `≥ 85%` (`T-031`)
- [ ] **Benchmark Suite:** All 6 `SPEC.md` §18 metrics reproduced via `xtask bench` (`T-034`)
- [ ] **v1.0 Packaging:** 3-OS static binaries, Homebrew tap, Scoop/winget, and `npx meshtree-mcp` verified (`T-035`, `T-037`, `T-038`)
