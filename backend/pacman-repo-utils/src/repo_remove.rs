use crate::repo_database::db::remove_from_db_file;
use std::path::Path;

pub fn repo_remove(filename: &str, db_archive: &Path, files_archive: &Path) -> anyhow::Result<()> {
    let (dir_name, _) = split_last_occurrence(filename, '-');
    remove_from_db_file(db_archive, dir_name)?;
    remove_from_db_file(files_archive, dir_name)?;
    Ok(())
}

fn split_last_occurrence(s: &str, delimiter: char) -> (&str, &str) {
    match s.rfind(delimiter) {
        Some(pos) => (&s[..pos], &s[pos + delimiter.len_utf8()..]),
        None => (s, ""),
    }
}
