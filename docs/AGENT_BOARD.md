# Agent Operations Board

<!--
Anti-Bloat Constraints:
- Telegraphic, high-density facts only. Zero conversational pleasantries or prose.
- Strict sliding window: Retain ONLY the last 3–5 entries. Any agent appending an entry must prune entries older than the 5th to keep token overhead below 300 tokens permanently.
- Maximum 5 lines per handoff entry.
-->

## Active Lock
- Model: Claude Opus 5.5 (Mac)
- Task: T-016
- Status: doing (2026-10-31)

## Team Roster & Roles
- Gemini 4 Argon: Lead Architect (ADRs, cross-module scaffolding, spec design)
- Gemini 3.8 High: Deep reviewer & complex platform logic
- Mistral Large 4: Terminal implementer via Vibe CLI (fast iteration, git ops)
- Claude Opus 5.5: Invariant modeling, state machines, property-based testing (proptest)

## Host & Toolchain Invariants
- Windows Host: Missing local `link.exe` / Windows SDK. `cargo check` and `cargo fmt` work locally; full test execution links on the GitHub Actions 3-OS CI matrix.
- Git Identity: Commits must strictly use `Lucas Pan <109758310+Aviation-0@users.noreply.github.com>`. Zero co-author trailers.

## Recent Handoff Log (Sliding window: max 5 entries)
- 2026-10-30 | Claude Opus 5.5: T-015 complete (15/42). platform::ipc NDJSON over UDS (0600 + parent check) / named pipe (owner+SYSTEM DACL, no remote, first-instance); MT-010 when unreachable. CI green (run 41155843711).
  Needs human review: ADR-0003, §6-key ADR candidates (§13/§17/§11), unowned coord tasks/ registry. Next: T-016.
- 2026-10-30 | Claude Opus 5.5: T-014 complete — M1 done. CLI claim/release/sync + spawn --claim over CoordStore (origin if configured); MT-001 exit 20 names holder; all-or-nothing batches; abort releases claims.
  sync flushes deferred pushes + revokes claims lost to remote winners. CI green (run 41155551527). Open: coord tasks/<id>.json registry unassigned. Next: T-015.
- 2026-10-30 | Claude Opus 5.5: T-013 complete. platform::coord::CoordStore::sync (fetch → union → replay → canonical tree → local CAS → push --force-with-lease; ×5 jittered retry; MT-003; offline ⇒ deferred push).
  Fixed: concurrent pushes lose at remote ref lock ("[remote rejected] failed to update ref") — now classified as lost race. CI green (run 41155358409). Next: T-014.
- 2026-10-30 | Claude Opus 5.5: T-012 complete. Lease::expires_at + skew test (no premature expiry within relative skew ≤ grace). lease::coord codec: leases/<sha1>.json, log/<ULID>.json, canonical JSON, std-only SHA-1.
  serde_json now a core dep. CI green (run 41154798925). Next: T-013.
- 2026-10-30 | Claude Opus 5.5: T-011 complete. gitops: hash_object/mktree/commit_tree (fixed identity)/ls_tree/cat_blob/fetch_ref/push_ref_cas (--force-with-lease; lost race → RemoteCasRejected = MT-003).
  Two clones + bare file:// remote test green on all 3 OSes (run 41154570699). Next: T-012.
