use std::fs;
use std::path::PathBuf;

/// Total size of the files under `path`.
///
/// Directory symlinks are *not* traversed: like the worker's original cache
/// sizing, the `is_dir` decision uses `DirEntry` metadata (lstat semantics),
/// so a symlink to a directory is counted as the single symlink entry rather
/// than recursed into. That keeps a symlink loop from recursing forever and an
/// external symlink target from leaking its contents into an entry's size.
pub fn dir_size(path: impl Into<PathBuf>) -> anyhow::Result<u64> {
    fn dir_size(mut dir: fs::ReadDir) -> anyhow::Result<u64> {
        dir.try_fold(0, |acc, file| {
            let file = file?;
            // `DirEntry::metadata` does not follow symlinks; using `fs::metadata`
            // here would recurse into symlinked directories (see tests below).
            let size = match file.metadata()? {
                data if data.is_dir() => dir_size(fs::read_dir(file.path())?)?,
                data => data.len(),
            };
            Ok(acc + size)
        })
    }

    dir_size(fs::read_dir(path.into())?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory symlink must be counted as one entry, not traversed — the
    /// property the worker's cache sizing relied on. Uses a 1 MB marker file:
    /// if `/link` were followed, the total would include its contents too.
    #[test]
    fn directory_symlink_is_counted_once_not_traversed() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("real")).unwrap();
        fs::write(tmp.path().join("real/data"), [b'x'; 1_000_000]).unwrap();

        #[cfg(unix)]
        std::os::unix::fs::symlink(tmp.path().join("real"), tmp.path().join("link")).unwrap();

        let with_link = dir_size(tmp.path()).unwrap();
        // Without the marker's copy leaking in, the total stays under ~2 KB of
        // directory/symlink overhead plus the single marker copy.
        assert!(
            with_link < 1_000_000 + 2_000,
            "symlinked directory contents leaked into total: {with_link}"
        );
    }

    /// A symlink loop must terminate: each symlink is one entry, so a/b are
    /// counted once each and never re-entered.
    #[test]
    fn symlink_loop_terminates() {
        let tmp = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(tmp.path().join("b"), tmp.path().join("a")).unwrap();
            std::os::unix::fs::symlink(tmp.path().join("a"), tmp.path().join("b")).unwrap();
        }
        let total = dir_size(tmp.path()).unwrap();
        assert!(
            total < 4096,
            "loop should be counted once, not resolved: {total}"
        );
    }

    /// The same property for the non-symlink path is unchanged: a plain tree
    /// sums its files recursively.
    #[test]
    fn plain_tree_sums_all_files() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("a/b")).unwrap();
        fs::write(tmp.path().join("one"), [b'1'; 10]).unwrap();
        fs::write(tmp.path().join("a/b/two"), [b'2'; 20]).unwrap();
        let total = dir_size(tmp.path()).unwrap();
        assert!(
            (30..1000).contains(&total),
            "2000-byte markers must be fully counted: {total}"
        );
    }
}
