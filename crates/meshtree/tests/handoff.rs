//! `meshtree handoff` and `meshtree render-agents` (`SPEC.md` §5, §13, §14).

mod common;

use common::{assert_exit, commit, git, meshtree, TempDir};
use meshtree_core::handoff::{
    render_agents_block, render_handoff_md, splice_agents_md, ActiveSession, Decision,
    HandoffRecord, DEFAULT_LINE_CAP,
};
use meshtree_core::{Task, TaskId};
use std::fs;
use std::path::Path;
use std::process::Output;

const SPEC_EXAMPLE: &str = r#"{
  "objective": "Add idempotency checks to Stripe webhook handler",
  "state": "mid-implementation",
  "done": ["idempotency-key column added", "migrations written"],
  "next": ["wrap handler in transaction", "write replay test"],
  "failing_tests": ["webhooks::test_double_charge"],
  "decisions": [{ "what": "use upsert instead of lock", "why": "avoids row lock contention" }],
  "ruled_out": ["redis-based dedupe — rejected as non-local-first"]
}"#;

const USER_TEXT: &str = "# Project rules\n\nBe kind to the build.\n";

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn spawn(repo: &Path, args: &[&str]) -> Task {
    let mut full = vec!["spawn"];
    full.extend_from_slice(args);
    let out = meshtree(repo, &full);
    assert_exit(&out, 0);
    let id = stdout(&out)
        .lines()
        .find_map(|l| l.strip_prefix("task_id: "))
        .expect("spawn prints task_id")
        .to_string();
    read_task(repo, &id)
}

fn read_task(repo: &Path, id: &str) -> Task {
    let path = repo.join(".meshtree/tasks").join(id).join("task.json");
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

fn tip(repo: &Path, task: &Task) -> String {
    git(repo, &["rev-parse", &format!("refs/heads/{}", task.branch)])
        .trim()
        .to_string()
}

/// A file as committed at the tip of the task branch.
fn on_branch(repo: &Path, task: &Task, file: &str) -> String {
    git(
        repo,
        &[
            "show",
            &format!(
                "refs/heads/{}:.meshtree/tasks/{}/{file}",
                task.branch, task.id
            ),
        ],
    )
}

fn branch_record(repo: &Path, task: &Task) -> HandoffRecord {
    serde_json::from_str(&on_branch(repo, task, "handoff.json")).unwrap()
}

/// The `AGENTS.md` the CLI must produce for `sessions` (core renderer).
fn expected_agents(sessions: &[ActiveSession]) -> String {
    splice_agents_md(USER_TEXT, &render_agents_block(sessions, DEFAULT_LINE_CAP))
}

fn agents_md(repo: &Path) -> String {
    fs::read_to_string(repo.join("AGENTS.md")).unwrap()
}

#[test]
fn cli_handoff_commits_on_task_branch_and_renders_agents_md() {
    let repo = TempDir::repo_with_commit();
    let root = repo.path();
    fs::write(root.join("AGENTS.md"), USER_TEXT).unwrap();
    git(root, &["add", "AGENTS.md"]);
    commit(root, "add AGENTS.md");
    assert_exit(&meshtree(root, &["init"]), 0);
    let main_head = git(root, &["rev-parse", "HEAD"]);

    let a = spawn(root, &["fix-webhooks", "--agent", "vibe-cli"]);
    let b = spawn(root, &["explore-auth"]);
    let a_worktree = root.join(&a.worktree);

    // The agent has its own work in progress: one staged, one untracked file.
    fs::write(a_worktree.join("README.md"), "hello\nstaged edit\n").unwrap();
    git(&a_worktree, &["add", "README.md"]);
    fs::write(a_worktree.join("scratch.txt"), "untracked\n").unwrap();
    let status_before = git(&a_worktree, &["status", "--porcelain"]);

    // 1. --json-file commits handoff.json + HANDOFF.md on the task branch.
    let a_tip = tip(root, &a);
    let json = root.join("handoff-a.json");
    fs::write(&json, SPEC_EXAMPLE).unwrap();
    let out = meshtree(
        root,
        &[
            "handoff",
            a.id.as_str(),
            "--json-file",
            json.to_str().unwrap(),
        ],
    );
    assert_exit(&out, 0);
    assert!(
        stdout(&out).contains(&format!("committed handoff for {}", a.id)),
        "{}",
        stdout(&out)
    );
    let record: HandoffRecord = serde_json::from_str(SPEC_EXAMPLE).unwrap();
    let new_tip = tip(root, &a);
    assert_eq!(
        git(root, &["rev-parse", &format!("{new_tip}^")]).trim(),
        a_tip
    );
    assert_eq!(on_branch(root, &a, "handoff.json"), record.to_json());
    assert_eq!(
        on_branch(root, &a, "HANDOFF.md"),
        render_handoff_md(&a.id, &a.name, &record)
    );
    let touched = git(
        root,
        &["diff-tree", "--no-commit-id", "--name-only", "-r", &new_tip],
    );
    let mut touched: Vec<&str> = touched.lines().collect();
    touched.sort();
    assert_eq!(
        touched,
        [
            format!(".meshtree/tasks/{}/HANDOFF.md", a.id),
            format!(".meshtree/tasks/{}/handoff.json", a.id),
        ]
    );
    // Rest of the tree is untouched: README.md is still the base version.
    assert_eq!(
        git(root, &["show", &format!("{new_tip}:README.md")]),
        "hello\n"
    );

    // The worktree moved with its branch, and the agent's changes survived.
    assert_eq!(git(&a_worktree, &["rev-parse", "HEAD"]).trim(), new_tip);
    assert_eq!(git(&a_worktree, &["status", "--porcelain"]), status_before);
    let on_disk =
        fs::read_to_string(a_worktree.join(format!(".meshtree/tasks/{}/handoff.json", a.id)))
            .unwrap()
            .replace("\r\n", "\n");
    assert_eq!(on_disk, record.to_json());
    // The main checkout is untouched.
    assert_eq!(git(root, &["rev-parse", "HEAD"]), main_head);

    // 2. AGENTS.md: user text kept, block = core renderer over both live tasks.
    let sessions = |a_rec: Option<HandoffRecord>, b_rec: Option<HandoffRecord>| {
        vec![
            ActiveSession::from_task(&a, a_rec),
            ActiveSession::from_task(&b, b_rec),
        ]
    };
    assert_eq!(
        agents_md(root),
        expected_agents(&sessions(Some(record.clone()), None))
    );
    let rendered = agents_md(root);
    assert!(rendered.starts_with(USER_TEXT));
    assert!(rendered.contains("mid-implementation."));
    assert!(rendered
        .contains("Next: wrap handler in transaction. Failing: webhooks::test_double_charge."));
    assert!(rendered.contains(&format!("[{}] explore-auth", b.id)));

    // 3. Identical record again: no new commit, AGENTS.md unchanged.
    let out = meshtree(
        root,
        &[
            "handoff",
            a.id.as_str(),
            "--json-file",
            json.to_str().unwrap(),
        ],
    );
    assert_exit(&out, 0);
    assert!(stdout(&out).contains("unchanged"), "{}", stdout(&out));
    assert_eq!(tip(root, &a), new_tip);

    // 4. --set on a task with no handoff yet, lists in the order given.
    let out = meshtree(
        root,
        &[
            "handoff",
            b.id.as_str(),
            "--set",
            "state=exploring",
            "--set",
            "next=read the session code",
            "--set",
            "next=write a failing test",
            "--set",
            "decisions=keep cookies|tokens need a migration",
        ],
    );
    assert_exit(&out, 0);
    let b_record = branch_record(root, &b);
    assert_eq!(
        b_record,
        HandoffRecord {
            state: "exploring".into(),
            next: vec![
                "read the session code".into(),
                "write a failing test".into()
            ],
            decisions: vec![Decision {
                what: "keep cookies".into(),
                why: "tokens need a migration".into()
            }],
            ..HandoffRecord::default()
        }
    );

    // 5. --set edits the record on the branch: other fields are kept, an
    //    empty value clears a list.
    let out = meshtree(
        root,
        &[
            "handoff",
            a.id.as_str(),
            "--set",
            "state=ready for review",
            "--set",
            "failing_tests=",
        ],
    );
    assert_exit(&out, 0);
    let a_record = branch_record(root, &a);
    assert_eq!(
        a_record,
        HandoffRecord {
            state: "ready for review".into(),
            failing_tests: vec![],
            ..record.clone()
        }
    );
    assert_eq!(
        agents_md(root),
        expected_agents(&sessions(Some(a_record.clone()), Some(b_record.clone())))
    );

    // 6. render-agents is idempotent.
    let out = meshtree(root, &["render-agents"]);
    assert_exit(&out, 0);
    assert!(stdout(&out).starts_with("unchanged"), "{}", stdout(&out));
    assert!(
        stdout(&out).contains("(2 active sessions)"),
        "{}",
        stdout(&out)
    );

    // 7. Usage and task errors change nothing.
    let before = (tip(root, &a), agents_md(root));
    let id = a.id.as_str();
    assert_exit(&meshtree(root, &["handoff", id]), 10);
    assert_exit(
        &meshtree(
            root,
            &[
                "handoff",
                id,
                "--json-file",
                json.to_str().unwrap(),
                "--set",
                "state=x",
            ],
        ),
        10,
    );
    assert_exit(&meshtree(root, &["handoff", id, "--set", "mood=great"]), 10);
    assert_exit(&meshtree(root, &["handoff", id, "--set", "state"]), 10);
    assert_exit(
        &meshtree(root, &["handoff", id, "--set", "decisions=no reason"]),
        10,
    );
    assert_exit(
        &meshtree(
            root,
            &["handoff", id, "--set", "state=a", "--set", "state=b"],
        ),
        10,
    );
    let typo = root.join("typo.json");
    fs::write(&typo, r#"{"state": "x", "nxet": ["y"]}"#).unwrap();
    assert_exit(
        &meshtree(
            root,
            &["handoff", id, "--json-file", typo.to_str().unwrap()],
        ),
        10,
    );
    assert_exit(
        &meshtree(root, &["handoff", "not-a-task", "--set", "state=x"]),
        10,
    );
    let unknown = TaskId::parse("t-01JAXGZK7Q4D3RT2WFH8MB3VQY").unwrap();
    assert_exit(
        &meshtree(root, &["handoff", unknown.as_str(), "--set", "state=x"]),
        20,
    );
    // A local edit to the handoff file blocks the fast-forward: nothing moves.
    let local = a_worktree.join(format!(".meshtree/tasks/{}/handoff.json", a.id));
    fs::write(&local, "{}\n").unwrap();
    let out = meshtree(root, &["handoff", id, "--set", "state=blocked"]);
    assert_exit(&out, 20);
    assert_eq!(tip(root, &a), before.0);
    fs::write(&local, a_record.to_json()).unwrap();
    git(
        &a_worktree,
        &[
            "checkout",
            "--",
            &format!(".meshtree/tasks/{}/handoff.json", a.id),
        ],
    );
    assert_eq!((tip(root, &a), agents_md(root)), before);

    // 8. A finished task takes no handoff and leaves the block.
    assert_exit(&meshtree(root, &["abort", b.id.as_str()]), 0);
    assert_exit(
        &meshtree(root, &["handoff", b.id.as_str(), "--set", "state=x"]),
        20,
    );
    let out = meshtree(root, &["render-agents"]);
    assert_exit(&out, 0);
    assert!(stdout(&out).starts_with("updated"), "{}", stdout(&out));
    assert_eq!(
        agents_md(root),
        expected_agents(&[ActiveSession::from_task(&a, Some(a_record))])
    );
    assert!(!agents_md(root).contains(b.id.as_str()));
}

#[test]
fn render_agents_creates_agents_md_and_preserves_crlf() {
    let repo = TempDir::repo_with_commit();
    let root = repo.path();
    assert_exit(&meshtree(root, &["init"]), 0);

    // No AGENTS.md yet: created with just the block.
    let out = meshtree(root, &["render-agents"]);
    assert_exit(&out, 0);
    assert_eq!(agents_md(root), render_agents_block(&[], DEFAULT_LINE_CAP));
    assert!(agents_md(root).contains("- (none)"));

    // CRLF files keep CRLF; a second render is a no-op.
    fs::write(root.join("AGENTS.md"), "# Rules\r\n\r\nUse CRLF.\r\n").unwrap();
    let task = spawn(root, &["crlf-task"]);
    assert_exit(&meshtree(root, &["render-agents"]), 0);
    let text = agents_md(root);
    assert!(text.starts_with(
        "# Rules\r\n\r\nUse CRLF.\r\n\r\n<!-- meshtree:active-sessions begin -->\r\n"
    ));
    assert!(
        !text.replace("\r\n", "").contains('\n'),
        "no bare LF: {text:?}"
    );
    assert!(text.contains(&format!("[{}] crlf-task", task.id)));
    let out = meshtree(root, &["render-agents"]);
    assert!(stdout(&out).starts_with("unchanged"), "{}", stdout(&out));
}

#[test]
fn render_agents_respects_max_lines() {
    let repo = TempDir::repo_with_commit();
    let root = repo.path();
    assert_exit(&meshtree(root, &["init"]), 0);
    let tasks: Vec<Task> = (0..5)
        .map(|i| spawn(root, &[&format!("cap-{i}")]))
        .collect();
    let config_path = root.join(".meshtree/config.toml");
    let template = fs::read_to_string(&config_path).unwrap();
    assert_eq!(template.matches("max_lines = 10").count(), 1, "{template}");
    let set_cap = |cap: &str| {
        let text = template.replace("max_lines = 10", &format!("max_lines = {cap}"));
        fs::write(&config_path, text).unwrap();
    };
    let entries = |text: &str| -> Vec<String> {
        text.lines()
            .filter(|l| l.starts_with("- "))
            .map(str::to_string)
            .collect()
    };

    // Cap 3: two whole entries and a summary line.
    set_cap("3");
    assert_exit(&meshtree(root, &["render-agents"]), 0);
    let lines = entries(&agents_md(root));
    assert_eq!(lines.len(), 3, "{lines:?}");
    assert!(lines[0].contains(&tasks[0].id.to_string()), "{lines:?}");
    assert!(lines[2].contains("and 3 more active sessions"), "{lines:?}");

    // The default cap of 10 lists all five.
    set_cap("10");
    assert_exit(&meshtree(root, &["render-agents"]), 0);
    let lines = entries(&agents_md(root));
    assert_eq!(lines.len(), 5, "{lines:?}");
    for task in &tasks {
        assert!(lines.iter().any(|l| l.contains(&task.id.to_string())));
    }

    // An invalid cap is a config error (MT-050, usage exit) and leaves the file alone.
    let before = agents_md(root);
    set_cap("0");
    let out = meshtree(root, &["render-agents"]);
    assert_exit(&out, 10);
    assert!(String::from_utf8_lossy(&out.stderr).contains("MT-050"));
    assert_eq!(agents_md(root), before);
}
