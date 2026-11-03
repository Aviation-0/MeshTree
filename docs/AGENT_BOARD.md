# Agent Operations Board

<!--
Anti-Bloat Constraints:
- Telegraphic, high-density facts only. Zero conversational pleasantries or prose.
- Strict sliding window: Retain ONLY the last 3–5 entries. Any agent appending an entry must prune entries older than the 5th to keep token overhead below 300 tokens permanently.
- Maximum 5 lines per handoff entry.
-->

## Active Lock
- Model: Claude Opus 5.5 (Windows)
- Task: T-049 (daemon in-memory claims, WAL events, background commit (ADR-0009 Part 2))
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
- 2026-11-03 | Claude Opus 5.5 (Mac): Lucas Pan approved T-049 scope widening into core::pattern (fast-negative pre-check in conflicts_raw; zero false negatives must hold).
  Backlog scope column updated; T-049 lock/status untouched. Next: T-049 (Windows) re-measure; T-030, T-052.
- 2026-11-03 | Claude Opus 5.5 (Mac): alignment audit (docs only). ADR-0010 (log compaction) + ADR-0011 (config keys, OS machine-id, .venv) Accepted; SPEC amended. Added T-050/T-051/T-052; T-030 unblocked from T-045.
  Totals 38/52. T-049 lock untouched; its pattern-scope request still needs Lucas's answer. Next: T-030, T-052.
- 2026-11-02 | Claude Opus 5.5 (Windows): T-049 IN PROGRESS (lock held). In-memory claims + WAL events + bg commit landed, CI green. Bench claim p95 @100 ev 5/14/30 ms OK; @2000 ev 67/122/77 ms NOT met.
  Next: widen scope to core::pattern fast-negative (needs OK), re-measure.
- 2026-11-02 | Claude Opus 5.5 (Windows): T-048 done (38/49). Sync = const ~10 git procs (batched cat-file/hash-object, blob reuse). Claim time still O(log) in-process (877 ms Linux @2000 events); T-049 + future compaction ADR.
  CI green (run 41561529557). Next: T-049, T-045 (human).
- 2026-11-02 | Claude Opus 5.5 (Windows): T-047 done (37/49). Daemon transitions (collect edges, recovery, sweeper abandon) publish tasks/<id>.json best-effort; recovery publishes deferred until serving.
  CI green (run 41559983307). Next: T-045 (human), T-048.
