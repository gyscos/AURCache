use aurcache_db::files;
use sea_orm::{DatabaseTransaction, ModelTrait};
use std::fs;
use std::path::PathBuf;
use tracing::{info, warn};

pub async fn try_remove_archive_file(
    file: files::Model,
    db: &DatabaseTransaction,
) -> anyhow::Result<()> {
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

    file.delete(db).await?;

    Ok(())
}
