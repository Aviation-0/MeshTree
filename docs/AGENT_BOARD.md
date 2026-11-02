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
- 2026-11-02 | Claude Opus 5.5 (Mac): T-027 done (29/46). meshtree handoff (--json-file/--set) commits handoff.json+HANDOFF.md on task branch via plumbing + worktree ff-only (agent WIP kept); render-agents rewrites AGENTS.md block on change.
  CI green (run 41509759232). Open: §6 cap key; OS machine-id unowned. Next: T-028.
- 2026-10-31 | Claude Opus 5.5 (Windows): T-026 done (28/46). core::handoff: HandoffRecord, HANDOFF.md + AGENTS.md block renderers (id order, 10-line cap, marker forgery defused), idempotent CRLF-preserving splice.
  CI green (run 41231429375). Open: no §6 key for the 10-line cap. Next: T-027.
- 2026-10-31 | Claude Opus 5.5 (Windows): T-046 done (27/46). Unix socket falls back to /tmp/meshtree-<uid>/<hash16>.sock past the path limit; dir must be our own real 0700 dir (bind+connect).
  CI green (run 41230869170). Next: T-026.
- 2026-10-31 | Claude Opus 5.5 (Windows): ADR-0007 Accepted by Lucas Pan; SPEC §5/§10 amended; T-046 (Unix socket path fallback, M2) added as dep of T-029.
  Totals 26/46. Next: T-026.
- 2026-10-31 | Claude Opus 5.5 (Windows): ADR review — ADR-0003/0005/0006 Accepted by Lucas Pan; SPEC §10 step 3 amended (ADR-0006).
  ADR-0007 Proposed: /tmp/meshtree-<uid>/<hash16>.sock fallback for long Unix socket paths (needs acceptance + task T-046). Next: T-026.
