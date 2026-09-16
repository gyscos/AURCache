//! Putting a finished build in the repository.
//!
//! A worker's part ends when the server accepts its completion: the build moves
//! to `PUBLISHING` and the worker moves on (see
//! [`accept_for_publishing`](crate::worker_complete::accept_for_publishing)).
//! Everything after that is the server's -- reading the packages, updating
//! the databases, recording the result -- and nothing the worker could do again
//! would help with any of it, so a failure here fails the build rather than
//! asking the worker for anything.

use crate::build_logger::append_build_output;
use crate::repository::{PublishedFile, Repository, Update};
use anyhow::{anyhow, bail};
use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_activitylog::failure_activity::PublishFailedActivity;
use aurcache_common::builder::BuildStates;
use aurcache_db::activities::ActivityType;
use aurcache_db::helpers::time::now_secs;
use aurcache_db::prelude::{Builds, Dependencies, Files, Packages};
use aurcache_db::{builds, dependencies, files, packages};
use pacman_mirrors::platforms::Platform;
use pacman_repo_utils::PackageEntry;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, IntoActiveModel,
    PaginatorTrait, QueryFilter, Set, TransactionTrait,
};
use std::path::PathBuf;
use tracing::{error, info, warn};

/// Publish build `build_id`, which must be `PUBLISHING`, from its staging
/// directory; or fail it.
///
/// Never returns an error: whatever goes wrong ends as a failed build with the
/// reason in its log, and the staging directory is removed either way. A build
/// that is not `PUBLISHING` (published already, or its package deleted) is left
/// alone.
pub async fn publish_build(
    db: &DatabaseConnection,
    repo: &Repository,
    activity: &ActivityLog,
    build_id: i32,
) {
    let Ok(Some(build)) = Builds::find_by_id(build_id).one(db).await else {
        let _ = tokio::fs::remove_dir_all(repo.staging_dir(build_id)).await;
        return;
    };
    if build.status != Some(BuildStates::PUBLISHING) {
        return;
    }
    let pkgbase = Packages::find_by_id(build.pkg_id)
        .one(db)
        .await
        .ok()
        .flatten()
        .map(|pkg| pkg.name);

    match publish(db, repo, &build).await {
        Ok(published) => {
            info!(
                "published build #{build_id}: {} package(s)",
                published.packages
            );
            log(
                pkgbase.as_deref(),
                build.number,
                "Published to the repository\n",
            )
            .await;
            if let Err(e) =
                crate::worker_complete::trigger_dependents(db, build.pkg_id, build.platform).await
            {
                error!(
                    "Failed to trigger dependents of package {}: {e}",
                    build.pkg_id
                );
            }
        }
        Err(e) => {
            warn!("publishing build #{build_id} failed: {e:#}");
            // The build worked and the package exists; it is the last step that
            // did not. That is worth an entry someone will come across, rather
            // than only a line in this process's journal.
            activity.record(
                PublishFailedActivity {
                    package: pkgbase.clone().unwrap_or_else(|| "?".to_string()),
                    build: build.number,
                    reason: format!("{e:#}"),
                },
                ActivityType::PublishFailed,
                None,
            );
            log(
                pkgbase.as_deref(),
                build.number,
                &format!("Publishing failed: {e:#}\n"),
            )
            .await;
            if let Err(e) = fail(db, &build).await {
                error!("could not mark build #{build_id} failed: {e}");
            }
        }
    }
    let _ = tokio::fs::remove_dir_all(repo.staging_dir(build_id)).await;
}

async fn log(pkgbase: Option<&str>, number: i32, text: &str) {
    if let Some(pkgbase) = pkgbase
        && let Err(e) = append_build_output(pkgbase, number, text).await
    {
        warn!("could not write to the log of {pkgbase}/{number}: {e}");
    }
}

struct Published {
    packages: usize,
}

/// A staged package, read and named.
struct Staged {
    path: PathBuf,
    filename: String,
    entry: PackageEntry,
    size: i64,
}

/// What the database is told, decided under the repository lock.
struct Plan {
    /// Each published file, with the `files` row it already has, if any.
    rows: Vec<(String, Option<i32>, i64)>,
    /// This package's files on this platform that the build no longer
    /// produces.
    stale: Vec<PublishedFile>,
    version: String,
    total_size: i64,
}

async fn publish(
    db: &DatabaseConnection,
    repo: &Repository,
    build: &builds::Model,
) -> anyhow::Result<Published> {
    let pkg = Packages::find_by_id(build.pkg_id)
        .one(db)
        .await?
        .ok_or_else(|| anyhow!("the package is gone"))?;
    let platform = build.platform;
    let staged = read_staging(repo, build, &pkg).await?;
    let packages = staged.len();

    // The slow part, and private: every archive read in full, before anything
    // is locked or changed.
    let mut described = Vec::with_capacity(staged.len());
    for (path, filename) in staged {
        let size = i64::try_from(tokio::fs::metadata(&path).await?.len()).unwrap_or(i64::MAX);
        let entry = repo
            .describe(path.clone())
            .await
            .map_err(|e| e.context(format!("reading {filename}")))?;
        described.push(Staged {
            path,
            filename,
            entry,
            size,
        });
    }
    let version = parse_arch_pkg(&described[0].filename)?.version;

    let mut update = repo.begin().await;
    let plan = plan(&update, db, &pkg, platform, &described, version).await?;
    for file in &plan.stale {
        update.retire(file)?;
    }
    for staged in described {
        update.add(platform, staged.path, staged.entry);
    }
    update
        .commit(|| record(db, build.id, &pkg, platform, &plan))
        .await?;

    Ok(Published { packages })
}

/// The staged files that are to be published, as `(path, filename)`.
///
/// Detached signatures are left out -- the repository moves them with their
/// package -- and so are makepkg's `-debug` packages (see
/// [`is_debug_artifact`]). What remains must be named for this package.
async fn read_staging(
    repo: &Repository,
    build: &builds::Model,
    pkg: &packages::Model,
) -> anyhow::Result<Vec<(PathBuf, String)>> {
    let expected = expected_pkgnames(pkg);
    let mut names = Vec::new();
    let mut entries = match tokio::fs::read_dir(repo.staging_dir(build.id)).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("no artifacts were uploaded")
        }
        Err(e) => return Err(e.into()),
    };
    while let Some(entry) = entries.next_entry().await? {
        if !entry.file_type().await?.is_file() {
            continue;
        }
        let filename = entry.file_name().to_string_lossy().into_owned();
        if filename.starts_with('.') || filename.ends_with(".sig") {
            continue;
        }
        if is_debug_artifact(&expected, &filename) {
            log(
                Some(&pkg.name),
                build.number,
                &format!("skipping debug package (not published): {filename}\n"),
            )
            .await;
            continue;
        }
        names.push((entry.path(), filename));
    }
    if names.is_empty() {
        bail!("no publishable artifacts were uploaded");
    }
    let filenames: Vec<String> = names.iter().map(|(_, f)| f.clone()).collect();
    validate_artifact_names(&expected, &filenames)?;
    names.sort_by(|a, b| a.1.cmp(&b.1));
    Ok(names)
}

/// Decide who owns each file and what goes stale, from what `update` reads.
///
/// A filename another package already publishes is refused, unless that
/// package depends on this one (the file moved between them) or no longer
/// exists (the row is a leftover, not a claim).
async fn plan(
    update: &Update<'_>,
    db: &DatabaseConnection,
    pkg: &packages::Model,
    platform: Platform,
    described: &[Staged],
    version: String,
) -> anyhow::Result<Plan> {
    let mut rows = Vec::with_capacity(described.len());
    for staged in described {
        let existing = update
            .published_file(db, platform, &staged.filename)
            .await?;
        if let Some(existing) = &existing
            && existing.package_id() != pkg.id
        {
            // Counted rather than read: all this needs is whether the row is
            // there, and loading the model would also have to deserialize
            // columns like `source_data`.
            let owner_exists = Packages::find_by_id(existing.package_id())
                .count(db)
                .await?
                > 0;
            let owner_depends_on_us = Dependencies::find()
                .filter(dependencies::Column::DependentId.eq(existing.package_id()))
                .filter(dependencies::Column::DependeeId.eq(pkg.id))
                .count(db)
                .await?
                > 0;
            if owner_exists && !owner_depends_on_us {
                bail!(
                    "File '{}' is already produced by another package",
                    staged.filename
                );
            }
        }
        rows.push((
            staged.filename.clone(),
            existing.map(|file| file.id()),
            staged.size,
        ));
    }

    let published: Vec<&str> = described.iter().map(|s| s.filename.as_str()).collect();
    let stale = update
        .published_files_of(db, &[pkg.id])
        .await?
        .into_iter()
        .filter(|file| file.platform() == platform && !published.contains(&file.filename()))
        .collect();

    Ok(Plan {
        rows,
        stale,
        version,
        total_size: described.iter().map(|s| s.size).sum(),
    })
}

/// The one database transaction: the files, the build's result and the
/// package's status, together.
///
/// Conditioned on the build still being `PUBLISHING`, so a build cancelled or
/// deleted meanwhile is never recorded as published.
async fn record(
    db: &DatabaseConnection,
    build_id: i32,
    pkg: &packages::Model,
    platform: Platform,
    plan: &Plan,
) -> anyhow::Result<()> {
    let txn = db.begin().await?;
    let marked = Builds::update_many()
        .col_expr(builds::Column::Status, BuildStates::SUCCESSFUL_BUILD.into())
        .col_expr(builds::Column::Version, plan.version.clone().into())
        .col_expr(builds::Column::Size, Some(plan.total_size).into())
        .col_expr(builds::Column::EndTime, Some(now_secs()).into())
        .filter(builds::Column::Id.eq(build_id))
        .filter(builds::Column::Status.eq(BuildStates::PUBLISHING))
        .exec(&txn)
        .await?;
    if marked.rows_affected == 0 {
        bail!("build #{build_id} is no longer being published");
    }

    for (filename, existing, size) in &plan.rows {
        match existing {
            // Updated even when otherwise unchanged: a rebuild at the same
            // version has the same filename and different contents.
            Some(id) => {
                files::ActiveModel {
                    id: Set(*id),
                    package_id: Set(pkg.id),
                    size: Set(Some(*size)),
                    ..Default::default()
                }
                .update(&txn)
                .await?;
            }
            None => {
                files::ActiveModel {
                    filename: Set(filename.clone()),
                    platform: Set(platform),
                    package_id: Set(pkg.id),
                    size: Set(Some(*size)),
                    ..Default::default()
                }
                .insert(&txn)
                .await?;
            }
        }
    }
    if !plan.stale.is_empty() {
        Files::delete_many()
            .filter(files::Column::Id.is_in(plan.stale.iter().map(PublishedFile::id)))
            .exec(&txn)
            .await?;
    }

    if let Some(row) = Packages::find_by_id(pkg.id).one(&txn).await? {
        let mut row = row.into_active_model();
        row.status = Set(BuildStates::SUCCESSFUL_BUILD);
        row.out_of_date = Set(0);
        row.upstream_version = Set(Some(plan.version.clone()));
        row.update(&txn).await?;
    }
    txn.commit().await?;
    Ok(())
}

/// Mark a build that could not be published as failed, with its package.
async fn fail(db: &DatabaseConnection, build: &builds::Model) -> anyhow::Result<()> {
    let txn = db.begin().await?;
    let marked = Builds::update_many()
        .col_expr(builds::Column::Status, BuildStates::FAILED_BUILD.into())
        .col_expr(builds::Column::EndTime, Some(now_secs()).into())
        .filter(builds::Column::Id.eq(build.id))
        .filter(builds::Column::Status.eq(BuildStates::PUBLISHING))
        .exec(&txn)
        .await?;
    if marked.rows_affected > 0
        && let Some(row) = Packages::find_by_id(build.pkg_id).one(&txn).await?
    {
        let mut row = row.into_active_model();
        row.status = Set(BuildStates::FAILED_BUILD);
        row.update(&txn).await?;
    }
    txn.commit().await?;
    Ok(())
}

/// The builds a restart interrupted mid-publish.
///
/// Nothing public happened to them yet -- the database transaction is what
/// moves a build out of `PUBLISHING` -- so publishing them again from their
/// staging directories is safe.
pub async fn interrupted(db: &DatabaseConnection) -> anyhow::Result<Vec<i32>> {
    Ok(Builds::find()
        .filter(builds::Column::Status.eq(BuildStates::PUBLISHING))
        .all(db)
        .await?
        .into_iter()
        .map(|build| build.id)
        .collect())
}

/// The package names a build of `pkg` may produce: its pkgbase plus any
/// split-package names recorded on the package row.
fn expected_pkgnames(pkg: &packages::Model) -> Vec<String> {
    let mut names = vec![pkg.name.clone()];
    if let Some(json) = pkg.split_packages.as_deref()
        && let Ok(split) = serde_json::from_str::<Vec<String>>(json)
    {
        for name in split {
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }
    names
}

#[derive(Debug, Clone)]
struct ParsedPkg {
    name: String,
    version: String,
    #[allow(unused)]
    arch: String,
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
///
/// Debug packages are dropped before this runs — see [`is_debug_artifact`].
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

/// True for makepkg's split debug package of one of the `expected` pkgnames
/// (`<pkgname>-debug`), which the server discards rather than publishing.
///
/// AURCache serves a single flat repo per architecture, so a debug package would
/// appear in `pacman -Ss` right next to the real ones. Arch keeps them out of
/// `core`/`extra`/`multilib` for exactly that reason — they live in separate,
/// opt-in `*-debug` repos.
///
/// The server already forces `OPTIONS=(!debug)` so these are not built at all.
/// This is the backstop for a worker whose user-supplied `makepkg.conf`
/// re-enables `debug`: without it, the extra artifact fails
/// [`validate_artifact_names`] and the build fails.
#[must_use]
pub fn is_debug_artifact(expected: &[String], filename: &str) -> bool {
    let Ok(parsed) = parse_arch_pkg(filename) else {
        return false;
    };
    parsed
        .name
        .strip_suffix("-debug")
        .is_some_and(|base| expected.iter().any(|e| e == base))
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

    #[test]
    fn debug_artifact_detected_for_expected_package() {
        let expected = vec!["hello".to_string(), "hello-docs".to_string()];
        assert!(is_debug_artifact(
            &expected,
            "hello-debug-2.12.1-2-x86_64.pkg.tar.zst"
        ));
        assert!(is_debug_artifact(
            &expected,
            "hello-docs-debug-2.12.1-2-x86_64.pkg.tar.zst"
        ));
    }

    /// Only the debug split of an *expected* package is dropped; a foreign
    /// `-debug` upload must still reach `validate_artifact_names` and be rejected.
    #[test]
    fn debug_artifact_of_foreign_package_is_not_dropped() {
        let expected = vec!["hello".to_string()];
        let foreign = "openssh-debug-9.0-1-x86_64.pkg.tar.zst";
        assert!(!is_debug_artifact(&expected, foreign));
        assert!(validate_artifact_names(&expected, &[foreign.to_string()]).is_err());
    }

    #[test]
    fn regular_package_is_not_a_debug_artifact() {
        let expected = vec!["hello".to_string()];
        assert!(!is_debug_artifact(
            &expected,
            "hello-2.12.1-2-x86_64.pkg.tar.zst"
        ));
    }

    /// The build server forces this; a worker on Arch defaults would otherwise
    /// emit `<pkgname>-debug` packages into the single flat repo.
    #[tokio::test]
    async fn makepkg_config_disables_debug_packages() {
        let conf =
            crate::job_config::create_makepkg_config(None, std::path::Path::new("/out")).await;
        assert!(conf.contains("OPTIONS=(!debug)"), "got:\n{conf}");
    }
}
