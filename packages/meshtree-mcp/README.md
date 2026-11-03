# meshtree-mcp

Runs the [MeshTree](https://github.com/Aviation-0/MeshTree) MCP server (`meshtree mcp`, stdio) through `npx`:

```json
{
  "mcpServers": {
    "meshtree": { "command": "npx", "args": ["-y", "meshtree-mcp"] }
  }
}
```

The launcher uses `MESHTREE_BIN` if set, then `meshtree` on `PATH`. Otherwise it downloads the release binary matching this package's version from GitHub Releases, verifies its SHA-256, and caches it under `~/.cache/meshtree-mcp/` (override with `MESHTREE_MCP_CACHE`). Supported: macOS arm64, Linux x64, Windows x64.

The MCP server proxies to `meshtree daemon`, which must be running in the repository.
