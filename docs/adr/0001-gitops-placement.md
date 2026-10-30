# ADR-0001: Place `GitOps` Trait and CLI-Git Backend in `meshtree-platform`

- **Status:** Accepted
- **Date:** 2026-10-30
- **Author:** Project Architect (Agent)
- **Authority:** `SPEC.md` §4, Appendix A, Appendix B; `AGENTS.md` Standing Order #1

## Context

`SPEC.md` §4 specifies that all git operations shell out to the `git` CLI (`-z` / `--porcelain` machine-readable flags) behind a `GitOps` trait.

However, there is a crate-boundary discrepancy between the initial task backlog and `SPEC.md` Appendix B:

1. `docs/tasks/backlog.md` (`T-001` and `T-005`) places `gitops` in `crates/meshtree-platform` (`platform::gitops`).
2. `SPEC.md` Appendix B currently summarizes the three crates as:
   - `meshtree-core/`: lease table, replay, task model, queue, handoff (pure; no fs, no git)
   - `meshtree-platform/`: IPC (UDS/named pipe), locks, junction/clonefile, quarantine, WAL
   - `meshtree/`: bin: daemon, cli (clap), mcp (rmcp); git plumbing behind `GitOps` trait

Per `AGENTS.md` Standing Order #1 (`SPEC.md` is authoritative; propose a change in `docs/adr/` rather than silently deviating), we must explicitly resolve whether `gitops` lives in `crates/meshtree-platform` or `crates/meshtree`.

## Decision

Keep the `GitOps` trait and its CLI-git implementation (`CliGitOps`) in `crates/meshtree-platform` (module `gitops`), and amend `SPEC.md` Appendix B accordingly.

## Rationale

1. **Process and OS-boundary cohesion:** `meshtree-platform` is the dedicated boundary crate for operating-system, filesystem, and subprocess primitives. Spawning `git` child processes, configuring platform-specific environment/handle inheritance, parsing NUL-delimited (`-z`) and `--porcelain` byte streams, and coordinating `git worktree remove --force` with Windows NTFS mandatory-lock teardown/quarantine (`SPEC.md` §10) are OS/process-boundary concerns.
2. **Thin binary crate:** Keeping `GitOps` in `meshtree-platform` allows `crates/meshtree` (the binary crate) to remain a thin wiring layer for the three user-facing surfaces (`cli`, `daemon`, `mcp`) without embedding low-level subprocess plumbing.
3. **Direct integration testability:** Integration tests for git worktree management, `refs/meshtree/coord` object-database plumbing (`hash-object`, `mktree`, `commit-tree`, `update-ref`), and `--force-with-lease` CAS pushes against `file://` remotes can run directly against `meshtree-platform` (`T-001`, `T-011`, `T-012`) before CLI or daemon subcommands are wired up.
4. **Core purity preserved:** `meshtree-core` remains 100% pure (no filesystem, no subprocesses, no git), preserving all determinism and property-testing invariants in `SPEC.md` §18.

## Consequences

- `crates/meshtree-platform` exposes `pub mod gitops` containing the `GitOps` trait and `CliGitOps` implementation.
- `T-001` and `T-005` in `docs/tasks/backlog.md` retain their existing scope fences (`crates/meshtree-platform` module `gitops`).
- Upon approval of this ADR, `SPEC.md` Appendix B (lines 602–609) will be amended to:

```text
meshtree/
├── crates/
│   ├── meshtree-core/     # lease table, replay, task model, queue, handoff (pure; no fs, no git)
│   ├── meshtree-platform/ # IPC (UDS/named pipe), locks, junction/clonefile, quarantine, WAL, git plumbing (GitOps)
│   └── meshtree/          # bin: daemon, cli (clap), mcp (rmcp)
├── xtask/                 # benchmark scenario runner
└── .github/workflows/     # ci.yml (3-OS matrix), release.yml (cargo-dist)
```

and the closing sentence of Appendix B will read:

> `meshtree-core` is the invariant-protected heart: everything in it is deterministic and property-tested; platform, filesystem, and git concerns live behind traits in `meshtree-platform`, keeping the `meshtree` binary crate a thin surface layer.
