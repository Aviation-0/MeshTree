# Agent Operations Board

<!--
Anti-Bloat Constraints:
- Telegraphic, high-density facts only. Zero conversational pleasantries or prose.
- Strict sliding window: Retain ONLY the last 3–5 entries. Any agent appending an entry must prune entries older than the 5th to keep token overhead below 300 tokens permanently.
- Maximum 5 lines per handoff entry.
-->

## Active Lock
- Model: Claude Opus 5.5
- Task: T-003
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
- 2026-10-30 | Gemini 3.8 High: T-002 complete. Config load/validation with MT-050 landed. 3-OS CI green.
- 2026-10-30 | Mistral Large 4: T-001 complete. CLI-git GitOps backend with CAS update-ref semantics merged.
