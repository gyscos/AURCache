use std::fs::File;
use std::io::{self, Cursor, Read, Write};
use std::path::Path;
use tar::{Archive, Builder, Header};

use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;

/// Rewrite `db_archive` as a fresh tar.gz: copy across every existing entry
/// whose path `keep` rejects, then hand the new builder to `fill`, and write
/// the result back. Used by every mutation so add/remove cannot drift on how a
/// database archive is rewritten.
fn rewrite_archive<F, G>(db_archive: &Path, keep: F, fill: G) -> anyhow::Result<()>
where
    F: Fn(&str) -> bool,
    G: FnOnce(&mut Builder<GzEncoder<&mut Vec<u8>>>) -> anyhow::Result<()>,
{
    let mut new_archive_data = Vec::new();
    {
        let mut builder = if db_archive.exists() {
            let mut existing_archive_data = Vec::new();

            // Decode the existing archive
            File::open(db_archive)?.read_to_end(&mut existing_archive_data)?;
            let mut archive = Archive::new(GzDecoder::new(Cursor::new(existing_archive_data)));

            let enc = GzEncoder::new(&mut new_archive_data, Compression::default());
            let mut tar_builder = Builder::new(enc);

            for mut entry in archive.entries()?.flatten() {
                let path = entry.path()?.to_string_lossy().into_owned();
                if keep(path.as_ref()) {
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

        fill(&mut builder)?;
        builder.finish()?;
    }

    File::create(db_archive)?.write_all(&new_archive_data)?;
    Ok(())
}

pub fn remove_from_db_file(db_archive: &Path, dir_name: &str) -> anyhow::Result<()> {
    if !db_archive.exists() {
        return Ok(());
    }

    rewrite_archive(db_archive, |path| path.starts_with(dir_name), |_| Ok(()))
}

pub fn add_to_db_file(
    content: &str,
    dir_name: &str,
    file_name: &str,
    db_archive: &Path,
) -> anyhow::Result<()> {
    let target_file = format!("{dir_name}/{file_name}");

    rewrite_archive(
        db_archive,
        |path| path == dir_name || path == target_file,
        |builder| {
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
            header.set_path(&target_file)?;
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append(&header, content.as_bytes())?;

            Ok(())
        },
    )
}
