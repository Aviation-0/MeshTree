//! Exhaustive conflict-rule table: zero false negatives (`SPEC.md` §7, §18; ADR-0003).
//!
//! Every normalized pattern over a small alphabet (depth ≤ 3) is paired with
//! every other. Ground truth is computed by brute force over all concrete
//! paths of depth ≤ 4 drawn from a segment alphabet that can satisfy every
//! glob in the pattern alphabet. Whenever some concrete path is covered by
//! both claims, `conflicts` must say so. The literal §7 heuristic
//! (canonical-literal match or directory prefix) must also imply `conflicts`.

use meshtree_core::pattern::{conflicts, normalize};

const PATTERN_SEGMENTS: [&str; 7] = ["a", "b", "ab", "*", "a*", "*b", "**"];
const PATH_SEGMENTS: [&str; 5] = ["a", "b", "ab", "ba", "x"];
const PROBE: &str = "\u{1}";

fn sequences(alphabet: &[&'static str], max_depth: usize) -> Vec<Vec<&'static str>> {
    let mut out: Vec<Vec<&'static str>> = Vec::new();
    let mut frontier: Vec<Vec<&'static str>> = vec![Vec::new()];
    for _ in 0..max_depth {
        let mut next = Vec::new();
        for prefix in &frontier {
            for seg in alphabet {
                let mut seq = prefix.clone();
                seq.push(seg);
                next.push(seq);
            }
        }
        out.extend(next.iter().cloned());
        frontier = next;
    }
    out
}

/// Oracle: single-segment glob match, `*` = any run of chars (case-insensitive).
fn segment_matches(glob: &str, text: &str) -> bool {
    fn go(g: &[u8], t: &[u8]) -> bool {
        match g.split_first() {
            None => t.is_empty(),
            Some((b'*', rest)) => (0..=t.len()).any(|k| go(rest, &t[k..])),
            Some((c, rest)) => t
                .split_first()
                .is_some_and(|(d, t_rest)| c.eq_ignore_ascii_case(d) && go(rest, t_rest)),
        }
    }
    go(glob.as_bytes(), text.as_bytes())
}

/// Oracle: whole-path glob match, `**` = any number of segments.
fn path_matches(pattern: &[&str], path: &[&str]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        Some((&"**", rest)) => (0..=path.len()).any(|k| path_matches(rest, &path[k..])),
        Some((seg, rest)) => path
            .split_first()
            .is_some_and(|(p, p_rest)| segment_matches(seg, p) && path_matches(rest, p_rest)),
    }
}

/// Oracle: a claim covers what it matches and everything below it.
fn covers(pattern: &[&str], path: &[&str]) -> bool {
    (1..=path.len()).any(|k| path_matches(pattern, &path[..k]))
}

/// The literal §7 heuristic.
fn spec_rule(a: &[&str], b: &[&str]) -> bool {
    let literal = |p: &[&str]| -> Vec<String> {
        p.iter()
            .map(|s| {
                if *s == "**" {
                    PROBE.to_string()
                } else {
                    s.replace('*', PROBE)
                }
            })
            .collect()
    };
    let (la, lb) = (literal(a), literal(b));
    let la: Vec<&str> = la.iter().map(String::as_str).collect();
    let lb: Vec<&str> = lb.iter().map(String::as_str).collect();
    let dir_prefix = |x: &[&str], y: &[&str]| x.len() <= y.len() && x == &y[..x.len()];
    path_matches(a, &lb) || path_matches(b, &la) || dir_prefix(a, b) || dir_prefix(b, a)
}

#[test]
fn pattern_conflict_exhaustive_case_table() {
    // Normalized patterns only (no `**/**`).
    let patterns: Vec<Vec<&str>> = sequences(&PATTERN_SEGMENTS, 3)
        .into_iter()
        .filter(|p| !p.windows(2).any(|w| w[0] == "**" && w[1] == "**"))
        .collect();
    let paths = sequences(&PATH_SEGMENTS, 4);
    let words = paths.len().div_ceil(64);

    let coverage: Vec<Vec<u64>> = patterns
        .iter()
        .map(|p| {
            let mut bits = vec![0u64; words];
            for (i, path) in paths.iter().enumerate() {
                if covers(p, path) {
                    bits[i / 64] |= 1 << (i % 64);
                }
            }
            bits
        })
        .collect();

    let joined: Vec<String> = patterns.iter().map(|p| p.join("/")).collect();
    for s in &joined {
        assert_eq!(
            normalize(s).as_deref(),
            Ok(s.as_str()),
            "table pattern {s} is normalized"
        );
    }

    let (mut pairs, mut positives, mut ours_true) = (0usize, 0usize, 0usize);
    for (i, a) in joined.iter().enumerate() {
        for (j, b) in joined.iter().enumerate() {
            pairs += 1;
            let ours = conflicts(a, b);
            ours_true += usize::from(ours);
            let ground = coverage[i]
                .iter()
                .zip(&coverage[j])
                .any(|(x, y)| x & y != 0);
            positives += usize::from(ground);
            assert!(!ground || ours, "false negative: {a} vs {b}");
            assert!(
                !spec_rule(&patterns[i], &patterns[j]) || ours,
                "misses SPEC §7 rule: {a} vs {b}"
            );
            if i == j {
                assert!(ours, "reflexive: {a}");
            }
        }
    }
    assert_eq!(pairs, joined.len() * joined.len());
    // The rule is useful, not "everything conflicts".
    assert!(ours_true < pairs, "{ours_true} of {pairs} pairs conflict");
    assert!(
        pairs - ours_true > pairs / 10,
        "too few non-conflicting pairs: {}",
        pairs - ours_true
    );
    assert!(positives > 0);
}
