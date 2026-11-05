# Agent Operations Board

<!--
Anti-Bloat Constraints:
- Telegraphic, high-density facts only. Zero conversational pleasantries or prose.
- Strict sliding window: Retain ONLY the last 3–5 entries. Any agent appending an entry must prune entries older than the 5th to keep token overhead below 300 tokens permanently.
- Maximum 5 lines per handoff entry.
-->

## Active Lock
- (none)

## Team Roster & Roles
- Gemini 4 Argon: Lead Architect (ADRs, cross-module scaffolding, spec design)
- Gemini 3.8 High: Deep reviewer & complex platform logic
- Mistral Large 4: Terminal implementer via Vibe CLI (fast iteration, git ops)
- Claude Opus 5.5: Invariant modeling, state machines, property-based testing (proptest)

## Host & Toolchain Invariants
- Windows Host: Missing local `link.exe` / Windows SDK. `cargo check` and `cargo fmt` work locally; full test execution links on the GitHub Actions 3-OS CI matrix.
- Mac Host: full local gates work (run long jobs under `caffeinate -i`: idle sleep stretches unattended runs by minutes); `git config user.email` is a placeholder — commit with `-c user.email=109758310+Aviation-0@users.noreply.github.com`.
- Git Identity: Commits must strictly use `Lucas Pan <109758310+Aviation-0@users.noreply.github.com>`. Zero co-author trailers.

## Recent Handoff Log (Sliding window: max 5 entries)
- 2026-11-05 | Claude Opus 5.5 (Mac): T-058 done (54/58). CLI claim/release/heartbeat re-issue events dropped below the snapshot watermark (was a silent no-op when the local clock trailed the log).
  CI green (run 42027563161). Lock released. Next: T-036, T-045 (human sessions), then T-037/T-038.
- 2026-11-05 | Claude Opus 5.5 (Mac): T-057 done (53/57). CLI claim 15 -> 8 git processes (tree_blobs, mktrees batch, refs_read, config fast path). CAS overhead median 219 -> 97 ms vs GitHub; all 6 rows met.
  CI green (run 41974576911). Lock released. Next: T-036, T-045 (human sessions), then T-037/T-038.
- 2026-11-04 | Claude Opus 5.5 (Mac): T-056 done (52/56). Bench CAS row = overhead over paired git floor (ADR-0012). Full scale vs GitHub: run 2 all 6 met (overhead p95 312 ms), run 1 miss (920 ms, noisy net).
  CI green (run 41892413976). Lock released. Next: T-036, T-045 (human sessions), then T-037/T-038.
- 2026-11-04 | Claude Opus 5.5 (Mac): T-055 done (51/55). fetch_ref = one git fetch (glob + --prune); GitHub CAS round p95 6.96 -> 4.90 s. Git floor (fetch+push) p95 2.70 s here.
  ADR-0012 Proposed (row = overhead over git floor, p95 < 500 ms). CI green (run 41868627070). Lock released. Next: ADR-0012 decision, T-036, T-045.
- 2026-11-04 | Claude Opus 5.5 (Mac): T-051 done (50/55). Full-scale bench vs GitHub: 5/6 met (spawn 207 ms @2 GB, claim p95 20-34 ms to 10k events, 0 overwrites vs 19-20/200, 0 index.lock, recovery <=200 ms).
  CAS round p95 ~6.96 s misses 2 s (ls-remote+fetch per sync) -> T-055. Lock released. Next: T-055, T-036 (humans), T-045.
