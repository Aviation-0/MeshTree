//! Dependency sharing links (`SPEC.md` §2 G6, §6 `[[share]]`, §11 Trap 3,
//! §16 `MT-040`).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use meshtree_core::{ShareConfig, ShareStrategy};
use meshtree_platform::share::{
    apply, apply_all, resolve, unlink, Applied, LinkKind, ERROR_CODE_SHARE_FAILED,
};

struct TempDir(PathBuf);

static COUNTER: AtomicU64 = AtomicU64::new(0);

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "mt-share-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // Remove links first so cleanup never follows them.
        for entry in fs::read_dir(&self.0).into_iter().flatten().flatten() {
            let _ = unlink(&entry.path().join("node_modules"));
        }
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn share(source: &str, strategy: ShareStrategy) -> ShareConfig {
    ShareConfig {
        source: source.to_string(),
        strategy,
    }
}

/// `primary/node_modules/pkg/index.js` and an empty `worktree/`.
fn layout(tmp: &TempDir) -> (PathBuf, PathBuf) {
    let primary = tmp.path().join("primary");
    let worktree = tmp.path().join("worktree");
    fs::create_dir_all(primary.join("node_modules/pkg")).unwrap();
    fs::write(
        primary.join("node_modules/pkg/index.js"),
        "module.exports = 1;\n",
    )
    .unwrap();
    fs::create_dir_all(&worktree).unwrap();
    (primary, worktree)
}

#[test]
fn share_creates_platform_link_under_100ms() {
    let tmp = TempDir::new();
    let (primary, worktree) = layout(&tmp);

    // `auto` = junction on Windows (no admin rights), symlink elsewhere.
    let out = apply(
        &primary,
        &worktree,
        &share("node_modules", ShareStrategy::Auto),
    );
    let expected = resolve(ShareStrategy::Auto).unwrap();
    assert_eq!(out.applied, Applied::Linked(expected), "{out}");
    assert_eq!(out.code(), None);
    assert!(
        out.elapsed < Duration::from_millis(100),
        "link took {:?} (§11 budget: < 100 ms)",
        out.elapsed
    );
    let link = worktree.join("node_modules");
    assert!(fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(
        fs::read_to_string(link.join("pkg/index.js")).unwrap(),
        "module.exports = 1;\n"
    );
    // Shared, not copied: a write through the link lands in the primary.
    fs::write(link.join("pkg/new.js"), "x").unwrap();
    assert!(primary.join("node_modules/pkg/new.js").exists());

    // Unlinking removes only the link; the shared directory survives.
    assert!(unlink(&link).unwrap());
    assert!(fs::symlink_metadata(&link).is_err());
    assert!(primary.join("node_modules/pkg/index.js").exists());
    assert!(!unlink(&link).unwrap(), "absent: nothing to unlink");
    assert!(
        !unlink(&primary.join("node_modules")).unwrap(),
        "never a real dir"
    );

    // Skips: `none`, a missing source, a path the worktree already has.
    let outs = apply_all(
        &primary,
        &worktree,
        &[
            share("node_modules", ShareStrategy::None),
            share("missing", ShareStrategy::Auto),
        ],
    );
    assert!(outs
        .iter()
        .all(|o| matches!(o.applied, Applied::Skipped(_))));
    fs::create_dir_all(worktree.join("node_modules")).unwrap();
    let out = apply(
        &primary,
        &worktree,
        &share("node_modules", ShareStrategy::Auto),
    );
    assert!(matches!(out.applied, Applied::Skipped(_)), "{out}");
    fs::remove_dir(worktree.join("node_modules")).unwrap();

    // macOS: an APFS copy-on-write clone is an independent tree.
    if cfg!(target_os = "macos") {
        let out = apply(
            &primary,
            &worktree,
            &share("node_modules", ShareStrategy::Clonefile),
        );
        assert_eq!(out.applied, Applied::Linked(LinkKind::Clonefile), "{out}");
        assert!(
            out.elapsed < Duration::from_millis(100),
            "{:?}",
            out.elapsed
        );
        let clone = worktree.join("node_modules");
        assert!(!fs::symlink_metadata(&clone)
            .unwrap()
            .file_type()
            .is_symlink());
        fs::write(clone.join("pkg/only-in-clone.js"), "y").unwrap();
        assert!(!primary.join("node_modules/pkg/only-in-clone.js").exists());
    }
}

#[test]
fn share_fallback_to_none_emits_mt040() {
    let tmp = TempDir::new();
    let (primary, worktree) = layout(&tmp);

    // A strategy this OS cannot provide.
    let unsupported = if cfg!(target_os = "macos") {
        ShareStrategy::Junction
    } else {
        ShareStrategy::Clonefile
    };
    let out = apply(&primary, &worktree, &share("node_modules", unsupported));
    match &out.applied {
        Applied::Fallback { requested, error } => {
            assert_eq!(*requested, unsupported);
            assert!(!error.is_empty());
        }
        other => panic!("expected MT-040 fallback, got {other:?}"),
    }
    assert_eq!(out.code(), Some(ERROR_CODE_SHARE_FAILED));
    assert!(out.to_string().contains("MT-040"), "{out}");
    assert!(out.to_string().contains("fell back to none"), "{out}");
    // Nothing was left behind; the source is intact.
    assert!(fs::symlink_metadata(worktree.join("node_modules")).is_err());
    assert!(primary.join("node_modules/pkg/index.js").exists());

    // After a fallback the platform default still works.
    let out = apply(
        &primary,
        &worktree,
        &share("node_modules", ShareStrategy::Auto),
    );
    assert!(matches!(out.applied, Applied::Linked(_)), "{out}");
}

/// What bundlers and watchers observe through one share mechanism
/// (`docs/compatibility.md` records the matrix this test verifies).
#[derive(Debug, PartialEq, Eq)]
struct Probe {
    /// `lstat` reports a link (symlink or junction).
    is_link: bool,
    /// `readlink` names the primary's directory.
    read_link_to_primary: bool,
    /// `realpath` of a file under the link is inside the primary worktree
    /// (Node.js without `--preserve-symlinks`, webpack `resolve.symlinks`,
    /// esbuild/Vite without `preserveSymlinks`).
    realpath_in_primary: bool,
    /// `<link>/..` physically resolves to the primary worktree, not the task
    /// worktree (lexical vs physical parent).
    dotdot_is_primary: bool,
    /// A write through the link appears in the primary (shared state:
    /// `npm install` in a task mutates every task's dependencies).
    writes_shared: bool,
    /// A nested link inside the shared tree (pnpm's `node_modules/<pkg>` →
    /// `.pnpm/…`) still resolves to the package contents.
    nested_link_resolves: bool,
}

fn probe(primary: &Path, worktree: &Path, kind: LinkKind) -> Probe {
    let link = worktree.join("node_modules");
    let canon_primary = fs::canonicalize(primary).unwrap();
    let canon_worktree = fs::canonicalize(worktree).unwrap();
    let real_file = fs::canonicalize(link.join("pkg/index.js")).unwrap();
    let probe_name = format!("probe-{kind}.js");
    fs::write(link.join("pkg").join(&probe_name), "w").unwrap();
    let probe = Probe {
        is_link: fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink(),
        read_link_to_primary: fs::read_link(&link)
            .ok()
            .and_then(|target| fs::canonicalize(target).ok())
            .is_some_and(|t| t == canon_primary.join("node_modules")),
        realpath_in_primary: real_file.starts_with(&canon_primary),
        dotdot_is_primary: fs::canonicalize(link.join("..")).unwrap() == canon_primary,
        writes_shared: primary.join("node_modules/pkg").join(&probe_name).exists(),
        nested_link_resolves: fs::read_to_string(link.join("pnpm-pkg/index.js"))
            .is_ok_and(|s| s == "pnpm\n"),
    };
    assert!(
        real_file.starts_with(&canon_primary) || real_file.starts_with(&canon_worktree),
        "{real_file:?}"
    );
    let _ = fs::remove_file(primary.join("node_modules/pkg").join(&probe_name));
    probe
}

#[test]
fn share_junction_realpath_probe_matrix() {
    let tmp = TempDir::new();
    let (primary, _) = layout(&tmp);
    // pnpm layout: node_modules/pnpm-pkg is itself a link into the virtual
    // store (`node_modules/.pnpm/...`), created with the platform default.
    let store = primary.join("node_modules/.pnpm/pnpm-pkg@1.0.0/node_modules");
    fs::create_dir_all(store.join("pnpm-pkg")).unwrap();
    fs::write(store.join("pnpm-pkg/index.js"), "pnpm\n").unwrap();
    let inner = apply(
        &store,
        &primary.join("node_modules"),
        &share("pnpm-pkg", ShareStrategy::Auto),
    );
    assert!(matches!(inner.applied, Applied::Linked(_)), "{inner}");

    let linked = Probe {
        is_link: true,
        read_link_to_primary: true,
        realpath_in_primary: true,
        dotdot_is_primary: true,
        writes_shared: true,
        nested_link_resolves: true,
    };
    let mut cases = vec![(ShareStrategy::Auto, linked)];
    if cfg!(target_os = "macos") {
        cases.push((
            ShareStrategy::Clonefile,
            Probe {
                is_link: false,
                read_link_to_primary: false,
                realpath_in_primary: false,
                dotdot_is_primary: false,
                writes_shared: false,
                nested_link_resolves: true,
            },
        ));
    }

    let mut matrix = Vec::new();
    for (i, (strategy, expected)) in cases.into_iter().enumerate() {
        let worktree = tmp.path().join(format!("wt-{i}"));
        fs::create_dir_all(&worktree).unwrap();
        let out = apply(&primary, &worktree, &share("node_modules", strategy));
        let Applied::Linked(kind) = out.applied else {
            panic!("{strategy:?}: {out}");
        };
        let got = probe(&primary, &worktree, kind);
        matrix.push(format!("{kind}: {got:?}"));
        assert_eq!(got, expected, "{strategy:?} via {kind}");
        if kind != LinkKind::Clonefile {
            assert!(unlink(&worktree.join("node_modules")).unwrap());
        } else {
            let _ = unlink(&worktree.join("node_modules/pnpm-pkg"));
        }
    }
    let _ = unlink(&primary.join("node_modules/pnpm-pkg"));
    println!("{}", matrix.join("\n"));

    // `strategy = "none"` is the escape hatch: nothing to resolve through.
    let worktree = tmp.path().join("wt-none");
    fs::create_dir_all(&worktree).unwrap();
    let out = apply(
        &primary,
        &worktree,
        &share("node_modules", ShareStrategy::None),
    );
    assert!(matches!(out.applied, Applied::Skipped(_)), "{out}");
    assert!(fs::symlink_metadata(worktree.join("node_modules")).is_err());
}
