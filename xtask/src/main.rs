//! `cargo run --release -p xtask -- bench [options]`: runs the `SPEC.md`
//! §18 benchmark suite and writes `docs/benchmarks.md`.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use xtask::{run, Options};

const USAGE: &str = "usage: cargo run --release -p xtask -- bench [--smoke] \
[--meshtree <path>] [--remote <url>] [--node-modules-mb <n>] [--out <path>]

  --smoke              small sizes (CI); default is the SPEC.md §18 scale
  --meshtree <path>    binary under test (default: build target/release/meshtree)
  --remote <url>       remote for the CAS scenario; a THROWAWAY repository
                       (main and refs/meshtree/coord are pushed to it)
  --node-modules-mb n  size of the generated node_modules (default 2048)
  --out <path>         report file (default docs/benchmarks.md)";

fn main() -> ExitCode {
    match real_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("xtask: {err}");
            ExitCode::FAILURE
        }
    }
}

fn real_main() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() != Some("bench") {
        return Err(USAGE.to_string());
    }
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives in the workspace")
        .to_path_buf();
    let (mut smoke, mut meshtree, mut remote, mut mb) = (false, None, None, None);
    let mut out = workspace.join("docs").join("benchmarks.md");
    while let Some(arg) = args.next() {
        let mut value = || {
            args.next()
                .ok_or_else(|| format!("{arg} needs a value\n{USAGE}"))
        };
        match arg.as_str() {
            "--smoke" => smoke = true,
            "--meshtree" => meshtree = Some(PathBuf::from(value()?)),
            "--remote" => remote = Some(value()?),
            "--node-modules-mb" => mb = Some(value()?.parse::<u64>().map_err(|e| e.to_string())?),
            "--out" => out = PathBuf::from(value()?),
            _ => return Err(format!("unknown argument {arg}\n{USAGE}")),
        }
    }
    let meshtree = match meshtree {
        Some(path) => path,
        None => build_release(&workspace)?,
    };
    let mut opts = if smoke {
        Options::smoke(meshtree)
    } else {
        Options::full(meshtree)
    };
    opts.remote = remote;
    if let Some(mb) = mb {
        opts.node_modules_mb = mb;
    }
    eprintln!(
        "xtask: running the §18 benchmark suite ({}) ...",
        scale(smoke)
    );
    let report = run(&opts)?;
    let markdown = report.to_markdown(scale(smoke));
    std::fs::write(&out, &markdown).map_err(|e| format!("{}: {e}", out.display()))?;
    print!("{markdown}");
    eprintln!("xtask: wrote {}", out.display());
    Ok(())
}

fn scale(smoke: bool) -> &'static str {
    if smoke {
        "smoke (CI sizes)"
    } else {
        "full (SPEC.md §18 sizes)"
    }
}

/// `cargo build --release -p meshtree`; returns the binary path.
fn build_release(workspace: &Path) -> Result<PathBuf, String> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(cargo)
        .args(["build", "--release", "-p", "meshtree"])
        .current_dir(workspace)
        .status()
        .map_err(|e| format!("cargo build: {e}"))?;
    if !status.success() {
        return Err("cargo build --release -p meshtree failed".into());
    }
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace.join("target"));
    Ok(target
        .join("release")
        .join(format!("meshtree{}", std::env::consts::EXE_SUFFIX)))
}
