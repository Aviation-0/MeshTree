# Agent Operations Board

<!--
Anti-Bloat Constraints:
- Telegraphic, high-density facts only. Zero conversational pleasantries or prose.
- Strict sliding window: Retain ONLY the last 3–5 entries. Any agent appending an entry must prune entries older than the 5th to keep token overhead below 300 tokens permanently.
- Maximum 5 lines per handoff entry.
-->

## Active Lock
- Model: Claude Opus 5.5 (Mac)
- Task: T-042 (MCP directory listing manifest)
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
- - 2026-11-03 | Claude Opus 5.5 (Windows): T-053 done. Supervisor emits one EXPIRE_SWEEP per expired lease (any machine), via WAL + bg commit; idempotent. wal: Events entries need no local task (2 lines, outside fence).
    CI green (run 41745062452). Next: T-051 (Mac), T-036, T-045 (human).
- - 2026-11-03 | Claude Opus 5.5 (Windows): T-050 done. Watermark snapshot compaction (core compact/replay_from, CoordStore larger-W merge, daemon compacts + re-issues late events). Claim p95 flat: 1-11 ms at 100/2,000/10,000 lifetime events, all OSes.
    CI green (run 41742438911). Open: CLI-only claim lift above W. Next: T-053, T-051 (Mac).
- - 2026-11-03 | Claude Opus 5.5 (Windows): T-049 done. In-memory daemon claims, WAL events (single fsync), 25 ms background commit, serialized coord writes, core segment-wise fast negative. Claim p95 @100 ev 4.6/26.2/25.1 ms (L/M/W).
    CI green (run 41737248886). 2,000-ev row moved to T-050. Unblocked: T-050, T-053.
- - 2026-11-03 | Claude Opus 5.5 (Windows): T-049 IN PROGRESS (lock held). Core segment-wise fast negative, serialized coord writes, single-fsync WAL batch; CI green. Claim p95 @2000 ev 60/118/92 ms (L/M/W) still > 50.
    Remaining cost = full-log background flush (T-050). Needs Lucas: move 2,000-ev row to T-050 exit, or more bench samples.
- 2026-11-03 | Claude Opus 5.5 (Mac): T-041 done (45/54). MIT OR Apache-2.0, crates.io metadata + READMEs, versioned path deps, CI publish dry-run job.
  CI green (run 41701931244). Next: T-042.
