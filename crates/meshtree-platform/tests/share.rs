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
