# ADR-0006: Teardown Fallback When an Exclusive Lock Blocks the Quarantine Move

- **Status:** Accepted (accepted by Lucas Pan, 2026-10-31; implemented by `T-024`, exercised by `T-025`; `SPEC.md` §10 step 3 amended)
- **Date:** 2026-10-31
- **Author:** Claude Opus 5.5 (Agent)
- **Authority:** `SPEC.md` §10 (Trap 2, teardown steps 3–4), §16 (`MT-020`, `MT-021`), §18; `AGENTS.md` Standing Order #1

## Context

§10 step 3 says that when teardown keeps failing, MeshTree moves the worktree (same-volume `MoveFileEx`) to `.meshtree/quarantine/<task>/` and marks the task done with a quarantine note (`MT-021`). The `T-025` exit criterion expects that note for a file held with `dwShareMode = 0`.

Windows does not allow it. A directory cannot be renamed or moved while any file beneath it is open without `FILE_SHARE_DELETE`; `MoveFileEx` fails with `ERROR_ACCESS_DENIED`. This was verified on Windows 11: `Directory.Move` of a tree containing a file opened with `FileShare.None` fails, while the file's siblings can still be deleted. The quarantine move therefore only succeeds for locks that block deletion but not the move of an ancestor, never for the exclusive-handle case §18 tests.

## Decision

The teardown pipeline (`meshtree_platform::teardown`) is:

1. Retry removal with the §10 backoff (250 ms → 8 s).
2. Move the tree to `.meshtree/quarantine/<task>` (a unique suffix if taken). Success is `MT-021` (quarantined).
3. **If the move fails:** delete everything that can be deleted (post-order, never following links), register what remains for deletion at reboot (`MoveFileExW(…, MOVEFILE_DELAY_UNTIL_REBOOT)`; this needs administrator rights and is skipped otherwise), and report `MT-020` (file locked), naming the path left in place.
4. If step 3 deleted everything, the outcome is plain removal.

In every case the task still reaches its terminal status (`merged` / `abandoned`), and the note is reported. The leftover is reclaimed by `meshtree prune` (a finished task's leftover worktree) or by the daemon's idle quarantine GC.

## Consequences

- `T-025`'s test accepts either note (`MT-021` when the move succeeds, `MT-020` when an exclusive handle blocks it) and asserts the essential §10/§18 property: the merge (or abort) never fails, and the leftover is cleaned once the handle closes.
- After the fallback, git still records the partially deleted worktree until it is removed, so its task branch stays checked out there. `meshtree prune` finishes the removal.
- `SPEC.md` §10 step 3 now describes this fallback (amended with this ADR's acceptance).
