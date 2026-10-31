# Agent Operations Board

<!--
Anti-Bloat Constraints:
- Telegraphic, high-density facts only. Zero conversational pleasantries or prose.
- Strict sliding window: Retain ONLY the last 3–5 entries. Any agent appending an entry must prune entries older than the 5th to keep token overhead below 300 tokens permanently.
- Maximum 5 lines per handoff entry.
-->

## Active Lock
- Model: Claude Opus 5.5 (Windows)
- Task: T-018 (meshtree daemon supervisor + two-client claim race)
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
- 2026-10-31 | Claude Opus 5.5 (Windows): T-044 done (18/45). git children run LC_ALL=C; update_ref CasRejected only if ref moved; retry-exhaustion (5 pushes → MT-003) + local CAS retry tests.
  CI green (run 41222922141). Windows host: clippy now runs locally via GNU toolchain (check-only). Next: T-018.
- 2026-10-31 | Claude Opus 5.5 (Windows): T-017 done (17/45). platform::wal CRC-framed fsynced journal, torn-tail truncation, mid-log corruption refused, pure replay → WalState + anomalies (MT-004).
  CI green (run 41222685141). Next: T-044 (gates T-018).
- 2026-10-31 | Claude Opus 5.5 (Mac): audit T-003–T-015 (no regressions, 6 debts) → approved restructure T-043–T-045, ADR-0004. T-016 done (16/45): platform::lock flock/named-mutex guard + pid/boot_id record, MT-011 adoption.
  CI green (run 41215529894). Open: OS machine-id derivation unowned; ADR-0003 acceptance; §17 allow-list ADR blocks T-019. Next: T-017.
- 2026-10-30 | Claude Opus 5.5: T-015 complete (15/42). platform::ipc NDJSON over UDS (0600 + parent check) / named pipe (owner+SYSTEM DACL, no remote, first-instance); MT-010 when unreachable. CI green (run 41155843711).
  Needs human review: ADR-0003, §6-key ADR candidates (§13/§17/§11), unowned coord tasks/ registry. Next: T-016.
- 2026-10-30 | Claude Opus 5.5: T-014 complete — M1 done. CLI claim/release/sync + spawn --claim over CoordStore (origin if configured); MT-001 exit 20 names holder; all-or-nothing batches; abort releases claims.
  sync flushes deferred pushes + revokes claims lost to remote winners. CI green (run 41155551527). Open: coord tasks/<id>.json registry unassigned. Next: T-015.
