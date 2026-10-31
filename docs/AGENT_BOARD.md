# Agent Operations Board

<!--
Anti-Bloat Constraints:
- Telegraphic, high-density facts only. Zero conversational pleasantries or prose.
- Strict sliding window: Retain ONLY the last 3–5 entries. Any agent appending an entry must prune entries older than the 5th to keep token overhead below 300 tokens permanently.
- Maximum 5 lines per handoff entry.
-->

## Active Lock
- Model: Claude Opus 5.5
- Task: T-013
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
- 2026-10-30 | Claude Opus 5.5: T-012 complete. Lease::expires_at + skew test (no premature expiry within relative skew ≤ grace). lease::coord codec: leases/<sha1>.json, log/<ULID>.json, canonical JSON, std-only SHA-1.
  serde_json now a core dep. CI green (run 41154798925). Next: T-013.
- 2026-10-30 | Claude Opus 5.5: T-011 complete. gitops: hash_object/mktree/commit_tree (fixed identity)/ls_tree/cat_blob/fetch_ref/push_ref_cas (--force-with-lease; lost race → RemoteCasRejected = MT-003).
  Two clones + bare file:// remote test green on all 3 OSes (run 41154570699). Next: T-012.
- 2026-10-30 | Claude Opus 5.5: T-010 complete — M0 done. walking_skeleton_init_spawn_status_abort asserts task.json at each step; README Quickstart + §14 exit codes. CI green (run 41154334602). Next: T-011.
- 2026-10-30 | Claude Opus 5.5: T-009 complete. core::pattern normalize() + conflicts() = exact glob intersection w/ subtree coverage, case-insensitive; .meshtree-led claims rejected.
  ADR-0003 (needs human review): SPEC §7 heuristic misses a*/*b, so exact superset used. Exhaustive table vs brute force: 0 false negatives. CI green (run 41154122988).
- 2026-10-30 | Claude Opus 5.5: T-008 complete. tests/lease_replay_props.rs: 256 random logs vs shuffled/duplicated/split-union copies → byte-identical lease table + MT-001 rejections; no cross-task conflicting leases.
  Extreme-TTL no-panic property. CI green (run 41153826336). Next: T-009.
