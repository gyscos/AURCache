use crate::repo_database::db::remove_from_db_file;
use std::path::Path;

pub fn repo_remove(filename: &str, db_archive: &Path, files_archive: &Path) -> anyhow::Result<()> {
    let dir_name = filename.rsplit_once('-').unwrap_or((filename, "")).0;
    remove_from_db_file(db_archive, dir_name)?;
    remove_from_db_file(files_archive, dir_name)?;
    Ok(())
}
