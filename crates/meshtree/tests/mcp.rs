//! `meshtree mcp`: the stdio MCP proxy (`SPEC.md` §4, §15, §16).

mod common;

use common::{assert_exit, commit, git, meshtree, DaemonProc, TempDir};
use meshtree_platform::ipc::{Endpoint, IpcClient};
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

const CONFIG: &str = r#"[project]
base_branch = "main"
integration_branch = "meshtree/integration"
validate = ["git --version"]
promote_validate = ["git --version"]

[queue.test]
concurrency = 1
command = "git rev-parse --abbrev-ref HEAD"
"#;

fn repo() -> TempDir {
    let repo = TempDir::repo_with_commit();
    assert_exit(&meshtree(repo.path(), &["init"]), 0);
    fs::write(repo.path().join(".meshtree/config.toml"), CONFIG).unwrap();
    repo
}

/// A `meshtree mcp` child speaking JSON-RPC over its stdio.
struct Mcp {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Mcp {
    fn start(cwd: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_meshtree"))
            .arg("mcp")
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn meshtree mcp");
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut mcp = Self {
            child,
            stdin,
            stdout,
            next_id: 0,
        };
        let init = mcp.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "meshtree-test", "version": "0" },
            }),
        );
        assert_eq!(init["result"]["serverInfo"]["name"], "meshtree", "{init}");
        mcp.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
        mcp
    }

    fn send(&mut self, message: &Value) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        writeln!(stdin, "{message}").unwrap();
        stdin.flush().unwrap();
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        let reply: Value = serde_json::from_str(&line).unwrap_or_else(|e| panic!("{e}: {line:?}"));
        assert_eq!(reply["id"], id, "{reply}");
        reply
    }

    /// `tools/call`; returns `(isError, structuredContent)`.
    fn call(&mut self, tool: &str, arguments: Value) -> (bool, Value) {
        let reply = self.request(
            "tools/call",
            json!({ "name": tool, "arguments": arguments }),
        );
        let result = &reply["result"];
        assert!(result.is_object(), "{tool}: {reply}");
        let text = result["content"][0]["text"].as_str().unwrap_or_default();
        assert!(!text.is_empty(), "{tool}: no text content: {reply}");
        (
            result["isError"].as_bool().unwrap(),
            result["structuredContent"].clone(),
        )
    }

    fn ok(&mut self, tool: &str, arguments: Value) -> Value {
        let (is_error, value) = self.call(tool, arguments);
        assert!(!is_error, "{tool}: {value}");
        value
    }

    /// Asserts a tool error carrying `code`; returns its message.
    fn err(&mut self, tool: &str, arguments: Value, code: &str) -> String {
        let (is_error, value) = self.call(tool, arguments);
        assert!(is_error, "{tool}: expected {code}, got {value}");
        assert_eq!(value["code"], code, "{tool}: {value}");
        value["message"].as_str().unwrap().to_string()
    }

    /// Closes stdin and waits for the process to exit.
    fn close(mut self) -> std::process::ExitStatus {
        drop(self.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "meshtree mcp did not exit on EOF"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn ping(repo: &Path) -> Value {
    let endpoint = Endpoint::for_repo(repo).unwrap();
    let resp = IpcClient::connect(&endpoint)
        .expect("daemon reachable")
        .call("ping", json!({}))
        .unwrap();
    assert!(resp.ok, "{resp:?}");
    resp.result.unwrap()
}

fn id_of(value: &Value) -> String {
    value["task_id"].as_str().unwrap().to_string()
}

#[test]
fn mcp_all_11_tools_roundtrip_and_mt_error_codes() {
    let repo = repo();
    let root = repo.path();

    // No daemon: every tool is an MT-010 tool error, not a crash.
    let mut mcp = Mcp::start(root);
    let tools = mcp.request("tools/list", json!({}));
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "spawn_task",
            "task_status",
            "list_tasks",
            "claim_paths",
            "release_paths",
            "request_shared_op",
            "commit_handoff",
            "collect_task",
            "abort_task",
            "render_agents_block",
            "sync",
        ]
    );
    mcp.err("list_tasks", json!({}), "MT-010");

    // The same proxy works once the daemon is up (it keeps no state).
    let daemon = DaemonProc::start(root);

    // spawn_task
    let a = mcp.ok(
        "spawn_task",
        json!({ "name": "alpha", "agent": "mcp-test", "claims": ["src/a/**"] }),
    );
    let a_id = id_of(&a);
    let a_wt = PathBuf::from(a["worktree_path"].as_str().unwrap());
    assert!(a_wt.join("README.md").is_file(), "{a}");
    assert_eq!(a["branch"], format!("meshtree/{a_id}"));
    assert_eq!(a["claims"], json!(["src/a/**"]));
    let b_id = id_of(&mcp.ok("spawn_task", json!({ "name": "beta" })));
    mcp.err(
        "spawn_task",
        json!({ "name": "x", "base": "no-such-ref" }),
        "usage",
    );

    // claim_paths: MT-001 on overlap, naming the holder; granted otherwise.
    let conflict = mcp.err(
        "claim_paths",
        json!({ "task_id": b_id, "paths": ["src/a/lib.rs"] }),
        "MT-001",
    );
    assert!(conflict.contains(&a_id), "{conflict}");
    let granted = mcp.ok(
        "claim_paths",
        json!({ "task_id": b_id, "paths": ["docs/**"], "ttl_secs": 120 }),
    );
    assert_eq!(granted["granted"], json!(["docs/**"]));
    assert_eq!(granted["rejected"], json!([]));
    mcp.err(
        "claim_paths",
        json!({ "task_id": b_id, "paths": ["x/**"], "ttl_secs": 0 }),
        "usage",
    );
    mcp.err("claim_paths", json!({ "task_id": b_id }), "usage");

    // release_paths
    let released = mcp.ok(
        "release_paths",
        json!({ "task_id": b_id, "paths": ["docs/**"] }),
    );
    assert_eq!(released["released"], json!(["docs/**"]));

    // task_status / list_tasks
    let status = mcp.ok("task_status", json!({ "task_id": a_id }));
    assert_eq!(status["status"], "active");
    assert_eq!(status["claims"], json!(["src/a/**"]));
    assert_eq!(
        mcp.ok("task_status", json!({}))["tasks"]
            .as_array()
            .unwrap()
            .len(),
        2,
        "a bad base is rejected before any task is recorded"
    );
    let live = mcp.ok("list_tasks", json!({ "active_only": true }));
    assert_eq!(live["tasks"].as_array().unwrap().len(), 2, "{live}");
    mcp.err(
        "task_status",
        json!({ "task_id": "t-00000000000000000000000000" }),
        "task",
    );

    // request_shared_op: waits, runs in the task worktree, releases.
    let op = mcp.ok(
        "request_shared_op",
        json!({ "task_id": a_id, "queue": "test" }),
    );
    assert_eq!(op["granted"], true);
    assert_eq!(op["success"], true, "{op}");
    assert_eq!(
        op["stdout"].as_str().unwrap().trim(),
        format!("meshtree/{a_id}")
    );
    mcp.err(
        "request_shared_op",
        json!({ "task_id": a_id, "queue": "test", "command_override": ["--evil"] }),
        "usage",
    );
    mcp.err(
        "request_shared_op",
        json!({ "task_id": a_id, "queue": "nope" }),
        "usage",
    );

    // commit_handoff: given fields replace, the rest are kept.
    let first = mcp.ok(
        "commit_handoff",
        json!({
            "task_id": a_id,
            "state": "mid-implementation",
            "next": ["write tests"],
            "decisions": [{ "what": "plumbing", "why": "keeps worktree edits" }],
        }),
    );
    assert!(first["commit"].is_string(), "{first}");
    let handoff_md = PathBuf::from(first["handoff_md"].as_str().unwrap());
    let second = mcp.ok(
        "commit_handoff",
        json!({ "task_id": a_id, "objective": "Build alpha" }),
    );
    assert!(second["commit"].is_string());
    let text = fs::read_to_string(&handoff_md).unwrap();
    for needle in [
        "**Objective:** Build alpha",
        "**State:** mid-implementation",
        "- write tests",
        "- plumbing (why: keeps worktree edits)",
    ] {
        assert!(text.contains(needle), "{needle:?} in\n{text}");
    }
    mcp.err(
        "commit_handoff",
        json!({ "task_id": a_id, "mood": "great" }),
        "usage",
    );

    // render_agents_block
    let block = mcp.ok("render_agents_block", json!({}));
    let body = block["block"].as_str().unwrap();
    assert!(
        body.contains(&format!("[{a_id}] alpha (mcp-test, ")),
        "{body}"
    );
    assert!(body.contains("mid-implementation."), "{body}");
    assert_eq!(block["sessions"], 2);
    assert!(fs::read_to_string(root.join("AGENTS.md"))
        .unwrap()
        .contains(body));

    // sync
    let synced = mcp.ok("sync", json!({}));
    assert!(
        synced["coord"].as_str().unwrap().starts_with("coord "),
        "{synced}"
    );

    // collect_task: merges alpha's work into integration.
    fs::create_dir_all(a_wt.join("src/a")).unwrap();
    fs::write(a_wt.join("src/a/lib.rs"), "pub fn a() {}\n").unwrap();
    git(&a_wt, &["add", "src/a/lib.rs"]);
    commit(&a_wt, "alpha work");
    let merged = mcp.ok("collect_task", json!({ "task_id": a_id }));
    assert_eq!(merged["status"], "merged", "{merged}");
    assert!(git(root, &["show", "meshtree/integration:src/a/lib.rs"]).contains("pub fn a"));

    // abort_task: MT-004 for a finished task; abandons an active one.
    mcp.err("abort_task", json!({ "task_id": a_id }), "MT-004");
    let aborted = mcp.ok("abort_task", json!({ "task_id": b_id }));
    assert_eq!(aborted["status"], "abandoned");
    mcp.err(
        "claim_paths",
        json!({ "task_id": b_id, "paths": ["y/**"] }),
        "task",
    );

    // Protocol errors stay JSON-RPC errors.
    let unknown = mcp.request("tools/call", json!({ "name": "rm_rf", "arguments": {} }));
    assert_eq!(unknown["error"]["code"], -32602, "{unknown}");

    assert!(mcp.close().success());
    daemon.stop(root);
}

#[test]
fn mcp_client_eof_exits_without_killing_daemon() {
    let repo = repo();
    let root = repo.path();
    let daemon = DaemonProc::start(root);
    let before = ping(root);

    let mut mcp = Mcp::start(root);
    let spawned = mcp.ok("spawn_task", json!({ "name": "eof" }));
    let id = id_of(&spawned);
    let status = mcp.close();
    assert!(status.success(), "mcp exit status {status}");

    // The daemon is the same process and still serves requests.
    let after = ping(root);
    assert_eq!(after["pid"], before["pid"]);
    assert_eq!(after["pid"], daemon.pid());
    let listed = meshtree(root, &["status", &id]);
    assert_exit(&listed, 0);

    // A second client after the first one left works as before.
    let mut again = Mcp::start(root);
    assert_eq!(
        again.ok("task_status", json!({ "task_id": id }))["status"],
        "active"
    );
    drop(again);
    assert_eq!(ping(root)["pid"], daemon.pid());
    daemon.stop(root);
}

#[test]
fn mcp_listing_manifest_matches_server_tools() {
    // packages/meshtree-mcp/server.json (MCP registry format) lists exactly
    // the tools `meshtree mcp` advertises, with the same descriptions, and
    // its versions and names agree with the crate and the npm package.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let read = |rel: &str| -> Value {
        serde_json::from_str(&fs::read_to_string(root.join(rel)).unwrap())
            .unwrap_or_else(|err| panic!("{rel}: {err}"))
    };
    let manifest = read("packages/meshtree-mcp/server.json");
    let package = read("packages/meshtree-mcp/package.json");

    let repo = repo();
    let mut mcp = Mcp::start(repo.path());
    let listed = mcp.request("tools/list", json!({}));
    let served: Vec<(String, String)> = listed["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("{listed}"))
        .iter()
        .map(|t| {
            (
                t["name"].as_str().unwrap().to_string(),
                t["description"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    let documented: Vec<(String, String)> = manifest["_meta"]
        ["io.modelcontextprotocol.registry/publisher-provided"]["tools"]
        .as_array()
        .expect("manifest tool list")
        .iter()
        .map(|t| {
            (
                t["name"].as_str().unwrap().to_string(),
                t["description"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(served.len(), 11, "SPEC §15 lists 11 tools: {served:?}");
    assert_eq!(documented, served, "server.json tools vs tools/list");

    let version = env!("CARGO_PKG_VERSION");
    assert_eq!(manifest["version"], version);
    assert_eq!(manifest["packages"][0]["version"], version);
    assert_eq!(manifest["packages"][0]["registryType"], "npm");
    assert_eq!(manifest["packages"][0]["identifier"], package["name"]);
    assert_eq!(manifest["packages"][0]["transport"]["type"], "stdio");
    assert_eq!(manifest["name"], package["mcpName"]);
    assert!(manifest["description"].as_str().unwrap().len() <= 100);
}
