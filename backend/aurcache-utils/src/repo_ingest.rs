//! Server-side repository ingest.
//!
//! Takes the built package artifacts (as in-memory bytes) for a single build,
//! writes them into the pacman repo tree, runs `repo_add`, and reconciles the
//! `files` table. Decoupled from any builder implementation so it can be called
//! from the worker upload/complete endpoint as well as the legacy Docker path.

use crate::build_logger::BuildLogger;
use crate::utils::remove_archive_file::try_remove_archive_file;
use anyhow::{anyhow, bail};
use aurcache_db::prelude::{Dependencies, Files};
use aurcache_db::{dependencies, files};
use pacman_mirrors::platforms::Platform;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set,
    TransactionTrait,
};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone)]
struct ParsedPkg {
    name: String,
    version: String,
    #[allow(unused)]
    arch: String,
}

/// A single built artifact: its filename and raw bytes.
pub type Artifact = (String, Vec<u8>);

/// Ingest built package artifacts into the repo for `(pkg_id, platform)`.
///
/// Writes each artifact to `./repo/{platform}/{filename}`, adds it to the
/// pacman repo databases, and updates the `files` table (transferring or
/// removing ownership as needed). Returns the version string parsed from the
/// built package filenames (the version *actually produced by makepkg*, which
/// keeps the server authoritative over the build's version).
pub async fn ingest_pkgs(
    db: &DatabaseConnection,
    logger: &BuildLogger,
    pkg_id: i32,
    platform: &Platform,
    artifacts: Vec<Artifact>,
) -> anyhow::Result<String> {
    ingest_pkgs_in(db, logger, pkg_id, platform, artifacts, Path::new("./repo")).await
}

/// Same as [`ingest_pkgs`] but with an explicit repo root (used by tests).
pub async fn ingest_pkgs_in(
    db: &DatabaseConnection,
    logger: &BuildLogger,
    pkg_id: i32,
    platform: &Platform,
    artifacts: Vec<Artifact>,
    repo_root: &Path,
) -> anyhow::Result<String> {
    if artifacts.is_empty() {
        bail!("No files found in build output");
    }

    // Parse + filter the artifacts (skip hidden helper files).
    let mut build_pkgs: Vec<(String, Vec<u8>, ParsedPkg)> = Vec::new();
    for (filename, bytes) in artifacts {
        if filename.starts_with('.') {
            continue;
        }
        let parsed = parse_arch_pkg(&filename)?;
        build_pkgs.push((filename, bytes, parsed));
    }

    if build_pkgs.is_empty() {
        bail!("No packages found in build output");
    }

    // Extract the version from the first built package. All split packages of
    // the same pkgbase share the same pkgver-pkgrel, so any one is representative.
    let actual_version = build_pkgs[0].2.version.clone();

    // PHASE 1: resolve file ownership in a short read transaction so we don't
    // hold a DB connection during the file-write / repo_add phase.
    struct FileInfo {
        archive_name: String,
        pkg_path: String,
        parsed_name: String,
        existing_id: Option<i32>,
        existing_package_id: Option<i32>,
    }

    let mut file_infos: Vec<FileInfo> = Vec::new();
    {
        let txn = db.begin().await?;
        for (filename, _bytes, parsed) in &build_pkgs {
            let archive_name = filename.clone();
            let pkg_path = format!("{}/{platform}/{archive_name}", repo_root.display());

            let existing = Files::find()
                .filter(files::Column::Filename.eq(&archive_name))
                .filter(files::Column::Platform.eq(*platform))
                .one(&txn)
                .await?;

            if let Some(ref ex) = existing
                && ex.package_id != pkg_id
            {
                let existing_owner_depends_on_new_owner = Dependencies::find()
                    .filter(dependencies::Column::DependentId.eq(ex.package_id))
                    .filter(dependencies::Column::DependeeId.eq(pkg_id))
                    .one(&txn)
                    .await?;

                if existing_owner_depends_on_new_owner.is_none() {
                    bail!("File '{archive_name}' is already produced by another package");
                }
                logger
                    .append(format!(
                        "Transferring file '{archive_name}' from package {} (depends on this package)\n",
                        ex.package_id
                    ))
                    .await;
            }

            file_infos.push(FileInfo {
                archive_name,
                pkg_path,
                parsed_name: parsed.name.clone(),
                existing_id: existing.as_ref().map(|e| e.id),
                existing_package_id: existing.as_ref().map(|e| e.package_id),
            });
        }
        txn.commit().await?;
    }

    // Ensure the repo directory exists.
    fs::create_dir_all(format!("{}/{platform}", repo_root.display()))?;

    // PHASE 2: write files and update the pacman repo — no DB connection held.
    for (fi, (_filename, bytes, _parsed)) in file_infos.iter().zip(build_pkgs.iter()) {
        logger
            .append(format!("Write {} to repo directory\n", fi.archive_name))
            .await;
        fs::write(&fi.pkg_path, bytes)?;

        logger
            .append(format!(
                "Add {} to repo.db.tar.gz and repo.files.tar.gz\n",
                fi.archive_name
            ))
            .await;
        pacman_repo_utils::repo_add::repo_add(
            &fi.pkg_path,
            format!("{}/{platform}/repo.db.tar.gz", repo_root.display()),
            format!("{}/{platform}/repo.files.tar.gz", repo_root.display()),
        )?;
    }

    // PHASE 3: write file records and remove stale entries in one short txn.
    let mut new_file_ids: HashMap<String, i32> = HashMap::new();
    {
        let txn = db.begin().await?;

        for fi in &file_infos {
            let file_id = if let Some(existing_id) = fi.existing_id {
                if fi.existing_package_id != Some(pkg_id) {
                    let active = files::ActiveModel {
                        id: Set(existing_id),
                        package_id: Set(pkg_id),
                        ..Default::default()
                    };
                    active.update(&txn).await?.id
                } else {
                    existing_id
                }
            } else {
                files::ActiveModel {
                    filename: Set(fi.archive_name.clone()),
                    platform: Set(*platform),
                    package_id: Set(pkg_id),
                    ..Default::default()
                }
                .insert(&txn)
                .await?
                .id
            };
            new_file_ids.insert(fi.parsed_name.clone(), file_id);
        }

        let stale = Files::find()
            .filter(files::Column::PackageId.eq(pkg_id))
            .filter(files::Column::Platform.eq(*platform))
            .all(&txn)
            .await?;

        for file in stale {
            if !new_file_ids.values().any(|&id| id == file.id) {
                logger
                    .append(format!("Removing dropped sub-package: {}\n", file.filename))
                    .await;
                try_remove_archive_file(file, &txn).await?;
            }
        }

        txn.commit().await?;
    }

    logger
        .append("Successfully updated repo and cleaned up old files\n".to_string())
        .await;
    Ok(actual_version)
}

/// Parse an Arch package filename into its name / version / arch components.
///
/// e.g. `hello-2.12.1-1-x86_64.pkg.tar.zst` -> name `hello`, version `2.12.1-1`.
fn parse_arch_pkg(filename: &str) -> anyhow::Result<ParsedPkg> {
    let base = filename
        .split(".pkg.")
        .next()
        .ok_or_else(|| anyhow!("Invalid pkg filename: {filename}"))?;

    let parts: Vec<&str> = base.split('-').collect();
    if parts.len() < 4 {
        bail!("Invalid pkg filename format: {filename}");
    }

    let arch = parts[parts.len() - 1].to_string();
    let pkgrel = parts[parts.len() - 2];
    let pkgver = parts[parts.len() - 3];
    let name = parts[..parts.len() - 3].join("-");

    Ok(ParsedPkg {
        name,
        version: format!("{pkgver}-{pkgrel}"),
        arch,
    })
}

/// Sanity-check uploaded artifact filenames against the set of package names the
/// server expects for this package (its pkgbase plus any split packages).
///
/// This blocks *wrong-named* artifacts (e.g. a worker uploading `openssh-*`
/// under a job for `hello`); it does **not** and cannot verify the *contents* of
/// a correctly-named package (see design non-goals). Signature sidecars
/// (`*.sig`) and hidden helper files are ignored; every remaining file must be a
/// `*.pkg.tar.*` whose parsed pkgname is in `expected`.
pub fn validate_artifact_names(expected: &[String], filenames: &[String]) -> anyhow::Result<()> {
    if expected.is_empty() {
        bail!("no expected package names to validate against");
    }
    for filename in filenames {
        if filename.starts_with('.') || filename.ends_with(".sig") {
            continue;
        }
        if !filename.contains(".pkg.tar") {
            bail!("unexpected non-package artifact: {filename}");
        }
        let parsed = parse_arch_pkg(filename)?;
        if !expected.iter().any(|e| e == &parsed.name) {
            bail!(
                "artifact '{filename}' has pkgname '{}' not among expected {expected:?}",
                parsed.name
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_pkg_filename() {
        let p = parse_arch_pkg("hello-2.12.1-1-x86_64.pkg.tar.zst").unwrap();
        assert_eq!(p.name, "hello");
        assert_eq!(p.version, "2.12.1-1");
        assert_eq!(p.arch, "x86_64");
    }

    #[test]
    fn parses_hyphenated_pkg_name() {
        let p = parse_arch_pkg("my-cool-pkg-1.0.0-2-aarch64.pkg.tar.zst").unwrap();
        assert_eq!(p.name, "my-cool-pkg");
        assert_eq!(p.version, "1.0.0-2");
        assert_eq!(p.arch, "aarch64");
    }

    #[test]
    fn rejects_malformed_filename() {
        assert!(parse_arch_pkg("garbage.pkg.tar.zst").is_err());
    }

    #[test]
    fn validate_accepts_expected_and_signatures() {
        let expected = vec!["hello".to_string(), "hello-docs".to_string()];
        let files = vec![
            "hello-1.0-1-x86_64.pkg.tar.zst".to_string(),
            "hello-1.0-1-x86_64.pkg.tar.zst.sig".to_string(),
            "hello-docs-1.0-1-x86_64.pkg.tar.zst".to_string(),
        ];
        assert!(validate_artifact_names(&expected, &files).is_ok());
    }

    #[test]
    fn validate_rejects_unexpected_pkgname() {
        let expected = vec!["hello".to_string()];
        let files = vec!["openssh-9.0-1-x86_64.pkg.tar.zst".to_string()];
        assert!(validate_artifact_names(&expected, &files).is_err());
    }

    #[test]
    fn validate_rejects_non_package_file() {
        let expected = vec!["hello".to_string()];
        let files = vec!["evil.sh".to_string()];
        assert!(validate_artifact_names(&expected, &files).is_err());
    }
}
