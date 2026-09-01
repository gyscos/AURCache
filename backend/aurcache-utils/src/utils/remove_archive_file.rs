use aurcache_db::files;
use sea_orm::{DatabaseTransaction, ModelTrait};
use std::fs;
use std::path::PathBuf;
use tracing::{info, warn};

pub async fn try_remove_archive_file(
    file: files::Model,
    db: &DatabaseTransaction,
) -> anyhow::Result<()> {
    forget_archive_file(&file);
    file.delete(db).await?;

    Ok(())
}

/// Take a built artifact out of the repository: off the disk, and out of the
/// pacman databases that index it.
///
/// Split from the row deletion because the two cannot be undone together. A
/// caller that deletes rows inside a transaction has to do this *after* it
/// commits -- a rolled-back transaction can put a row back, and nothing can put
/// back a file. Both steps are best-effort and logged: the row is the record
/// that matters, and a file left behind is tidier than a row pointing at
/// nothing.
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
