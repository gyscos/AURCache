use std::fs::File;
use std::io::{self, Cursor, Read, Write};
use std::path::Path;
use tar::{Archive, Builder, Header};

use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;

/// Rewrite `db_archive` as a fresh tar.gz: copy across every existing entry
/// except those `superseded` selects, then hand the new builder to `fill`, and
/// write the result back. Used by every mutation so add/remove cannot drift on
/// how a database archive is rewritten.
///
/// `superseded` takes a [`Path`], not a `&str`, so both callers match on whole
/// path components rather than on bytes. Entry paths here are `{pkgname}-{ver}`
/// directories, where one name is routinely a byte prefix of another: with
/// `str::starts_with`, removing `foo-1.0-1` also drops the `foo-1.0-10` a
/// rebuild just added.
fn rewrite_archive<F, G>(db_archive: &Path, superseded: F, fill: G) -> anyhow::Result<()>
where
    F: Fn(&Path) -> bool,
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
                if superseded(&entry.path()?) {
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
        |path| path == Path::new(dir_name) || path == Path::new(&target_file),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Every entry path in `db_archive`, for asserting what a rewrite kept.
    fn entry_paths(db_archive: &Path) -> Vec<String> {
        let mut data = Vec::new();
        File::open(db_archive)
            .unwrap()
            .read_to_end(&mut data)
            .unwrap();
        let mut archive = Archive::new(GzDecoder::new(Cursor::new(data)));
        archive
            .entries()
            .unwrap()
            .flatten()
            .map(|e| e.path().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    /// Removing one package must not take its version-prefixed neighbours with
    /// it. `foo-1.0-1` is a byte prefix of `foo-1.0-10`, so a `str::starts_with`
    /// predicate deletes the entry a rebuild just added — the package silently
    /// disappears from the repository database while its file stays on disk.
    #[test]
    fn removing_a_pkgrel_does_not_remove_its_longer_neighbour() {
        let tmp = tempfile::tempdir().unwrap();
        let db_archive = tmp.path().join("repo.db.tar.gz");

        add_to_db_file("old", "foo-1.0-1", "desc", &db_archive).unwrap();
        add_to_db_file("new", "foo-1.0-10", "desc", &db_archive).unwrap();

        remove_from_db_file(&db_archive, "foo-1.0-1").unwrap();

        let paths = entry_paths(&db_archive);
        assert!(
            paths.iter().any(|p| p.starts_with("foo-1.0-10")),
            "the rebuilt package was removed along with the old pkgrel: {paths:?}"
        );
        assert!(
            !paths.iter().any(|p| p == "foo-1.0-1/desc"),
            "the old pkgrel survived removal: {paths:?}"
        );
    }

    /// Re-adding a package replaces its entries rather than stacking
    /// duplicates, which is what the equality half of the predicate is for.
    #[test]
    fn re_adding_replaces_rather_than_duplicates() {
        let tmp = tempfile::tempdir().unwrap();
        let db_archive = tmp.path().join("repo.db.tar.gz");

        add_to_db_file("first", "foo-1.0-1", "desc", &db_archive).unwrap();
        add_to_db_file("second", "foo-1.0-1", "desc", &db_archive).unwrap();

        let paths = entry_paths(&db_archive);
        assert_eq!(
            paths.iter().filter(|p| p.starts_with("foo-1.0-1")).count(),
            2,
            "expected exactly one directory and one desc entry: {paths:?}"
        );
    }
}
