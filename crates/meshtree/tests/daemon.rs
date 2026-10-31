//! `meshtree daemon`: singleton service, IPC-routed claims, and the §18
//! "two client processes racing claims through one daemon — exactly one
//! wins" integration test (`SPEC.md` §4, §9, §10, §14, §16, §18).

mod common;

use common::{assert_exit, meshtree, TempDir};
use meshtree_core::{Task, TaskId, ERROR_CODE_CLAIM_CONFLICT};
use meshtree_platform::ipc::{Endpoint, IpcClient};
use meshtree_platform::wal::{self, WalOp};
use serde_json::json;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

/// A running `meshtree daemon`, killed on drop if still alive.
struct DaemonProc(Child);

impl DaemonProc {
    /// Starts the daemon in `repo` and waits for its "listening" line.
    fn start(repo: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_meshtree"))
            .arg("daemon")
            .current_dir(repo)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn daemon");
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        if !line.contains("listening on") {
            let _ = child.kill();
            let out = child.wait_with_output().unwrap();
            panic!(
                "daemon did not start: {line:?} / {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Self(child)
    }

    /// Sends `shutdown` and waits for a clean exit.
    fn stop(mut self, repo: &Path) {
        let endpoint = Endpoint::for_repo(repo).unwrap();
        let resp = IpcClient::connect(&endpoint)
            .expect("daemon reachable")
            .call("shutdown", json!({}))
            .unwrap();
        assert!(resp.ok, "{resp:?}");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                assert!(status.success(), "daemon exit status {status}");
                return;
            }
            assert!(Instant::now() < deadline, "daemon did not stop");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for DaemonProc {
    fn drop(&mut self) {
        if let Ok(None) = self.0.try_wait() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn spawn_task(repo: &Path, name: &str) -> String {
    let out = meshtree(repo, &["spawn", name]);
    assert_exit(&out, 0);
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("task_id: "))
        .expect("spawn prints task_id")
        .to_string()
}

fn read_task(repo: &Path, id: &str) -> Task {
    let path = repo.join(".meshtree/tasks").join(id).join("task.json");
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

fn claim_process(repo: &Path, task: &str, path: &str) -> Child {
    Command::new(env!("CARGO_BIN_EXE_meshtree"))
        .args(["claim", task, path])
        .current_dir(repo)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn claim client")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn two_client_processes_race_claims_one_wins() {
    let repo = TempDir::repo_with_commit();
    let root = repo.path();
    assert_exit(&meshtree(root, &["init"]), 0);
    let a = spawn_task(root, "task-a");
    let b = spawn_task(root, "task-b");

    let daemon = DaemonProc::start(root);

    // A second daemon for the same repository is refused (singleton, §10).
    let second = meshtree(root, &["daemon"]);
    assert_exit(&second, 30);
    assert!(
        stderr(&second).contains("already holds"),
        "{}",
        stderr(&second)
    );

    // Each round, two client processes claim overlapping paths for different
    // tasks at the same time (alternating launch order).
    let mut winners: Vec<(String, String)> = Vec::new();
    for round in 0..4 {
        let path = format!("src/r{round}/**");
        let overlap = format!("src/r{round}/x/**");
        let (first, second) = if round % 2 == 0 { (&a, &b) } else { (&b, &a) };
        let p1 = claim_process(root, first, &path);
        let p2 = claim_process(root, second, &overlap);
        let outs = [
            p1.wait_with_output().unwrap(),
            p2.wait_with_output().unwrap(),
        ];
        let codes: Vec<Option<i32>> = outs.iter().map(|o| o.status.code()).collect();
        let won: Vec<usize> = (0..2).filter(|&i| codes[i] == Some(0)).collect();
        assert_eq!(
            won.len(),
            1,
            "round {round}: exactly one client wins; codes {codes:?}, stderr {:?} / {:?}",
            stderr(&outs[0]),
            stderr(&outs[1])
        );
        let (w, l) = (won[0], 1 - won[0]);
        assert_eq!(codes[l], Some(20), "loser exits 20 (§14)");
        let loser_err = stderr(&outs[l]);
        assert!(loser_err.contains(ERROR_CODE_CLAIM_CONFLICT), "{loser_err}");
        let (win_task, win_path) = if w == 0 {
            (first.clone(), path.clone())
        } else {
            (second.clone(), overlap.clone())
        };
        assert!(
            loser_err.contains(&win_task),
            "names the holder: {loser_err}"
        );
        winners.push((win_task, win_path));
    }

    // task.json records only the winning claims.
    for id in [&a, &b] {
        let mut expected: Vec<String> = winners
            .iter()
            .filter(|(t, _)| t == id)
            .map(|(_, p)| p.clone())
            .collect();
        let mut claims = read_task(root, id).claims;
        expected.sort();
        claims.sort();
        assert_eq!(claims, expected, "{id}");
    }

    // `task_status` over IPC serves the same registry.
    let endpoint = Endpoint::for_repo(root).unwrap();
    let mut client = IpcClient::connect(&endpoint).unwrap();
    let listed = client.call("task_status", json!({})).unwrap();
    assert_eq!(listed.result.unwrap().as_array().unwrap().len(), 2);
    // A conflicting claim over IPC carries MT-001 in the error body.
    let (holder, held) = &winners[0];
    let other = if holder == &a { &b } else { &a };
    let bad = client
        .call("claim_paths", json!({ "task_id": other, "paths": [held] }))
        .unwrap();
    assert!(!bad.ok);
    let body = bad.error.unwrap();
    assert_eq!(body.code, ERROR_CODE_CLAIM_CONFLICT, "{body:?}");
    assert!(body.message.contains(holder.as_str()), "{body:?}");
    drop(client);

    daemon.stop(root);

    // Every granted claim went through the daemon's WAL; nothing else did.
    let log = wal::read(root).unwrap().entries;
    let journaled: Vec<(TaskId, Vec<String>)> = log
        .iter()
        .filter_map(|e| match &e.op {
            WalOp::Claim { task, paths } => Some((task.clone(), paths.clone())),
            _ => None,
        })
        .collect();
    let expected: Vec<(TaskId, Vec<String>)> = winners
        .iter()
        .map(|(t, p)| (TaskId::parse(t).unwrap(), vec![p.clone()]))
        .collect();
    assert_eq!(journaled, expected);
    let state = wal::replay(&log);
    assert!(state.anomalies.is_empty(), "{:?}", state.anomalies);
    let mut winning_tasks: Vec<&String> = winners.iter().map(|(t, _)| t).collect();
    winning_tasks.sort();
    winning_tasks.dedup();
    assert_eq!(
        state.tasks.len(),
        winning_tasks.len(),
        "adopted on first claim"
    );

    // With the daemon stopped the CLI works in-process (no MT-010), and the
    // WAL is untouched.
    let out = meshtree(root, &["claim", &a, "docs/**"]);
    assert_exit(&out, 0);
    assert_eq!(wal::read(root).unwrap().entries, log);
}
