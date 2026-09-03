use aurcache_db::files;
use std::fs;
use std::path::PathBuf;
use tracing::{info, warn};

/// Take a built artifact out of the repository: off the disk, and out of the
/// pacman databases that index it.
///
/// Deliberately *only* the file, with no row deletion paired to it: the two
/// cannot be undone together. Every caller deletes the rows inside a
/// transaction and calls this after it commits -- a rolled-back transaction can
/// put a row back, and nothing can put back a file. There was once a
/// `try_remove_archive_file` that did both at once, which made getting that
/// order wrong the path of least resistance; the ordering is the caller's to
/// get right, so it is the caller that spells it out.
///
/// Both steps are best-effort and logged: the row is the record that matters,
/// and a file left behind is tidier than a row pointing at nothing.
pub fn forget_archive_file(file: &files::Model) {
    let platform_repo = PathBuf::from(format!("./repo/{}", file.platform));
    let file_path = platform_repo.join(&file.filename);

    if let Err(e) = pacman_repo_utils::repo_remove::repo_remove(
        &file.filename,
        &platform_repo.join("repo.db.tar.gz"),
        &platform_repo.join("repo.files.tar.gz"),
    ) {
        warn!("Failed to run repo-remove for {}: {e}", file.filename);
    }

    if let Err(e) = fs::remove_file(&file_path) {
        warn!("Failed to remove package file {}: {e}", file_path.display());
    } else {
        info!("Removed old file: {}", file_path.display());
    }
}
