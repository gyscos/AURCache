use std::fs::File;
use std::io::{self, Cursor, Read, Write};
use std::path::Path;
use tar::{Archive, Builder, Header};

use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;

pub fn remove_from_db_file(db_archive: &Path, dir_name: &str) -> anyhow::Result<()> {
    if !db_archive.exists() {
        return Ok(());
    }

    let mut new_archive_data = Vec::new();
    {
        let mut existing_archive_data = Vec::new();
        File::open(db_archive)?.read_to_end(&mut existing_archive_data)?;

        // Decode the existing archive
        let mut archive = Archive::new(GzDecoder::new(Cursor::new(existing_archive_data)));

        let enc = GzEncoder::new(&mut new_archive_data, Compression::default());
        let mut tar_builder = Builder::new(enc);

        // Copy existing entries to the new archive
        for mut entry in archive.entries()?.flatten() {
            // skip file and folder we want to delete
            if entry.header().path()?.starts_with(dir_name) {
                continue;
            }
            tar_builder.append(&entry.header().clone(), &mut entry)?;
        }

        tar_builder.finish()?;
    }

    File::create(db_archive)?.write_all(&new_archive_data)?;
    Ok(())
}

pub fn add_to_db_file(
    content: &str,
    dir_name: &str,
    file_name: &str,
    db_archive: &Path,
) -> anyhow::Result<()> {
    let mut new_archive_data = Vec::new();
    {
        let mut builder = if db_archive.exists() {
            let mut existing_archive_data = Vec::new();

            // Decode the existing archive
            File::open(db_archive)?.read_to_end(&mut existing_archive_data)?;
            let mut archive = Archive::new(GzDecoder::new(Cursor::new(existing_archive_data)));

            let enc = GzEncoder::new(&mut new_archive_data, Compression::default());
            let mut tar_builder = Builder::new(enc);

            let target_file = format!("{dir_name}/{file_name}");

            // Copy all entries *except* the ones we will replace
            for mut entry in archive.entries()?.flatten() {
                let path = entry.path()?.to_string_lossy().to_string();

                // Skip the directory and file that will be replaced
                if path == dir_name || path == target_file {
                    continue;
                }

                tar_builder.append(&entry.header().clone(), &mut entry)?;
            }
            tar_builder
        } else {
            // Create a new archive
            let encoder = GzEncoder::new(&mut new_archive_data, Compression::default());

            Builder::new(encoder)
        };

        // Add folder (replacing old one)
        let mut header = Header::new_gnu();
        header.set_path(dir_name)?;
        header.set_entry_type(tar::EntryType::Directory);
        header.set_mode(0o755);
        header.set_size(0);
        header.set_cksum();
        builder.append(&header, io::empty())?;

        // Add file (replacing old one)
        let mut header = Header::new_gnu();
        header.set_path(format!("{dir_name}/{file_name}"))?;
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append(&header, content.as_bytes())?;

        builder.finish()?;
    }

    File::create(db_archive)?.write_all(&new_archive_data)?;
    Ok(())
}
