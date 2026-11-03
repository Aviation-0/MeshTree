# MeshTree MCP Server

`meshtree mcp` is a stdio MCP server that proxies every tool call to the repository's `meshtree daemon` (`SPEC.md` §15). It holds no state of its own and exits with its client without stopping the daemon.

## Install

Start the daemon in the repository first (`meshtree daemon`), then point your MCP client at the server.

**Through npx** (no install; downloads the matching release binary on first use, verified by SHA-256):

```json
{
  "mcpServers": {
    "meshtree": { "command": "npx", "args": ["-y", "meshtree-mcp"] }
  }
}
```

**With `meshtree` already installed** (`cargo install meshtree`, Homebrew, Scoop or a release archive):

```json
{
  "mcpServers": {
    "meshtree": { "command": "meshtree", "args": ["mcp"] }
  }
}
```

The npx launcher (`packages/meshtree-mcp`) uses `MESHTREE_BIN` if set, then `meshtree` on `PATH`, then the downloaded release binary cached under `~/.cache/meshtree-mcp/` (override with `MESHTREE_MCP_CACHE`). Release binaries exist for macOS arm64, Linux x64 and Windows x64.

The client must start the server inside the repository (or a task worktree), since that is how it finds the daemon.

## Tools

| Tool | Purpose |
|---|---|
| `spawn_task` | Create an isolated task worktree and branch, optionally claiming paths first. |
| `task_status` | One task record, or all of them. |
| `list_tasks` | Task records, optionally only live ones. |
| `claim_paths` | Claim path patterns for a task: all granted, or `MT-001` naming the holders. |
| `release_paths` | Release claims. |
| `request_shared_op` | Run a configured shared operation (tests, build) through its FIFO queue. |
| `commit_handoff` | Record the task's handoff on its branch and refresh the `AGENTS.md` block. |
| `collect_task` | Validate and merge the task into the integration branch. |
| `abort_task` | Discard a task, releasing its claims and worktree. |
| `render_agents_block` | Regenerate the Active Sessions block in `AGENTS.md`. |
| `sync` | Flush and pull coordination-ref events. |

Failures come back as MCP tool errors carrying a MeshTree code (`MT-001` claim conflict, `MT-010` daemon unreachable, …), so agents can react; see the catalog in [`protocol.md`](protocol.md#10-error-catalog). Parameters and results match the IPC methods of the same names ([`protocol.md` §9.4](protocol.md#94-methods)).

## Directory listing

`packages/meshtree-mcp/server.json` is the listing manifest in the MCP registry's `server.json` format: server name `io.github.aviation-0/meshtree`, the `meshtree-mcp` npm package over stdio, and the tool list under `_meta` (publisher-provided). The npm package's `mcpName` field proves ownership to the registry. The test `mcp_listing_manifest_matches_server_tools` fails if the manifest's tools, descriptions, versions or names drift from what `meshtree mcp` serves.

Publishing to the registry is a manual step after the npm package is out: `mcp-publisher login github` then `mcp-publisher publish` from `packages/meshtree-mcp/`. The release workflow keeps `server.json`'s version in step with the npm package.
