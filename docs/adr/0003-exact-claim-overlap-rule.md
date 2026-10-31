# ADR-0003: Exact Glob-Intersection Test for Claim Conflicts

- **Status:** Proposed (implemented in `T-009`; awaiting human acceptance — 2026-10-31 audit)
- **Date:** 2026-10-30
- **Author:** Claude Opus 5.5 (Agent); flagged for review by Lucas Pan
- **Authority:** `SPEC.md` §7 (path patterns & conflict rule), §17, §18; `AGENTS.md` Standing Order #1

## Context

`SPEC.md` §7 defines the conflict rule as: two claims conflict if either pattern matches the other's *canonical literal* (wildcards replaced by a probe segment), or either is a directory-prefix of the other. It then requires the rule to be a conservative over-approximation with **zero false negatives**, and §18 asserts that on an exhaustive case table.

The heuristic alone does not satisfy that requirement. `src/a*` and `src/*b` both cover `src/ab`, yet neither matches the other's canonical literal (`src/a<probe>` vs `src/<probe>b`) and neither is a prefix of the other. The same holds for `**/x` vs `a/**`.

## Decision

`meshtree-core::pattern::conflicts` returns `true` iff some concrete path is covered by both claims, where a claim covers what its glob matches *and everything below it* (the §7 directory-prefix clause). It is computed exactly by dynamic programming over segments (`**`) and characters within segments (`*`), ASCII case-insensitively.

This is a strict superset of the §7 heuristic: every pair the heuristic flags is flagged, plus the pairs it misses. False positives remain acceptable per §7; false negatives are eliminated.

Related normalization choices in the same module (all within §7/§17's text):

- Comparison is ASCII case-insensitive, so claims stay conservative on case-insensitive filesystems (Windows, default macOS).
- Patterns whose first segment can name `.meshtree` (e.g. `.meshtree/**`, `.mesh*`) are rejected (§17). Wildcard-led patterns (`**`, `*/x`) remain legal; `.meshtree/` is implicitly outside what they claim.

## Consequences

- `pattern_conflict_exhaustive_case_table` checks both properties against brute force: no false negatives, and the §7 heuristic always implies a conflict.
- `SPEC.md` §7 text is unchanged; this ADR records that the implementation is the exact intersection, which satisfies §7's "conservative over-approximation" requirement.
