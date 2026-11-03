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
- Model: Claude Opus 5.5 (Mac)
- Task: T-052 (ADR-0011 config keys, OS machine-id, .venv)
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
- 2026-11-03 | Claude Opus 5.5 (Mac): T-033 done (41/53). docs/protocol.md + docs/architecture.md; docs_schema.rs parses every JSON example against code types, catalog vs SPEC §16, methods vs dispatch.
  CI green (run 41683066476). Next: T-052; T-035 (needs Lucas's secrets); T-050/T-053 after T-049.
- 2026-11-03 | Claude Opus 5.5 (Mac): T-031 done (40/53). §17 suite: queue overrides, .meshtree claims, Unix socket dir, Windows pipe DACL read-back; CI coverage job (core 97.0% lines, gate 85%).
  CI green (run 41668611341). Next: T-033, T-052; T-035 unblocked (needs Lucas's secrets).
- 2026-11-03 | Claude Opus 5.5 (Mac): added T-053 (M4, deps T-030/T-049): daemon supervisor emits EXPIRE_SWEEP via sweep_candidates; dep of T-036. Totals 39/53.
  Docs only. Next: T-031, T-033, T-052; T-050/T-053 after T-049.
- 2026-11-03 | Claude Opus 5.5 (Mac): T-030 done (39/52). core sweep_candidates; chaos_skew.rs: ±30 min skew (no premature sweep, audited idempotent sweeps), 4-clone CAS contention converges.
  CI green (run 41648065503). Gap: nothing emits EXPIRE_SWEEP in prod (follow-up after T-049). Next: T-031, T-033, T-052.
- 2026-11-03 | Claude Opus 5.5 (Mac): Lucas Pan approved T-049 scope widening into core::pattern (fast-negative pre-check in conflicts_raw; zero false negatives must hold).
  Backlog scope column updated; T-049 lock/status untouched. Next: T-049 (Windows) re-measure; T-030, T-052.
