# MeshTree Task Backlog

One task at a time, lowest `todo` number first. Scope fences are binding.

| ID | Status | Title | Scope | Exit criterion |
|----|--------|-------|-------|----------------|
| T-001 | todo | `GitOps` trait + CLI-git backend | `crates/meshtree-platform` (module `gitops`) | worktree add/remove/list, ref read, `update-ref` CAS work against temp repos in tests |
| T-002 | todo | Config load & validation | `meshtree-core` (module `config`) | serde parse of `.meshtree/config.toml` spec fields; typed errors |
| T-003 | todo | Task model + ID allocation | `meshtree-core` (module `task`) | Task record, ULID-based ids, serde round-trip tests |
| T-004 | todo | `meshtree init` CLI | `crates/meshtree` | writes config template; idempotent |
| T-005 | todo | `meshtree spawn` | `crates/meshtree` + `platform::gitops` | creates worktree + branch + task.json; integration test on temp repo |
| T-006 | todo | `meshtree status` / `meshtree abort` | `crates/meshtree` | lists tasks from registry; abort removes worktree + record |
| T-007 | todo | Lease event model + ordering key | `meshtree-core` (module `lease`) | event types ACQUIRE/RELEASE/HEARTBEAT/EXPIRE_SWEEP; ordering key defined |
| T-008 | todo | Deterministic replay → lease table | `meshtree-core` | property test: 200 randomized event streams → identical lease table |
| T-009 | todo | Path-pattern normalization + conflict rule | `meshtree-core` | conservative overlap rule; exhaustive case table, zero false negatives |
| T-010 | todo | Week-1 exit smoke test + quickstart | repo root | spawn completes < 5s on a repo with a shared node_modules link; README quickstart section |

## Handoff log
