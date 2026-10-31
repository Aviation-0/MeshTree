# Agent Operations Board

<!--
Anti-Bloat Constraints:
- Telegraphic, high-density facts only. Zero conversational pleasantries or prose.
- Strict sliding window: Retain ONLY the last 3–5 entries. Any agent appending an entry must prune entries older than the 5th to keep token overhead below 300 tokens permanently.
- Maximum 5 lines per handoff entry.
-->

## Active Lock
- Model: Claude Opus 5.5
- Task: T-007
- Status: Doing

## Team Roster & Roles
- Gemini 4 Argon: Lead Architect (ADRs, cross-module scaffolding, spec design)
- Gemini 3.8 High: Deep reviewer & complex platform logic
- Mistral Large 4: Terminal implementer via Vibe CLI (fast iteration, git ops)
- Claude Opus 5.5: Invariant modeling, state machines, property-based testing (proptest)

## Host & Toolchain Invariants
- Windows Host: Missing local `link.exe` / Windows SDK. `cargo check` and `cargo fmt` work locally; full test execution links on the GitHub Actions 3-OS CI matrix.
- Git Identity: Commits must strictly use `Lucas Pan <109758310+Aviation-0@users.noreply.github.com>`. Zero co-author trailers.

## Recent Handoff Log (Sliding window: max 5 entries)
- 2026-10-30 | Claude Opus 5.5: T-006 complete. status (text/--json; missing tasks/ = empty, malformed warned+skipped) + abort (active→abandoned, worktree removed, branch kept; else MT-004 exit 20). 5 tests. CI green (run 41153075068).
  Template fix: [machine] id commented out; spawn derives mc-<hostname> (env, then `hostname`). Backlog audit: added T-039 (§8 non-ff retry), T-040 (§12 ticket TTL), T-041 (§19 crates.io), T-042 (MCP listing) → 42 tasks.
  ADR candidates: §13 configurable cap, §17 queue override allow-list, §11 .venv recreate (no §6 keys). Next: T-007.
- 2026-10-30 | Claude Opus 5.5: T-005 complete. `meshtree spawn`: ULID → task.json (spawning) → worktree add meshtree/<id> → active|failed; atomic temp+rename writes.
  Bad base rejected pre-write (exit 10). std-only clock/entropy/RFC 3339 (src/clock.rs); repo/config helpers in src/repo.rs. 5 integration tests. CI green (run 41152201797).
  Deferred: sharing T-023, coord registry + --claim T-013/14, OS machine-id T-016. Flag: Config::template() hard-codes machine id "mc-lucas-mac". Next: T-006.
- 2026-10-30 | Claude Opus 5.5: T-004 complete. `meshtree init`: repo root via nearest .git; creates .meshtree/{worktrees,tasks,quarantine}, config.toml template, .gitignore (only config + task handoffs visible).
  Idempotent (create_new, never overwrites); invalid existing config → MT-050 exit 10. §14 exit codes in crates/meshtree/src/exit.rs.
  6 integration tests. 3-OS CI green (run 41151620305). Unblocks T-005 (spawn).
- 2026-10-30 | Claude Opus 5.5: T-003 complete. meshtree-core::task: SPEC §8 machine (7 states, 9 edges), illegal → MT-004 (ADR-0002; MT-010/011 rejected as daemon codes).
  Monotonic t-+ULID allocator, clock/entropy injected (pure). 3 named unit tests + 4 proptest properties (arbitrary sequences, terminal absorption, ID order/bijection).
  3-OS CI green (run 41151157057). Unblocks T-004 (init) → T-005 (spawn). T-028 must map MT-004. gh CLI unauthenticated on host; CI read via public API.
- 2026-10-30 | Gemini 3.8 High: T-002 complete. Config load/validation with MT-050 landed. 3-OS CI green.
