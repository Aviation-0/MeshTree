# Agent Operations Board

<!--
Anti-Bloat Constraints:
- Telegraphic, high-density facts only. Zero conversational pleasantries or prose.
- Strict sliding window: Retain ONLY the last 3–5 entries. Any agent appending an entry must prune entries older than the 5th to keep token overhead below 300 tokens permanently.
- Maximum 5 lines per handoff entry.
-->

## Active Lock
- Model: Claude Opus 5.5
- Task: T-010
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
- 2026-10-30 | Claude Opus 5.5: T-009 complete. core::pattern normalize() + conflicts() = exact glob intersection w/ subtree coverage, case-insensitive; .meshtree-led claims rejected.
  ADR-0003 (needs human review): SPEC §7 heuristic misses a*/*b, so exact superset used. Exhaustive table vs brute force: 0 false negatives. CI green (run 41154122988).
- 2026-10-30 | Claude Opus 5.5: T-008 complete. tests/lease_replay_props.rs: 256 random logs vs shuffled/duplicated/split-union copies → byte-identical lease table + MT-001 rejections; no cross-task conflicting leases.
  Extreme-TTL no-panic property. CI green (run 41153826336). Next: T-009.
- 2026-10-30 | Claude Opus 5.5: T-007 complete. core::lease event model + (ts,counter,machine,id) key, holder-ts expiry (ttl+grace), replay() w/ id de-dup + MT-001 Rejections; overlap = caller predicate (T-009 plugs in).
  core::time::Timestamp canonical RFC 3339 ms. 2 named tests. CI green (run 41153630251). Next: T-008.
- 2026-10-30 | Claude Opus 5.5: T-006 complete. status (text/--json; missing tasks/ = empty, malformed warned+skipped) + abort (active→abandoned, worktree removed, branch kept; else MT-004 exit 20). 5 tests. CI green (run 41153075068).
  Template fix: [machine] id commented out; spawn derives mc-<hostname> (env, then `hostname`). Backlog audit: added T-039 (§8 non-ff retry), T-040 (§12 ticket TTL), T-041 (§19 crates.io), T-042 (MCP listing) → 42 tasks.
  ADR candidates: §13 configurable cap, §17 queue override allow-list, §11 .venv recreate (no §6 keys). Next: T-007.
- 2026-10-30 | Claude Opus 5.5: T-005 complete. `meshtree spawn`: ULID → task.json (spawning) → worktree add meshtree/<id> → active|failed; atomic temp+rename writes.
  Bad base rejected pre-write (exit 10). std-only clock/entropy/RFC 3339 (src/clock.rs); repo/config helpers in src/repo.rs. 5 integration tests. CI green (run 41152201797).
  Deferred: sharing T-023, coord registry + --claim T-013/14, OS machine-id T-016. Flag: Config::template() hard-codes machine id "mc-lucas-mac". Next: T-006.
