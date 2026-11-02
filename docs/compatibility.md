# Dependency-Sharing Compatibility (Junctions, Symlinks, Realpath)

Appendix to `SPEC.md` §11 (Trap 3) and §21 ("Junction/realpath incompatibility with some node tooling"). `[[share]]` links a directory of the primary worktree into each task worktree. This page records what a linked directory looks like to tools, which tools are known to care, and how to opt out per directory with `strategy = "none"`.

## What is verified

The named test `share_junction_realpath_probe_matrix` (`crates/meshtree-platform/tests/share.rs`) runs on the 3-OS CI matrix. It builds a primary `node_modules` containing a regular package and a pnpm-style nested link (`node_modules/pnpm-pkg` → `node_modules/.pnpm/pnpm-pkg@1.0.0/node_modules/pnpm-pkg`), shares it into a task worktree, and probes it the way resolvers and watchers do:

| Observation (through `<worktree>/node_modules`) | `auto` on Windows (junction) | `auto` on macOS / Linux (symlink) | `clonefile` (macOS) | `none` |
|---|---|---|---|---|
| `lstat` reports a link | yes | yes | no (real directory) | no directory |
| `readlink` names the primary's directory | yes | yes | — | — |
| `realpath` of a package file lands in the **primary** worktree | **yes** | **yes** | no (stays in the task worktree) | — |
| `<link>/..` physically resolves to the **primary** worktree root | **yes** | **yes** | no | — |
| A write through the link appears in the primary (shared state) | **yes** | **yes** | no (copy-on-write) | — |
| A nested pnpm link inside the shared tree resolves | yes | yes | yes | — |
| Removing a worktree deletes only the link, never the primary's files | yes (`T-022`/`T-024` tests) | yes | n/a (independent copy) | n/a |

The rows in bold are the source of every incompatibility below: a tool that follows links sees the dependency at the primary worktree's path, outside the task worktree's project root, and every task shares one mutable copy.

## What is not verified here

The per-tool notes below come from each tool's documented options and known issues. CI does not install or run these tools. If a note turns out wrong for a version you use, `strategy = "none"` for that directory is always safe; please report it so this page can be corrected.

## Per-tool notes and recipes

Configuration lives in `.meshtree/config.toml` (`SPEC.md` §6):

```toml
[[share]]
source = "node_modules"
strategy = "auto"   # auto | junction | symlink | clonefile | none
```

### npm (`node_modules`)

- **Resolution works.** Node.js resolves symlinks and junctions to their real path by default (`--preserve-symlinks` turns this off). Because the whole `node_modules` is shared, a package's own dependencies resolve inside the same real tree.
- **Installs are shared.** `npm install` / `npm ci` run in a task worktree write through the link into the primary's `node_modules`, changing it for every task. `npm ci` deletes `node_modules` first; through a link that empties the primary's copy.
- **Recipe.** Install only in the primary worktree. A task that changes dependencies (edits `package.json` / the lockfile) should use its own copy:

  ```toml
  [[share]]
  source = "node_modules"
  strategy = "none"   # this task line installs its own dependencies
  ```

### pnpm

- The content-addressable store is global and already shared between worktrees; `node_modules/<pkg>` entries are links into `node_modules/.pnpm/…`. Sharing the whole `node_modules` keeps those inner links working (verified row "nested pnpm link").
- `pnpm install` in a task worktree mutates the primary's virtual store through the link, as with npm. Same recipe: install in the primary only, or `strategy = "none"` and let pnpm hard-link from its global store (cheap).

### Cargo (`target/`)

- `SPEC.md` §11: `target` is **not shared**. Cargo takes its own file lock on the target directory, so concurrent builds from several worktrees would serialize on one lock or rebuild each other's artifacts; fingerprints also embed absolute source paths, so a shared `target` thrashes between worktrees.
- **Recipe.** Do not add a `[[share]]` entry for `target` (or set `strategy = "none"`). Use a `[queue.build]` (§12) to serialize heavy builds instead. `~/.cargo/registry` is already per-user and shared by every worktree without MeshTree.

### Python (`.venv`)

- `SPEC.md` §11: **never shared**. A virtual environment records absolute paths (`pyvenv.cfg` `home`, console-script shebangs, `.pth` entries); a linked `.venv` runs the primary worktree's code, not the task's, for editable installs.
- **Recipe.** Leave `.venv` out of `[[share]]` (or `strategy = "none"`) and recreate it per worktree from the lockfile (`uv sync`, `poetry install`, `python -m venv .venv && pip install -r requirements.txt`). `uv`'s and pip's download caches are already shared per user.

### Vite / esbuild

- Both follow links by default (`resolve.preserveSymlinks: false` in Vite, `preserveSymlinks: false` in esbuild), so a dependency's real path is in the primary worktree.
- Vite's dev server refuses to serve files outside `server.fs.allow` (default: the workspace root). Files reached through a shared `node_modules` live outside the task worktree's root, which can surface as "outside of Vite serving allow list" errors, typically for assets or CSS served directly from a dependency.
- **Recipes**, in order of preference:
  1. allow the primary worktree: `server: { fs: { allow: ['<primary worktree path>', '.'] } }` (task worktrees live under `<primary>/.meshtree/worktrees/`, so `allow: ['../../..']` reaches the primary from a task worktree);
  2. `resolve: { preserveSymlinks: true }` so paths stay inside the task worktree (may duplicate module instances for linked monorepo packages);
  3. `strategy = "none"` for `node_modules`.

### webpack / watchpack

- `resolve.symlinks` (default `true`) resolves modules to their real path, so module ids and source maps name the primary worktree. Set `resolve.symlinks: false` to keep task-worktree paths.
- watchpack watches real paths. A dependency change made through any task's link (e.g. an install in another task) is seen by every task's watcher. With `snapshot.managedPaths` (default: `node_modules`), webpack may also cache-key managed packages by the primary's path.
- **Recipe.** Keep installs in the primary (see npm). If a build depends on `node_modules` being inside the project root (custom loaders with `include: path.resolve(__dirname, 'node_modules/…')`, strict `resolve.roots`), use `resolve.symlinks: false` or `strategy = "none"`.

### Next.js (webpack and Turbopack)

- Under webpack, the webpack notes above apply.
- Turbopack only reads files under its project root. A `node_modules` that resolves to the primary worktree is outside a task worktree's root, which can surface as module-not-found errors for dependencies. Next.js lets you widen the root (`turbopack.root`, and `outputFileTracingRoot` for output tracing) to a common ancestor; the primary worktree is one, since task worktrees live under `<primary>/.meshtree/worktrees/`.
- **Recipe.** Set the Turbopack root to the primary worktree, or `strategy = "none"` for `node_modules` in Next.js projects. The `.next` cache must never be shared: it is per-build output.

## Choosing a strategy

| Situation | Strategy |
|---|---|
| Tasks only edit source; dependencies are installed once in the primary | `auto` (default) |
| A task changes dependencies or runs installs | `none` for that directory |
| Tool insists dependencies are inside the project root and cannot be configured | `none` (or `clonefile` on macOS: independent copy, no link) |
| Large read-only cache on macOS | `clonefile` |
| Windows without Developer Mode | `auto` / `junction` (symlinks need Developer Mode) |

A strategy that fails on the current OS never fails `spawn`: the entry falls back to `none` with `MT-040` (`SPEC.md` §16).
