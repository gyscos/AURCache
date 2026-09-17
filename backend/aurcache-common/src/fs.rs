//! Filesystem helpers shared by the server and the standalone worker.
//!
//! Behind the `fs` feature, which browser consumers leave off along with `db`.

use std::fs;
use std::path::{Path, PathBuf};

/// Total size in bytes of the files under `path`. Best-effort.
///
/// Entries that cannot be read are skipped and the walk continues, so a single
/// unreadable file costs its own size rather than the whole total. Both callers
/// — the repository size on the stats page and the worker's cache-eviction
/// budget — want the best number available and neither can act on an error, so
/// an unreadable or missing `path` is `0` rather than a failure.
///
/// Directory symlinks are *not* traversed: the `is_dir` decision uses
/// [`fs::DirEntry::metadata`], which does not follow symlinks, so a symlink to
/// a directory counts as the single symlink entry instead of being descended
/// into. That keeps a symlink loop from descending forever and stops an external
/// symlink target from leaking its contents into an entry's size.
///
/// The walk is iterative over an explicit stack: recursion depth would follow
/// directory depth, and these roots include extracted archives whose nesting
/// nobody controls.
#[must_use]
pub fn dir_size(path: impl AsRef<Path>) -> u64 {
    let mut total = 0;
    let mut stack = vec![path.as_ref().to_path_buf()];
    while let Some(path) = stack.pop() {
        let Ok(dir) = fs::read_dir(&path) else {
            continue;
        };
        for entry in dir.flatten() {
            match entry.metadata() {
                // `DirEntry::metadata` does not follow symlinks; `fs::metadata`
                // here would descend into symlinked directories.
                Ok(data) if data.is_dir() => stack.push(entry.path()),
                Ok(data) => total += data.len(),
                Err(_) => {}
            }
        }
    }
    total
}

/// Directory holding one log file per build.
///
/// Relative by default, like the repository and the source cache, so a
/// deployment that mounts a single data directory gets this inside it.
///
/// Lives here rather than beside the logger because the backfill migration in
/// `aurcache-db` needs it too, and `aurcache-db` cannot depend on the crate the
/// logger lives in without a cycle.
#[must_use]
pub fn build_log_root() -> PathBuf {
    std::env::var("AURCACHE_BUILD_LOG_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./build_logs"))
}

/// One path segment, with anything that is not a plain name flattened.
///
/// A pkgbase from the AUR cannot contain a separator, but `AURCache` also builds
/// from git, where the pkgbase comes from a parsed PKGBUILD and is whatever
/// that file says. `.` and `..` are handled separately because they survive
/// character filtering intact and are still traversal.
///
/// Distinct names could in principle collapse onto one segment; that needs a
/// pkgbase containing a character the AUR does not allow, and the cost is a
/// shared log directory rather than anything escaping the root.
fn sanitize_segment(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+' | '@') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        format!("_{cleaned}")
    } else {
        cleaned
    }
}

/// Directory holding one package's build logs.
#[must_use]
pub fn build_log_dir(pkgbase: &str) -> PathBuf {
    build_log_root().join(sanitize_segment(pkgbase))
}

/// Path of one build's log, keyed the way the API is.
///
/// `<pkgbase>/<number>.log` rather than the build id: `<pkgbase>/<number>` is
/// a build's public identity -- what the URLs, the CLI and the screens all use
/// -- and the id is an internal key none of them show. Naming the files after
/// the identity means someone reading the directory sees what they see
/// everywhere else, and deleting a package's logs is one `remove_dir_all`.
#[must_use]
pub fn build_log_path(pkgbase: &str, number: i32) -> PathBuf {
    build_log_dir(pkgbase).join(format!("{number}.log"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plain tree sums its files recursively. Directories contribute only
    /// what is inside them, never their own inode size.
    #[test]
    fn plain_tree_sums_all_files() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("a/b")).unwrap();
        fs::write(tmp.path().join("one"), [b'1'; 10]).unwrap();
        fs::write(tmp.path().join("a/b/two"), [b'2'; 20]).unwrap();
        assert_eq!(dir_size(tmp.path()), 30);
    }

    /// A directory symlink is counted as one entry, not traversed — the
    /// property the worker's cache sizing relies on. Uses a 1 MB marker file:
    /// if `link` were followed, the total would include its contents twice.
    #[cfg(unix)]
    #[test]
    fn directory_symlink_is_counted_once_not_traversed() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("real")).unwrap();
        fs::write(tmp.path().join("real/data"), vec![b'x'; 1_000_000]).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("real"), tmp.path().join("link")).unwrap();

        let total = dir_size(tmp.path());
        assert!(
            total < 1_000_000 + 2_000,
            "symlinked directory contents leaked into total: {total}"
        );
    }

    /// Depth nobody controls must not become stack depth. Paths cap nesting
    /// around 1500 levels (`PATH_MAX`), which a recursive walk survives on a
    /// normal stack — so this runs the walk on a 64 KiB thread stack, where
    /// recursion per level overflows and the explicit stack does not.
    #[test]
    fn deep_nesting_does_not_overflow_the_stack() {
        let tmp = tempfile::tempdir().unwrap();
        let mut dir = tmp.path().to_path_buf();
        for _ in 0..1500 {
            dir.push("d");
        }
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("deep"), [b'x'; 7]).unwrap();
        let root = tmp.path().to_path_buf();
        let total = std::thread::Builder::new()
            .stack_size(64 * 1024)
            .spawn(move || dir_size(&root))
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(total, 7);
    }

    /// A symlink loop terminates: each symlink is one entry, counted once and
    /// never re-entered.
    #[cfg(unix)]
    #[test]
    fn symlink_loop_terminates() {
        let tmp = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(tmp.path().join("b"), tmp.path().join("a")).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("a"), tmp.path().join("b")).unwrap();

        let total = dir_size(tmp.path());
        assert!(
            total < 4096,
            "loop should be counted once, not resolved: {total}"
        );
    }

    /// The best-effort contract: an unreadable subtree costs only itself. The
    /// readable sibling is still counted, rather than the whole walk collapsing
    /// to zero the way a `Result` plus `unwrap_or(0)` used to make it.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_subtree_does_not_zero_the_total() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("readable")).unwrap();
        fs::write(tmp.path().join("readable/data"), [b'x'; 1000]).unwrap();

        let locked = tmp.path().join("locked");
        fs::create_dir(&locked).unwrap();
        fs::write(locked.join("hidden"), [b'y'; 1000]).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

        // Running as root defeats the permission bits, so `locked` may or may
        // not be counted; what must hold either way is that `readable` is.
        let total = dir_size(tmp.path());
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            total >= 1000,
            "a failed subtree swallowed its readable sibling: {total}"
        );
    }

    /// A missing directory is zero, not a panic — the stats page asks for
    /// `repo/` before anything has been built.
    #[test]
    fn a_missing_directory_is_zero() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(dir_size(tmp.path().join("nope")), 0);
    }
}
