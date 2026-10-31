//! `[[share]]` applied by `meshtree spawn` (`SPEC.md` §8 `spawn` step 3, §11,
//! §18 spawn budget, §20 Week 4).

mod common;

use common::{assert_exit, commit, git, meshtree, DaemonProc, TempDir};
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

const PACKAGES: usize = 100;
const FILES_PER_PACKAGE: usize = 20;

fn config(strategy: &str) -> String {
    format!(
        r#"[project]
base_branch = "main"
validate = ["git --version"]
promote_validate = ["git --version"]

[[share]]
source = "node_modules"
strategy = "{strategy}"

[[share]]
source = "target"
strategy = "none"
"#
    )
}

/// A populated `node_modules` in the primary worktree (gitignored).
fn populate_node_modules(root: &Path) {
    for p in 0..PACKAGES {
        let dir = root.join("node_modules").join(format!("pkg-{p:03}"));
        fs::create_dir_all(&dir).unwrap();
        for f in 0..FILES_PER_PACKAGE {
            fs::write(dir.join(format!("f{f:02}.js")), format!("// {p}/{f}\n")).unwrap();
        }
    }
}

fn count_files(dir: &Path) -> usize {
    fs::read_dir(dir)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            if e.file_type().unwrap().is_dir() {
                count_files(&e.path())
            } else {
                1
            }
        })
        .sum()
}

fn spawn(root: &Path, name: &str) -> (String, String, String) {
    let out = meshtree(root, &["spawn", name]);
    assert_exit(&out, 0);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let id = stdout
        .lines()
        .find_map(|l| l.strip_prefix("task_id: "))
        .unwrap()
        .to_string();
    (
        id,
        stdout,
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn spawn_shared_node_modules_under_5s() {
    let repo = TempDir::repo_with_commit();
    let root = repo.path();
    fs::write(root.join(".gitignore"), "node_modules/\n").unwrap();
    git(root, &["add", ".gitignore"]);
    commit(root, "ignore node_modules");
    assert_exit(&meshtree(root, &["init"]), 0);
    fs::write(root.join(".meshtree/config.toml"), config("auto")).unwrap();
    populate_node_modules(root);
    let total = PACKAGES * FILES_PER_PACKAGE;
    assert_eq!(count_files(&root.join("node_modules")), total);

    // spawn links node_modules (junction on Windows, symlink elsewhere).
    let start = Instant::now();
    let (a, stdout, _) = spawn(root, "task-a");
    let elapsed = start.elapsed();
    assert!(elapsed < Duration::from_secs(5), "spawn took {elapsed:?}");
    assert!(stdout.contains("share: shared node_modules"), "{stdout}");
    let wt = root.join(".meshtree/worktrees").join(&a);
    let link = wt.join("node_modules");
    assert!(fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(
        count_files(&link),
        total,
        "the link resolves to the full tree"
    );
    assert_eq!(
        fs::read_to_string(link.join("pkg-042/f07.js"))
            .unwrap()
            .replace("\r\n", "\n"),
        "// 42/7\n"
    );
    assert!(!wt.join("target").exists(), "strategy none shares nothing");
    // The worktree sees the link as ignored, not as a change.
    assert_eq!(git(&wt, &["status", "--porcelain"]), "");

    // abort removes the worktree but never the shared tree behind the link.
    assert_exit(&meshtree(root, &["abort", &a]), 0);
    assert!(!wt.exists());
    assert_eq!(count_files(&root.join("node_modules")), total);

    // Neither does a merge's worktree cleanup.
    let (b, _, _) = spawn(root, "task-b");
    let wt_b = root.join(".meshtree/worktrees").join(&b);
    fs::write(wt_b.join("b.txt"), "b\n").unwrap();
    git(&wt_b, &["add", "b.txt"]);
    commit(&wt_b, "b");
    let daemon = DaemonProc::start(root);
    assert_exit(&meshtree(root, &["collect", &b]), 0);
    daemon.stop(root);
    assert!(!wt_b.exists());
    assert_eq!(count_files(&root.join("node_modules")), total);

    // An unsupported strategy degrades to no sharing (MT-040), spawn succeeds.
    let unsupported = if cfg!(target_os = "macos") {
        "junction"
    } else {
        "clonefile"
    };
    fs::write(root.join(".meshtree/config.toml"), config(unsupported)).unwrap();
    let (c, _, stderr) = spawn(root, "task-c");
    assert!(stderr.contains("MT-040"), "{stderr}");
    let wt_c = root.join(".meshtree/worktrees").join(&c);
    assert!(wt_c.exists());
    assert!(fs::symlink_metadata(wt_c.join("node_modules")).is_err());
}
