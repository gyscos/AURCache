use anyhow::{anyhow, bail};

use crate::pkginfo::parser::Pkginfo;
use crate::repo_database::db::add_to_db_file;
use crate::repo_database::desc::Desc;
use liblzma::bufread::XzDecoder;
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{BufReader, Read};
use std::path::Path;
use tar::Archive;
use tracing::{debug, error, warn};
use zstd::stream::read::Decoder as ZstdDecoder;

pub fn repo_add(pkgfile: &Path, db_archive: &Path, files_archive: &Path) -> anyhow::Result<()> {
    let mut files = vec![];
    let mut pkginfo = Pkginfo::new();

    // Path to the .tar.zst file
    let file = File::open(pkgfile)?;
    let ext = pkgfile.extension().and_then(|e| e.to_str());

    // Select the appropriate decompression method
    let decompressor: Box<dyn Read> = match ext {
        Some("zst") => {
            let decoder = ZstdDecoder::new(BufReader::new(file))?;
            Box::new(decoder)
        }
        Some("xz") => {
            let decoder = XzDecoder::new(BufReader::new(file));
            Box::new(decoder)
        }
        _ => {
            bail!("Unsupported file type");
        }
    };
    let mut archive = Archive::new(decompressor);

    // Iterate over the entries in the tar archive
    let pkgpath = pkgfile.display();
    for entry in archive.entries()? {
        match entry {
            Ok(entry) => {
                if let Ok(path) = entry.path() {
                    let name = path.display().to_string();
                    if !name.starts_with('.') {
                        files.push(name);
                    }

                    if path == Path::new(".PKGINFO") {
                        debug!("Found .PKGINFO file in '{pkgpath}'.");
                        pkginfo.parse(entry)?;
                    }
                }
            }
            Err(e) => warn!("Error reading entry: {e:?}"),
        }
    }

    if !pkginfo.valid() {
        error!("Invalid package file '{pkgpath}'.");
        bail!("Invalid package file");
    }

    // Compute base64'd PGP signature
    debug!("Setting signature for '{pkgpath}'.");
    pkginfo.set_signature(pkgfile)?;

    debug!("Calculating compressed size for '{pkgpath}'.");
    let csize = fs::metadata(pkgfile)?.len() as usize;

    debug!("Calculating checksums for '{pkgpath}'.");
    let (md5sum, sha256sum) = calc_checksums(pkgfile)?;

    let filename = pkgfile
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow!("invalid package path: {pkgpath}"))?
        .to_string();

    let dir_name = format!("{}-{}", pkginfo.pkgname, pkginfo.pkgver);

    debug!("Creating DESC file for db entry");
    let mut desc = Desc::from(pkginfo);
    desc.filename = filename;
    desc.md5sum = md5sum;
    desc.csize = csize.to_string();
    desc.sha256sum = sha256sum;
    let desc_str = desc.to_string();

    debug!("Adding DESC and FILES entries to db archive");
    add_to_db_file(&desc_str, &dir_name, "desc", db_archive)?;

    files.sort();
    let files_comb = format!("%FILES%\n{}", files.join("\n"));
    add_to_db_file(&desc_str, &dir_name, "desc", files_archive)?;
    add_to_db_file(&files_comb, &dir_name, "files", files_archive)?;

    Ok(())
}

fn calc_checksums(path: &Path) -> anyhow::Result<(String, String)> {
    let mut file = File::open(path)?;
    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer)?;
    let md5sum = format!("{:x}", md5::compute(&buffer));
    let mut hasher = Sha256::new();
    hasher.update(&buffer);
    let sha256sum = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();

    Ok((md5sum, sha256sum))
}
