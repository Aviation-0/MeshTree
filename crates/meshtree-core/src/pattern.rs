//! Claim path-pattern normalization and the conservative conflict rule.
//!
//! Authoritative specification: `SPEC.md` §7 (path patterns & conflict rule),
//! §17 (`.meshtree/` excluded from write-claims), §18; `ADR-0003`.
//!
//! Patterns use forward slashes, `**` = any number of segments (including
//! none), `*` = any run of characters within one segment. A claim also covers
//! everything below what it names (§7 "directory-prefix"), so `src/auth`
//! conflicts with `src/auth/login.rs`.
//!
//! [`conflicts`] returns `true` whenever some concrete path could be covered by
//! both patterns. That is a superset of the §7 heuristic (canonical-literal
//! match or directory prefix), which on its own misses pairs such as `a*` /
//! `*b` (both cover `ab`). §7 requires zero false negatives, so the exact
//! intersection test is used (`ADR-0003`). Comparison is ASCII
//! case-insensitive, conservatively covering case-insensitive filesystems.

use std::fmt;

/// Directory holding MeshTree state; never claimable (§17).
const MESHTREE_DIR: &str = ".meshtree";

/// A rejected claim pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatternError {
    /// Offending input.
    pub pattern: String,
    /// Why it was rejected.
    pub reason: &'static str,
}

impl fmt::Display for PatternError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid claim pattern '{}': {}",
            self.pattern, self.reason
        )
    }
}

impl std::error::Error for PatternError {}

/// Normalizes a claim pattern (§7): forward slashes, no `./` or empty
/// segments, no trailing slash, runs of `*` collapsed (`a***b` → `a*b`,
/// `**/**` → `**`).
///
/// Rejects empty, absolute (`/x`, `C:/x`), and parent-traversing (`..`)
/// patterns, and patterns whose first segment names `.meshtree` (§17).
/// Wildcard-led patterns such as `**` stay legal; `.meshtree/` is implicitly
/// excluded from what they cover.
pub fn normalize(raw: &str) -> Result<String, PatternError> {
    let err = |reason| PatternError {
        pattern: raw.to_string(),
        reason,
    };
    let unified = raw.trim().replace('\\', "/");
    if unified.starts_with('/') {
        return Err(err("absolute paths are not allowed"));
    }
    let mut segments: Vec<String> = Vec::new();
    for segment in unified.split('/') {
        match segment {
            "" | "." => continue,
            ".." => return Err(err("parent traversal ('..') is not allowed")),
            _ => {}
        }
        let collapsed = collapse_stars(segment);
        if collapsed == "**" && segments.last().is_some_and(|s| s == "**") {
            continue;
        }
        segments.push(collapsed);
    }
    let Some(first) = segments.first() else {
        return Err(err("pattern is empty"));
    };
    if first.contains(':') {
        return Err(err("absolute paths are not allowed"));
    }
    if first != "*" && first != "**" && segment_intersects(first, MESHTREE_DIR) {
        return Err(err(".meshtree/ is reserved for MeshTree state"));
    }
    Ok(segments.join("/"))
}

/// `**` stays `**`; any other run of `*` becomes a single `*`.
fn collapse_stars(segment: &str) -> String {
    if segment.chars().all(|c| c == '*') {
        let stars = if segment.len() >= 2 { "**" } else { "*" };
        return stars.to_string();
    }
    let mut out = String::with_capacity(segment.len());
    for c in segment.chars() {
        if !(c == '*' && out.ends_with('*')) {
            out.push(c);
        }
    }
    out
}

/// `true` iff some concrete path is covered by both normalized patterns
/// (including directory-prefix coverage). Symmetric; reflexive.
pub fn conflicts(a: &str, b: &str) -> bool {
    let a = with_subtree(a);
    let b = with_subtree(b);
    let mut memo = vec![vec![None; b.len() + 1]; a.len() + 1];
    path_intersects(&a, &b, 0, 0, &mut memo)
}

/// [`conflicts`] over raw patterns (e.g. read back from a remote event log),
/// usable directly as the [`crate::lease::replay`] predicate. A pattern that
/// fails [`normalize`] conservatively conflicts with everything.
pub fn conflicts_raw(a: &str, b: &str) -> bool {
    match (normalize(a), normalize(b)) {
        (Ok(a), Ok(b)) => conflicts(&a, &b),
        _ => true,
    }
}

/// Lower-cased segments plus a trailing `**`: a claim covers its subtree.
fn with_subtree(pattern: &str) -> Vec<String> {
    let mut segments: Vec<String> = pattern
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())
        .collect();
    segments.push("**".to_string());
    segments
}

/// Segment-level intersection; `**` may absorb any number of the other
/// pattern's segments (including its `**`).
fn path_intersects(
    a: &[String],
    b: &[String],
    i: usize,
    j: usize,
    memo: &mut [Vec<Option<bool>>],
) -> bool {
    if let Some(known) = memo[i][j] {
        return known;
    }
    let (a_end, b_end) = (i == a.len(), j == b.len());
    let a_star = !a_end && a[i] == "**";
    let b_star = !b_end && b[j] == "**";
    let result = (a_end && b_end)
        || (a_star
            && (path_intersects(a, b, i + 1, j, memo)
                || (!b_end && path_intersects(a, b, i, j + 1, memo))))
        || (b_star
            && (path_intersects(a, b, i, j + 1, memo)
                || (!a_end && path_intersects(a, b, i + 1, j, memo))))
        || (!a_end
            && !b_end
            && !a_star
            && !b_star
            && segment_intersects(&a[i], &b[j])
            && path_intersects(a, b, i + 1, j + 1, memo));
    memo[i][j] = Some(result);
    result
}

/// `true` iff some single segment matches both segment globs (`*` only).
fn segment_intersects(a: &str, b: &str) -> bool {
    let a: Vec<char> = a.to_ascii_lowercase().chars().collect();
    let b: Vec<char> = b.to_ascii_lowercase().chars().collect();
    let mut memo = vec![vec![None; b.len() + 1]; a.len() + 1];
    chars_intersect(&a, &b, 0, 0, &mut memo)
}

/// Character-level intersection; `*` may absorb any number of the other
/// glob's characters (including its `*`).
fn chars_intersect(
    a: &[char],
    b: &[char],
    i: usize,
    j: usize,
    memo: &mut [Vec<Option<bool>>],
) -> bool {
    if let Some(known) = memo[i][j] {
        return known;
    }
    let (a_end, b_end) = (i == a.len(), j == b.len());
    let a_star = !a_end && a[i] == '*';
    let b_star = !b_end && b[j] == '*';
    let result = (a_end && b_end)
        || (a_star
            && (chars_intersect(a, b, i + 1, j, memo)
                || (!b_end && chars_intersect(a, b, i, j + 1, memo))))
        || (b_star
            && (chars_intersect(a, b, i, j + 1, memo)
                || (!a_end && chars_intersect(a, b, i + 1, j, memo))))
        || (!a_end
            && !b_end
            && !a_star
            && !b_star
            && a[i] == b[j]
            && chars_intersect(a, b, i + 1, j + 1, memo));
    memo[i][j] = Some(result);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_canonicalizes_and_rejects() {
        for (raw, want) in [
            ("src/auth/**", "src/auth/**"),
            ("./src//auth/", "src/auth"),
            ("src\\auth\\**", "src/auth/**"),
            ("**/**/x", "**/x"),
            ("a***b/***", "a*b/**"),
            ("  docs  ", "docs"),
            ("**", "**"),
            ("*/x", "*/x"),
            ("src/.meshtree", "src/.meshtree"),
        ] {
            assert_eq!(normalize(raw).as_deref(), Ok(want), "{raw:?}");
        }
        for raw in [
            "",
            "./",
            "/etc/passwd",
            "\\\\server\\share",
            "C:/x",
            "../x",
            "a/../b",
            ".meshtree",
            ".meshtree/tasks/**",
            "./.meshtree",
            ".MeshTree/x",
            ".mesh*",
        ] {
            assert!(normalize(raw).is_err(), "must reject {raw:?}");
        }
    }

    #[test]
    fn conflicts_spec_examples() {
        for (a, b, want) in [
            ("src/auth/**", "src/auth/**", true),
            ("src/auth/**", "src/auth/login.rs", true),
            ("src/auth", "src/auth/login.rs", true),
            ("src/**", "src/auth/**", true),
            ("src/auth/**", "tests/auth/**", false),
            ("src/*.rs", "src/main.rs", true),
            ("src/*.rs", "src/lib/main.rs", false),
            ("src/*.rs", "src/*.toml", false),
            ("src/a*", "src/*b", true),
            ("**/mod.rs", "src/auth/mod.rs", true),
            // A claim covers its subtree, so `src/auth/lib.rs` could hold `mod.rs`.
            ("**/mod.rs", "src/auth/lib.rs", true),
            ("src/**/mod.rs", "tests/x", false),
            ("SRC/Auth", "src/auth/x", true),
            ("docs", "documentation", false),
        ] {
            assert_eq!(conflicts(a, b), want, "{a} vs {b}");
            assert_eq!(conflicts(b, a), want, "{b} vs {a}");
        }
    }
}
