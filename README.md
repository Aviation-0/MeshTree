# MeshTree

MeshTree is a local-first, multi-agent workspace coordinator daemon. Status: pre-MVP — see [SPEC.md](SPEC.md).

## Status

The project is currently in Week 1 of the roadmap in [SPEC.md](SPEC.md) §20.

## Quickstart (Week-1 walking skeleton)

Build from source (Rust 1.75+, `git` on `PATH`):

```bash
cargo install --path crates/meshtree
```

Inside any git repository with at least one commit:

```bash
meshtree init                                  # write .meshtree/config.toml, create .meshtree/
meshtree spawn fix-auth --agent my-agent       # isolated worktree on branch meshtree/<task-id>
meshtree status                                # one line per task; add --json for task records
meshtree abort <task-id>                       # remove the worktree, mark the task abandoned
```

`spawn` prints the `task_id`, `branch`, and `worktree` path; point the agent at that worktree. Task records live in `.meshtree/tasks/<task-id>/task.json`; `--base <ref>` picks a base other than `project.base_branch`.

Exit codes (SPEC §14): `0` ok · `10` usage (bad args, not a repo, missing/invalid config) · `20` task/lease error (unknown task, `MT-004` illegal transition) · `30` daemon · `40` platform (filesystem, git worktree failure).

Claims, the daemon, the merge gate, and MCP arrive in later milestones; see [docs/tasks/backlog.md](docs/tasks/backlog.md).
