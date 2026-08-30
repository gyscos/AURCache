use anyhow::Context;
use flate2::Compression;
use flate2::write::GzEncoder;
use std::fs;
use std::fs::File;
use std::os::unix::fs::symlink;
use std::path::Path;
use tracing::info;

pub fn init_repo(path: &Path, name: &str) -> anyhow::Result<()> {
    if repo_exists(path, name) {
        info!(
            "Pacman repo '{}' archive already exists at path '{}'",
            name,
            path.display()
        );
        return Ok(());
    }

    // create repo folder
    info!("Initializing empty pacman Repo archive");
    fs::create_dir_all(path)?;

    create_empty_archive(path, name, "db")?;
    create_empty_archive(path, name, "files")?;
    Ok(())
}

/// check if every repo archive and its symlink already exist
fn repo_exists(path: &Path, name: &str) -> bool {
    ["db", "files"].into_iter().all(|suffix| {
        get_archive_names(name, suffix)
            .into_iter()
            .all(|file| path.join(file).exists())
    })
}

/// assembles the filenames of the archive and its symlink, in that order
fn get_archive_names(name: &str, suffix: &str) -> [String; 2] {
    [
        format!("{name}.{suffix}.tar.gz"),
        format!("{name}.{suffix}"),
    ]
}

/// create empty archive and corresponding symlink
fn create_empty_archive(path: &Path, name: &str, suffix: &str) -> anyhow::Result<()> {
    let [archive_file_name, symlink_name] = get_archive_names(name, suffix);
    let archive_path = path.join(&archive_file_name);
    let symlink_path = path.join(&symlink_name);

    let tar_gz = File::create(archive_path)?;
    let enc = GzEncoder::new(tar_gz, Compression::default());
    let mut tar = tar::Builder::new(enc);
    tar.finish().context("failed to create repo archive")?;
    symlink(archive_file_name, symlink_path).context("failed to create repo symlink")?;
    Ok(())
}
