# Agent Operations Board

<!--
Anti-Bloat Constraints:
- Telegraphic, high-density facts only. Zero conversational pleasantries or prose.
- Strict sliding window: Retain ONLY the last 3–5 entries. Any agent appending an entry must prune entries older than the 5th to keep token overhead below 300 tokens permanently.
- Maximum 5 lines per handoff entry.
-->

## Active Lock
- Model: Claude Opus 5.5 (Windows)
- Task: T-024 (teardown backoff, quarantine, prune)
- Status: doing

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
- 2026-10-31 | Claude Opus 5.5 (Windows): T-023 done (24/45). spawn links [[share]] dirs (MT-040 never fails spawn), info/exclude for links; all worktree deletions unlink shares first.
  CI green (run 41227946875). Next: T-024.
- 2026-10-31 | Claude Opus 5.5 (Windows): T-022 done (23/45). platform::share: junction (FSCTL_SET_REPARSE_POINT) / clonefile / symlink, auto per OS, MT-040 fallback to none, non-following unlink.
  CI green (run 41227678198). Local clippy now covers linux + macOS targets. Next: T-023.
- 2026-10-31 | Claude Opus 5.5 (Windows): T-021 done (22/45). Startup recovery before bind: WAL↔task.json reconcile, aged git-lock cleanup, integration reset, landed merges completed / interrupted collects → active; ping reports recovery_ms.
  CI green (run 41226059939). Open: macOS deep repo paths overflow daemon.sock SUN_LEN (ADR needed); ADR-0005/0003. Next: T-022.
- 2026-10-31 | Claude Opus 5.5 (Windows): T-020 done (21/45), session end. Merge gate: collect_task RPC (validate via queue, MT-032 claim check, merge mutex, integration worktree merge, promote_validate before ref CAS) + meshtree promote (ff-only).
  CI green (run 41224611824) after Windows CRLF test fix. Open: ADR-0005 + ADR-0003 acceptance. Next: T-021.
- 2026-10-31 | Claude Opus 5.5 (Windows): T-019 done (20/45). core::queue FIFO + dead-holder eviction; daemon queue RPCs; meshtree queue CLI; ADR-0005 allow_args (Proposed, needs human acceptance).
  CI green (run 41223916867). Next: T-020 merge gate.
