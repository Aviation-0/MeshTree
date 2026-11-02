//! `meshtree mcp`: the stdio MCP server.
//!
//! Authoritative specification: `SPEC.md` §4 (MCP is an IPC client of the
//! daemon), §15 (the 11 tools; a stateless proxy that dies with its client
//! and never kills the daemon; MeshTree error codes as MCP tool errors), §16.
//!
//! Transport: newline-delimited JSON-RPC 2.0 on stdin/stdout (MCP stdio).
//! Handled: `initialize`, `ping`, `tools/list`, `tools/call`; notifications
//! get no reply. stdout carries protocol messages only.
//!
//! Each tool call resolves the repository from the working directory and
//! makes one IPC request to its daemon, so the proxy keeps no state and
//! survives daemon restarts. A failed call is an MCP tool result with
//! `isError: true`, its text starting with the `[MT-xxx]` code (§16) and
//! `structuredContent` `{code, message}`; with no daemon that is `MT-010`.
//!
//! `request_shared_op` is the one tool with local work: like
//! `meshtree queue … request` it holds the ticket as this process (pid +
//! boot id), waits for the grant, runs the queue command in the task
//! worktree with its output captured, and releases the slot before replying
//! (§12: the slot is held until the command exits).
//!
//! At end of input the server returns: the client went away, and the daemon
//! (a separate process) is untouched.

use crate::daemon;
use crate::exit::{CliError, DAEMON, PLATFORM};
use crate::queue;
use crate::repo::require_repo_root;
use meshtree_platform::lock::boot_id;
use serde_json::{json, Map, Value};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;

/// Newest MCP protocol revision this server speaks; offered when the client
/// asks for one it does not know.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// Protocol revisions accepted as requested.
const SUPPORTED_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

/// Captured queue command output kept per stream (the tail).
const OUTPUT_TAIL_BYTES: usize = 16 * 1024;

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

/// Runs `meshtree mcp` on the process's stdin/stdout until end of input.
pub fn run(cwd: &Path) -> Result<(), CliError> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    serve(stdin.lock(), stdout.lock(), cwd)
        .map_err(|err| CliError::new(PLATFORM, format!("mcp stdio: {err}")))
}

/// Serves requests read from `input` until it ends, writing one response
/// line per request to `output`.
pub fn serve(input: impl BufRead, mut output: impl Write, cwd: &Path) -> io::Result<()> {
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = handle_line(&line, cwd) {
            let mut text = response.to_string();
            text.push('\n');
            output.write_all(text.as_bytes())?;
            output.flush()?;
        }
    }
    Ok(())
}

/// The response to one input line, `None` for a notification.
fn handle_line(line: &str, cwd: &Path) -> Option<Value> {
    let message: Value = match serde_json::from_str(line) {
        Ok(message) => message,
        Err(err) => return Some(rpc_error(Value::Null, PARSE_ERROR, &err.to_string())),
    };
    let Some(object) = message.as_object() else {
        return Some(rpc_error(
            Value::Null,
            INVALID_REQUEST,
            "expected a JSON-RPC request object",
        ));
    };
    let method = object.get("method").and_then(Value::as_str);
    let Some(id) = object.get("id").cloned() else {
        // A notification (`notifications/initialized`, cancellations, …)
        // or a stray response: nothing to answer.
        return None;
    };
    let Some(method) = method else {
        return Some(rpc_error(id, INVALID_REQUEST, "missing 'method'"));
    };
    let params = object.get("params").cloned().unwrap_or(Value::Null);
    Some(match dispatch(method, &params, cwd) {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, message)) => rpc_error(id, code, &message),
    })
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn dispatch(method: &str, params: &Value, cwd: &Path) -> Result<Value, (i64, String)> {
    match method {
        "initialize" => {
            let requested = params.get("protocolVersion").and_then(Value::as_str);
            let version = requested
                .filter(|v| SUPPORTED_VERSIONS.contains(v))
                .unwrap_or(PROTOCOL_VERSION);
            Ok(json!({
                "protocolVersion": version,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": "meshtree", "version": env!("CARGO_PKG_VERSION") },
                "instructions": "MeshTree coordinates parallel agents: spawn_task gives you an \
                    isolated worktree; claim_paths before editing; commit_handoff to record \
                    progress; collect_task to merge. Errors carry [MT-xxx] codes (SPEC.md §16).",
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools() })),
        "tools/call" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .ok_or((INVALID_PARAMS, "missing tool 'name'".to_string()))?;
            if !TOOLS.iter().any(|(tool, _, _)| *tool == name) {
                return Err((INVALID_PARAMS, format!("unknown tool '{name}'")));
            }
            let args = match params.get("arguments") {
                None | Some(Value::Null) => Map::new(),
                Some(Value::Object(args)) => args.clone(),
                Some(_) => return Err((INVALID_PARAMS, "'arguments' must be an object".into())),
            };
            Ok(match call_tool(name, args, cwd) {
                Ok(value) => tool_result(value, false),
                Err(err) => tool_error(&err),
            })
        }
        other => Err((METHOD_NOT_FOUND, format!("method '{other}' not found"))),
    }
}

fn tool_result(value: Value, is_error: bool) -> Value {
    let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": value,
        "isError": is_error,
    })
}

/// A failed call as an MCP tool error carrying the §16 code.
fn tool_error(err: &CliError) -> Value {
    let code = daemon::error_code(err);
    let text = if err.message.contains(&format!("[{code}]")) {
        err.message.clone()
    } else {
        format!("[{code}] {}", err.message)
    };
    json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": { "code": code, "message": err.message },
        "isError": true,
    })
}

/// Forwards one tool call to the daemon and shapes its result.
fn call_tool(name: &str, args: Map<String, Value>, cwd: &Path) -> Result<Value, CliError> {
    let repo_root = require_repo_root(cwd)?;
    if name == "request_shared_op" {
        return shared_op(&repo_root, args);
    }
    let result = forward(&repo_root, name, Value::Object(args))?;
    Ok(match name {
        // structuredContent must be an object.
        "list_tasks" => json!({ "tasks": result }),
        "task_status" if result.is_array() => json!({ "tasks": result }),
        "claim_paths" => json!({
            "granted": result["paths"],
            "rejected": [],
            "coord": result["coord"],
            "deferred": result["deferred"],
        }),
        "release_paths" => json!({
            "released": result["paths"],
            "coord": result["coord"],
            "deferred": result["deferred"],
        }),
        _ => result,
    })
}

fn forward(repo_root: &Path, method: &str, params: Value) -> Result<Value, CliError> {
    daemon::call(repo_root, method, params).unwrap_or_else(|| {
        Err(daemon::unreachable(
            "meshtree mcp proxies tools to the daemon",
        ))
    })
}

/// `request_shared_op`: ticket (held by this process), grant, run, release.
fn shared_op(repo_root: &Path, mut args: Map<String, Value>) -> Result<Value, CliError> {
    let boot = boot_id().map_err(|err| CliError::new(PLATFORM, format!("boot id: {err}")))?;
    // The holder is always this process; a client cannot name another one.
    args.insert("pid".into(), json!(std::process::id()));
    args.insert("boot_id".into(), json!(boot));
    let ticket = forward(repo_root, "request_shared_op", Value::Object(args))?;
    let field = |key: &str| -> Result<String, CliError> {
        ticket[key]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| CliError::new(DAEMON, format!("malformed ticket from daemon: {ticket}")))
    };
    let (id, command) = (field("id")?, field("command")?);
    let worktree = PathBuf::from(field("worktree")?);

    let outcome = queue::wait_for_grant(repo_root, &id).and_then(|()| {
        queue::shell(&command)
            .current_dir(&worktree)
            .stdin(Stdio::null())
            .output()
            .map_err(|err| CliError::new(PLATFORM, format!("run `{command}`: {err}")))
    });
    let released = forward(repo_root, "release_shared_op", json!({ "ticket_id": id }));
    let output = outcome?;
    released?;
    Ok(json!({
        "ticket_id": id,
        "queue": ticket["queue"],
        "position": ticket["position"],
        "granted": true,
        "command": command,
        "exit_code": output.status.code(),
        "success": output.status.success(),
        "stdout": tail(&output.stdout),
        "stderr": tail(&output.stderr),
    }))
}

/// The last [`OUTPUT_TAIL_BYTES`] of `bytes`, lossily decoded.
fn tail(bytes: &[u8]) -> String {
    let start = bytes.len().saturating_sub(OUTPUT_TAIL_BYTES);
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

/// One tool: name, description, input-schema builder.
type Tool = (&'static str, &'static str, fn() -> Value);

/// The §15 tools, in table order.
const TOOLS: [Tool; 11] = [
    (
        "spawn_task",
        "Create an isolated task worktree and branch (optionally claiming paths first).",
        || {
            schema(
                json!({
                    "name": string("Task name"),
                    "base": string("Base ref (default: project.base_branch)"),
                    "agent": string("Agent identifier"),
                    "claims": strings("Path patterns to claim"),
                }),
                &["name"],
            )
        },
    ),
    (
        "task_status",
        "One task record, or all task records when task_id is omitted.",
        || schema(json!({ "task_id": string("Task id (t-…)") }), &[]),
    ),
    (
        "list_tasks",
        "Task records; active_only keeps active, collecting and merging tasks.",
        || {
            schema(
                json!({ "active_only": { "type": "boolean", "description": "Only live tasks" } }),
                &[],
            )
        },
    ),
    (
        "claim_paths",
        "Claim path patterns for an active task: all granted, or MT-001 naming the holders.",
        || {
            schema(
                json!({
                    "task_id": string("Task id"),
                    "paths": strings("Path patterns"),
                    "ttl_secs": { "type": "integer", "minimum": 1, "description": "Lease TTL (default: lease.ttl_secs)" },
                }),
                &["task_id", "paths"],
            )
        },
    ),
    (
        "release_paths",
        "Release path claims held by an active task.",
        || {
            schema(
                json!({ "task_id": string("Task id"), "paths": strings("Path patterns") }),
                &["task_id", "paths"],
            )
        },
    ),
    (
        "request_shared_op",
        "Wait for a slot in a shared-operation queue, run its configured command in the task \
         worktree, release the slot, and return the result.",
        || {
            schema(
                json!({
                    "task_id": string("Task id"),
                    "queue": string("Queue name ([queue.<name>] in config.toml)"),
                    "command_override": strings("Extra arguments (must be in the queue's allow_args)"),
                }),
                &["task_id", "queue"],
            )
        },
    ),
    (
        "commit_handoff",
        "Commit the task's handoff record on its branch (given fields replace those already \
         recorded) and regenerate the AGENTS.md block.",
        || {
            schema(
                json!({
                    "task_id": string("Task id"),
                    "objective": string("What the task is for"),
                    "state": string("Where it stands"),
                    "done": strings("Finished steps"),
                    "next": strings("Next steps"),
                    "failing_tests": strings("Failing test names"),
                    "decisions": {
                        "type": "array",
                        "description": "Design decisions",
                        "items": {
                            "type": "object",
                            "properties": { "what": string("Decision"), "why": string("Reason") },
                            "required": ["what", "why"],
                        },
                    },
                    "ruled_out": strings("Approaches rejected"),
                }),
                &["task_id"],
            )
        },
    ),
    (
        "collect_task",
        "Validate and merge the task into the integration branch (MT-030/031/032 on failure).",
        || {
            schema(
                json!({ "task_id": string("Task id"), "message": string("Merge commit message") }),
                &["task_id"],
            )
        },
    ),
    (
        "abort_task",
        "Discard an active task: release its claims, remove its worktree, mark it abandoned.",
        || schema(json!({ "task_id": string("Task id") }), &["task_id"]),
    ),
    (
        "render_agents_block",
        "Regenerate the Active Sessions block in the root AGENTS.md and return it.",
        || schema(json!({}), &[]),
    ),
    (
        "sync",
        "Flush local claim events and pull remote ones (refs/meshtree/coord).",
        || schema(json!({}), &[]),
    ),
];

fn tools() -> Vec<Value> {
    TOOLS
        .iter()
        .map(|(name, description, input)| {
            json!({ "name": name, "description": description, "inputSchema": input() })
        })
        .collect()
}

fn schema(properties: Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": properties, "required": required })
}

fn string(description: &str) -> Value {
    json!({ "type": "string", "description": description })
}

fn strings(description: &str) -> Value {
    json!({ "type": "array", "items": { "type": "string" }, "description": description })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(lines: &[Value], cwd: &Path) -> Vec<Value> {
        let input: String = lines.iter().map(|l| format!("{l}\n")).collect();
        let mut out = Vec::new();
        serve(input.as_bytes(), &mut out, cwd).unwrap();
        String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn protocol_handshake_list_and_errors() {
        let cwd = std::env::temp_dir();
        let replies = roundtrip(
            &[
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}),
                json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
                json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
                json!({"jsonrpc":"2.0","id":"x","method":"nope"}),
                json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"nope"}}),
                json!({"jsonrpc":"2.0","id":4,"method":"ping"}),
            ],
            &cwd,
        );
        assert_eq!(
            replies.len(),
            5,
            "no reply to the notification: {replies:?}"
        );
        assert_eq!(replies[0]["result"]["protocolVersion"], "2024-11-05");
        let names: Vec<&str> = replies[1]["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names.len(), 11);
        assert_eq!(names[0], "spawn_task");
        assert_eq!(names[10], "sync");
        assert_eq!(replies[2]["id"], "x");
        assert_eq!(replies[2]["error"]["code"], METHOD_NOT_FOUND);
        assert_eq!(replies[3]["error"]["code"], INVALID_PARAMS);
        assert_eq!(replies[4]["result"], json!({}));
    }

    #[test]
    fn unknown_protocol_version_gets_ours_and_garbage_is_a_parse_error() {
        let cwd = std::env::temp_dir();
        let replies = roundtrip(
            &[
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"1999-01-01"}}),
            ],
            &cwd,
        );
        assert_eq!(replies[0]["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(
            handle_line("{not json", &cwd).unwrap()["error"]["code"],
            PARSE_ERROR
        );
        assert_eq!(
            handle_line("[1,2]", &cwd).unwrap()["error"]["code"],
            INVALID_REQUEST
        );
    }

    #[test]
    fn tool_errors_lead_with_the_code() {
        let err = CliError::new(crate::exit::TASK, "[MT-001] claim conflict: a held by b");
        let v = tool_error(&err);
        assert_eq!(v["isError"], true);
        assert_eq!(v["structuredContent"]["code"], "MT-001");
        assert_eq!(
            v["content"][0]["text"],
            "[MT-001] claim conflict: a held by b"
        );
        let v = tool_error(&CliError::new(crate::exit::USAGE, "bad"));
        assert_eq!(v["content"][0]["text"], "[usage] bad");
    }

    #[test]
    fn tail_keeps_the_end() {
        let long = vec![b'a'; OUTPUT_TAIL_BYTES + 5];
        assert_eq!(tail(&long).len(), OUTPUT_TAIL_BYTES);
        assert_eq!(tail(b"ok"), "ok");
    }
}
