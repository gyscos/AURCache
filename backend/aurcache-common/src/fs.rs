//! Filesystem helpers shared by the server and the standalone worker.
//!
//! Behind the `fs` feature, which browser consumers leave off along with `db`.

use std::fs;
use std::path::Path;

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
/// a directory counts as the single symlink entry instead of being recursed
/// into. That keeps a symlink loop from recursing forever and stops an external
/// symlink target from leaking its contents into an entry's size.
#[must_use]
pub fn dir_size(path: impl AsRef<Path>) -> u64 {
    fn walk(path: &Path) -> u64 {
        let Ok(dir) = fs::read_dir(path) else {
            return 0;
        };
        dir.flatten()
            .map(|entry| match entry.metadata() {
                // `DirEntry::metadata` does not follow symlinks; `fs::metadata`
                // here would recurse into symlinked directories.
                Ok(data) if data.is_dir() => walk(&entry.path()),
                Ok(data) => data.len(),
                Err(_) => 0,
            })
            .sum()
    }

    walk(path.as_ref())
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
        fs::write(tmp.path().join("real/data"), [b'x'; 1_000_000]).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("real"), tmp.path().join("link")).unwrap();

        let total = dir_size(tmp.path());
        assert!(
            total < 1_000_000 + 2_000,
            "symlinked directory contents leaked into total: {total}"
        );
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
