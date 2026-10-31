# ADR-0007: Short Fallback Path When `.meshtree/daemon.sock` Exceeds the Unix Socket Limit

- **Status:** Accepted (accepted by Lucas Pan, 2026-10-31; implementation tracked as `T-046`; `SPEC.md` §5 and §10 amended)
- **Date:** 2026-10-31
- **Author:** Claude Opus 5.5 (Agent)
- **Authority:** `SPEC.md` §5 (layout: `.meshtree/daemon.sock`), §10 (IPC table), §16 (`MT-010`), §17 (socket `0600` + parent directory check; no TCP listener); `AGENTS.md` Standing Order #1

## Context

A Unix domain socket address holds at most 103 path bytes on macOS (`sun_path` is 104 bytes including the terminating NUL) and 107 on Linux. §5 and §10 fix the socket at `<repo>/.meshtree/daemon.sock`, which adds 22 bytes to the repository path. On macOS the repository path is also resolved through `/private` (for example `/private/var/folders/…`).

Any repository whose canonical path is longer than about 81 bytes on macOS (85 on Linux) therefore cannot run a daemon: `bind` fails with "path must be shorter than SUN_LEN", and every daemon-backed command (`collect`, `queue`, MCP) reports `MT-010`. `T-021` hit exactly this in CI (run 41225848028) with a temp-directory repository; real projects under deep paths (monorepo checkouts, `~/Library/…`, nested workspaces) will hit it too. Windows is unaffected: it uses the named pipe `\\.\pipe\meshtree-<repo-hash-16>`.

Alternatives considered:

- **Linux abstract-namespace sockets** have no length problem but no filesystem permissions: any local user could connect, violating §17. Not available on macOS.
- **`chdir` + relative `bind`/`connect`** works around the limit, but changing the working directory of a multi-threaded process (the daemon and the MCP proxy) is unsafe.
- **Always using a short path** would move the socket for every repository, contradicting §5 where it is not needed.

## Decision

On Unix, the endpoint for a repository is:

1. `<repo>/.meshtree/daemon.sock` when that path fits the platform limit (unchanged behavior);
2. otherwise `/tmp/meshtree-<uid>/<repo-hash-16>.sock`, where `<repo-hash-16>` is the first 16 hex digits of SHA-1 over the canonical repository root (the same derivation as the Windows pipe name, without lower-casing, since Unix paths are case-sensitive).

Both the daemon and every client compute the endpoint with the same function (`Endpoint::for_repo`), so no discovery file is needed.

§17 for the fallback directory:

- `/tmp/meshtree-<uid>` is created with mode `0700`. If it already exists it must be a real directory (not a symlink) owned by the current uid with mode `0700`; otherwise the daemon refuses to bind and clients refuse to connect (another user could have planted it).
- The socket itself stays `0600`, with the existing parent-directory owner/mode check applied to the fallback directory.
- When the fallback is used, the daemon logs the socket path at startup.

## Consequences

- Daemons work for repositories at any depth on macOS and Linux, keeping §17's per-user access control.
- `SPEC.md` §5 and §10 now note the fallback path (amended with this ADR's acceptance).
- Implementation is a small change to `meshtree-platform::ipc` (endpoint derivation plus the fallback directory check) with tests: a long-path repository binds and serves through the fallback; a fallback directory with the wrong owner, mode or a symlink is refused. Tracked as `T-046` (M2), a dependency of the `T-029` MVP exit.
- The CLI test temp directories were shortened in `T-021` as a stop-gap; they can stay short regardless.
