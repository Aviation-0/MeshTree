//! Release pipeline (`SPEC.md` §19, `T-035`): the `meshtree-mcp` npx shim
//! launches `meshtree mcp` over stdio, and the release configuration is wired
//! together (workflows, targets, versions, Scoop/winget templates).
//!
//! These tests need Node.js. On CI (`CI` is set; GitHub runners ship Node) a
//! missing Node is a failure; on a developer machine without it they skip
//! with a note.

mod common;

use common::{assert_exit, meshtree, TempDir};
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// `npm` / `npx` / `node` as a runnable command name (`.cmd` shims on Windows).
fn tool(name: &str) -> String {
    if cfg!(windows) && name != "node" {
        format!("{name}.cmd")
    } else {
        name.to_string()
    }
}

/// `true` if Node tooling is available; panics on CI if it is not.
fn have_node() -> bool {
    let ok = Command::new(tool("npm"))
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !ok {
        assert!(
            std::env::var_os("CI").is_none(),
            "Node.js/npm is required on CI"
        );
        eprintln!("skipping: Node.js/npm not installed");
    }
    ok
}

fn read_json_line(reader: &mut impl BufRead, deadline: Instant) -> Value {
    let mut line = String::new();
    while line.trim().is_empty() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for MCP output"
        );
        line.clear();
        let n = reader.read_line(&mut line).expect("read MCP stdout");
        assert!(n > 0, "MCP stdout closed before a response");
    }
    serde_json::from_str(&line).unwrap_or_else(|err| panic!("not JSON: {line:?}: {err}"))
}

#[test]
fn npx_shim_spawns_meshtree_mcp_stdio_handshake() {
    if !have_node() {
        return;
    }
    let scratch = TempDir::new();
    let pack_dir = scratch.path().join("pack");
    fs::create_dir_all(&pack_dir).unwrap();
    let pack = Command::new(tool("npm"))
        .args(["pack", "--silent", "--pack-destination"])
        .arg(&pack_dir)
        .current_dir(repo_root().join("packages/meshtree-mcp"))
        .env("npm_config_cache", scratch.path().join("npm-cache"))
        .output()
        .expect("npm pack");
    assert!(
        pack.status.success(),
        "npm pack: {}",
        String::from_utf8_lossy(&pack.stderr)
    );
    let tarball = pack_dir.join(String::from_utf8_lossy(&pack.stdout).trim());
    assert!(tarball.is_file(), "{}", tarball.display());

    let repo = TempDir::repo_with_commit();
    assert_exit(&meshtree(repo.path(), &["init"]), 0);

    // `npx` installs the packed shim (no registry: it has no dependencies)
    // and runs its bin; MESHTREE_BIN points it at the binary under test.
    let mut child = Command::new(tool("npx"))
        .arg("--yes")
        .arg(format!("--package={}", tarball.display()))
        .arg("meshtree-mcp")
        .current_dir(repo.path())
        .env("MESHTREE_BIN", env!("CARGO_BIN_EXE_meshtree"))
        .env("npm_config_cache", scratch.path().join("npm-cache"))
        .env("npm_config_update_notifier", "false")
        .env("npm_config_fund", "false")
        .env("npm_config_audit", "false")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn npx");
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let deadline = Instant::now() + Duration::from_secs(120);

    let initialize = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "npx-shim-test", "version": "0" },
        },
    });
    writeln!(stdin, "{initialize}").unwrap();
    stdin.flush().unwrap();
    let init = read_json_line(&mut stdout, deadline);
    assert_eq!(init["id"], 1, "{init}");
    assert_eq!(init["result"]["serverInfo"]["name"], "meshtree", "{init}");

    writeln!(
        stdin,
        "{}",
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })
    )
    .unwrap();
    writeln!(
        stdin,
        "{}",
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" })
    )
    .unwrap();
    stdin.flush().unwrap();
    let tools = read_json_line(&mut stdout, deadline);
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("{tools}"))
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert!(names.contains(&"claim_paths"), "{names:?}");

    // EOF on stdin ends the server; the shim exits with its status.
    drop(stdin);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("shim did not exit after stdin closed");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success(), "shim exit: {status:?}");
}

#[test]
fn release_workflow_is_wired() {
    let root = repo_root();
    let read = |rel: &str| {
        fs::read_to_string(root.join(rel)).unwrap_or_else(|err| panic!("read {rel}: {err}"))
    };

    // Every local reusable workflow release.yml calls exists (dist 0.34.0
    // writes `$/...`, which dist/regenerate-release-workflow.sh repairs).
    let release = read(".github/workflows/release.yml");
    assert!(
        !release.contains("uses: $/"),
        "run dist/regenerate-release-workflow.sh"
    );
    let local: Vec<&str> = release
        .lines()
        .filter_map(|l| l.trim().strip_prefix("uses: ./"))
        .collect();
    assert_eq!(
        local,
        [
            ".github/workflows/release-audit.yml",
            ".github/workflows/release-publish.yml"
        ]
    );
    for path in &local {
        assert!(root.join(path).is_file(), "{path} missing");
    }
    assert!(read(".github/workflows/release-audit.yml").contains("cargo audit"));

    // The SPEC §19 targets, and the shim version tracks the workspace.
    let cargo = read("Cargo.toml");
    assert!(cargo.contains(
        r#"targets = ["aarch64-apple-darwin", "x86_64-unknown-linux-gnu", "x86_64-pc-windows-msvc"]"#
    ));
    let package: Value = serde_json::from_str(&read("packages/meshtree-mcp/package.json")).unwrap();
    assert_eq!(package["name"], "meshtree-mcp");
    assert_eq!(package["license"], "MIT OR Apache-2.0");
    assert_eq!(package["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(package["bin"]["meshtree-mcp"], "bin/meshtree-mcp.js");

    // Each publish channel is gated on its own secret.
    let publish = read(".github/workflows/release-publish.yml");
    for secret in [
        "HOMEBREW_TAP_TOKEN",
        "NPM_TOKEN",
        "SCOOP_BUCKET_TOKEN",
        "WINGET_TOKEN",
    ] {
        assert!(publish.contains(&format!("secrets.{secret}")), "{secret}");
    }

    if !have_node() {
        return;
    }
    // The shim maps each supported platform to its release asset.
    let assets = Command::new("node")
        .arg("-e")
        .arg(
            "const r = require(process.argv[1]); \
             console.log(JSON.stringify(['darwin-arm64','linux-x64','win32-x64','freebsd-x64'] \
             .map(k => { const [p, a] = k.split('-'); const x = r.assetFor(p, a); \
             return x && r.assetUrl('9.9.9', x.file); })))",
        )
        .arg(root.join("packages/meshtree-mcp/lib/resolve.js"))
        .output()
        .expect("node");
    assert!(
        assets.status.success(),
        "{}",
        String::from_utf8_lossy(&assets.stderr)
    );
    let base = "https://github.com/Aviation-0/MeshTree/releases/download/v9.9.9";
    let urls: Value = serde_json::from_slice(&assets.stdout).unwrap();
    assert_eq!(
        urls,
        json!([
            format!("{base}/meshtree-aarch64-apple-darwin.tar.xz"),
            format!("{base}/meshtree-x86_64-unknown-linux-gnu.tar.xz"),
            format!("{base}/meshtree-x86_64-pc-windows-msvc.zip"),
            null,
        ])
    );

    // Scoop and winget templates render with the version and hash filled in.
    let out = TempDir::new();
    let sha = "ab".repeat(32);
    let rendered = Command::new("node")
        .arg(root.join("dist/render-manifests.mjs"))
        .args(["1.2.3", &sha])
        .arg(out.path())
        .output()
        .expect("node render-manifests");
    assert!(
        rendered.status.success(),
        "{}",
        String::from_utf8_lossy(&rendered.stderr)
    );
    let scoop: Value =
        serde_json::from_str(&fs::read_to_string(out.path().join("scoop/meshtree.json")).unwrap())
            .unwrap();
    assert_eq!(scoop["version"], "1.2.3");
    assert_eq!(scoop["architecture"]["64bit"]["hash"], sha.as_str());
    assert_eq!(
        scoop["architecture"]["64bit"]["url"],
        "https://github.com/Aviation-0/MeshTree/releases/download/v1.2.3/meshtree-x86_64-pc-windows-msvc.zip"
    );
    let installer =
        fs::read_to_string(out.path().join("winget/Aviation-0.MeshTree.installer.yaml")).unwrap();
    assert!(installer.contains("PackageVersion: 1.2.3"));
    assert!(installer.contains(&format!("InstallerSha256: {}", sha.to_uppercase())));
    for name in fs::read_dir(out.path().join("winget")).unwrap() {
        let text = fs::read_to_string(name.unwrap().path()).unwrap();
        assert!(!text.contains("{{"), "{text}");
    }

    // Bad input is refused rather than rendered.
    let bad = Command::new("node")
        .arg(root.join("dist/render-manifests.mjs"))
        .args(["1.2.3", "not-a-hash"])
        .arg(out.path())
        .output()
        .unwrap();
    assert_eq!(bad.status.code(), Some(2));
}
