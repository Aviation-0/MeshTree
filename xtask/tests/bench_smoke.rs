//! The §18 benchmark suite at smoke scale (`SPEC.md` §18, §22).

use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use xtask::{run, Options};

/// `target/<profile>/meshtree`, next to this test's `deps/` directory;
/// built with cargo if `cargo test -p xtask` ran alone.
fn meshtree_bin() -> PathBuf {
    let exe = std::env::current_exe().expect("test exe");
    let profile_dir = exe
        .parent()
        .and_then(|deps| deps.parent())
        .expect("target/<profile>/deps/<test>")
        .to_path_buf();
    let bin = profile_dir.join(format!("meshtree{}", std::env::consts::EXE_SUFFIX));
    if !bin.exists() {
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let status = Command::new(cargo)
            .args(["build", "-p", "meshtree"])
            .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/.."))
            .status()
            .expect("run cargo build");
        assert!(status.success(), "cargo build -p meshtree failed");
    }
    assert!(bin.exists(), "{} not built", bin.display());
    bin
}

/// On GitHub Actions, a workflow-command annotation (written past the test
/// harness's output capture, which only intercepts `print!`).
fn annotate(level: &str, title: &str, text: &str) {
    if std::env::var_os("GITHUB_ACTIONS").is_none() {
        return;
    }
    let text = text
        .replace('%', "%25")
        .replace('\r', "")
        .replace('\n', "%0A");
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "::{level} title={title}::{text}");
    let _ = out.flush();
}

#[test]
fn xtask_bench_smoke_verifies_six_scenarios() {
    let opts = Options::smoke(meshtree_bin());
    let report = match run(&opts) {
        Ok(report) => report,
        Err(err) => {
            annotate("error", "bench smoke failed", &err);
            panic!("{err}");
        }
    };
    let markdown = report.to_markdown("smoke (CI sizes)");
    annotate(
        "notice",
        &format!(
            "bench smoke {}-{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        ),
        &report
            .rows
            .iter()
            .map(|r| {
                format!(
                    "{} | {} | {} | met={}",
                    r.scenario, r.target, r.measured, r.pass
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
    );

    let names: Vec<&str> = report.rows.iter().map(|r| r.scenario).collect();
    assert_eq!(
        names,
        [
            "Spawn with shared `node_modules`",
            "Claim acquire (local)",
            "Cross-machine claim CAS round",
            "Two agents, one repo",
            "`index.lock` collisions",
            "Crash recovery",
        ]
    );
    for row in &report.rows {
        assert!(!row.measured.is_empty() && !row.notes.is_empty(), "{row:?}");
    }
    // Correctness rows hold at any scale; latency rows are reported, not
    // asserted (shared CI runners are noisy).
    assert!(
        report.rows[3].pass,
        "silent overwrite with MeshTree: {:?}",
        report.rows[3]
    );
    assert!(report.rows[3].measured.starts_with("0 with MeshTree"));
    assert!(
        report.rows[4].pass,
        "index.lock collision through the queue: {:?}",
        report.rows[4]
    );
    // Scenario 3 (ADR-0012): the paired git floor, the claim round, the
    // overhead and B's visibility are all recorded; the network-call bound is
    // deterministic, so it is asserted (the latency budget is not, on CI).
    let cas = &report.rows[2];
    for name in [
        "floor_p95_ms",
        "claim_p95_ms",
        "visibility_p95_ms",
        "max_fetches",
        "max_pushes",
    ] {
        let value = cas
            .metric(name)
            .unwrap_or_else(|| panic!("{name}: {cas:?}"));
        assert!(value > 0.0, "{name} = {value}: {cas:?}");
    }
    assert!(cas.metric("overhead_p95_ms").is_some(), "{cas:?}");
    assert_eq!(
        cas.metric("max_fetches"),
        Some(1.0),
        "one fetch per claim: {cas:?}"
    );
    assert_eq!(
        cas.metric("max_pushes"),
        Some(1.0),
        "one push per claim: {cas:?}"
    );
    assert!(cas.target.contains("500 ms"), "{cas:?}");
    assert!(
        report.rows[0].notes.contains("node_modules linked: true"),
        "{:?}",
        report.rows[0]
    );

    assert_eq!(
        markdown.lines().filter(|l| l.starts_with("| ")).count(),
        7,
        "header and six rows:\n{markdown}"
    );
    assert!(markdown.starts_with("# MeshTree Benchmarks\n"));
}
