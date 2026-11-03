# Agent Operations Board

<!--
Anti-Bloat Constraints:
- Telegraphic, high-density facts only. Zero conversational pleasantries or prose.
- Strict sliding window: Retain ONLY the last 3–5 entries. Any agent appending an entry must prune entries older than the 5th to keep token overhead below 300 tokens permanently.
- Maximum 5 lines per handoff entry.
-->

## Active Lock
- Model: None (Idle)
- Task: None
- Status: Idle

## Team Roster & Roles
- Gemini 4 Argon: Lead Architect (ADRs, cross-module scaffolding, spec design)
- Gemini 3.8 High: Deep reviewer & complex platform logic
- Mistral Large 4: Terminal implementer via Vibe CLI (fast iteration, git ops)
- Claude Opus 5.5: Invariant modeling, state machines, property-based testing (proptest)

## Host & Toolchain Invariants
- Windows Host: Missing local `link.exe` / Windows SDK. `cargo check` and `cargo fmt` work locally; full test execution links on the GitHub Actions 3-OS CI matrix.
- Mac Host: full local gates work; `git config user.email` is a placeholder — commit with `-c user.email=109758310+Aviation-0@users.noreply.github.com`.
- Git Identity: Commits must strictly use `Lucas Pan <109758310+Aviation-0@users.noreply.github.com>`. Zero co-author trailers.

## Recent Handoff Log (Sliding window: max 5 entries)
- - 2026-11-02 | Claude Opus 5.5 (Windows): T-043 done (34/46). refs/meshtree/coord tasks/<id>.json registry via CoordStore::sync_with (revision-wins merge); spawn/failed spawn/abort publish. Open: merge gate + sweeper do not publish yet.
    CI green (run 41551357771). Next: T-029.
- - 2026-11-02 | Claude Opus 5.5 (Windows): T-040 done (33/46). Queue slot lapses lease.ttl_secs after grant even with a live holder; next FIFO ticket granted at that instant. No §6 queue TTL key (ADR if wanted).
    CI green (run 41550700924). Next: T-043.
- - 2026-11-02 | Claude Opus 5.5 (Windows): T-039 done (32/46). collect pushes integration to origin with lease; lost race => refetch, replay-merge, retry (<=5) then MT-031; network failure defers push. Two-clone race test.
    CI green (run 41549927497). Next: T-040.
- - 2026-11-02 | Claude Opus 5.5 (Windows): T-032 done (31/46). share_junction_realpath_probe_matrix + docs/compatibility.md (npm/pnpm/Cargo/.venv/Vite/webpack/Next recipes). Windows junction ".." is lexical (task wt), POSIX physical (primary).
    CI green (run 41548903359). Next: T-039.
- - 2026-11-02 | Claude Opus 5.5 (Windows): T-028 done (30/46). meshtree mcp: stateless stdio JSON-RPC proxy, 11 §15 tools, [MT-xxx] isError results (MT-010 no daemon), EOF exit leaves daemon. Daemon +spawn/abort/list/commit_handoff/render methods; claim ttl_secs.
    CI green (run 41540486618). Open: §6 cap key; machine-id. Next: T-039.
