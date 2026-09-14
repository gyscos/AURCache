//! Publishing a build a worker has handed over: what reaches the repository,
//! what the database records, and what a failure leaves untouched.

use aurcache_common::builder::BuildStates;
use aurcache_db::migration::Migrator;
use aurcache_db::prelude::{Builds, Files, Packages};
use aurcache_utils::publish::{interrupted, publish_build};
use aurcache_utils::repository::Repository;
use sea_orm::{ConnectionTrait, Database, DatabaseConnection, EntityTrait};
use sea_orm_migration::MigratorTrait;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Build logs go somewhere disposable, the same place for every test in this
/// process.
fn quiet_logs() {
    static LOGS: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    LOGS.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap().keep();
        // SAFETY: set once, before any test reads it, and never changed.
        unsafe { std::env::set_var("AURCACHE_BUILD_LOG_PATH", &dir) };
        dir
    });
}

/// A minimal valid `.pkg.tar.zst`: a zstd-compressed tar holding a `.PKGINFO`.
fn fake_pkg_zst(pkgname: &str, pkgver: &str) -> Vec<u8> {
    let pkginfo = format!("pkgname = {pkgname}\npkgver = {pkgver}\n");
    let mut tar_bytes = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut tar_bytes);
        let mut header = tar::Header::new_gnu();
        header.set_path(".PKGINFO").unwrap();
        header.set_size(pkginfo.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append(&header, pkginfo.as_bytes()).unwrap();
        builder.finish().unwrap();
    }
    zstd::stream::encode_all(tar_bytes.as_slice(), 0).unwrap()
}

fn filename(pkgname: &str, pkgver: &str) -> String {
    format!("{pkgname}-{pkgver}-x86_64.pkg.tar.zst")
}

async fn db() -> DatabaseConnection {
    quiet_logs();
    let db = Database::connect("sqlite::memory:").await.unwrap();
    Migrator::up(&db, None).await.unwrap();
    db
}

async fn package(db: &DatabaseConnection, id: i32, name: &str) {
    db.execute_unprepared(&format!(
        "INSERT INTO packages \
         (id, name, status, out_of_date, build_flags, platforms, source_type, source_data, directly_requested) \
         VALUES ({id}, '{name}', {}, 1, '', 'x86_64', 'aur', '{{\"type\":\"aur\",\"name\":\"{name}\"}}', 1)",
        BuildStates::PUBLISHING
    ))
    .await
    .unwrap();
}

/// Build `id` of package `pkg_id`, handed over by worker 7 and being published.
async fn publishing_build(db: &DatabaseConnection, id: i32, pkg_id: i32) {
    db.execute_unprepared(&format!(
        "INSERT INTO builds (id, pkg_id, status, start_time, platform, version, number, worker_id) \
         VALUES ({id}, {pkg_id}, {}, 1, 'x86_64', '', {id}, 7)",
        BuildStates::PUBLISHING
    ))
    .await
    .unwrap();
}

fn stage(repo: &Repository, build_id: i32, pkgname: &str, pkgver: &str) -> PathBuf {
    let dir = repo.staging_dir(build_id);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(filename(pkgname, pkgver));
    std::fs::write(&path, fake_pkg_zst(pkgname, pkgver)).unwrap();
    path
}

/// The entry directories `repo.db` lists for x86_64, or none if it has none.
fn listed(root: &Path) -> Vec<String> {
    let Ok(mut file) = std::fs::File::open(root.join("x86_64").join("repo.db.tar.gz")) else {
        return Vec::new();
    };
    let mut data = Vec::new();
    file.read_to_end(&mut data).unwrap();
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(data.as_slice()));
    let mut dirs: Vec<String> = archive
        .entries()
        .unwrap()
        .flatten()
        .filter(|e| e.header().entry_type().is_dir())
        .map(|e| e.path().unwrap().display().to_string())
        .collect();
    dirs.sort();
    dirs
}

async fn status(db: &DatabaseConnection, build_id: i32) -> Option<i32> {
    Builds::find_by_id(build_id)
        .one(db)
        .await
        .unwrap()
        .unwrap()
        .status
}

#[tokio::test]
async fn a_published_build_is_in_the_repository_and_recorded() {
    let db = db().await;
    let tmp = tempfile::tempdir().unwrap();
    let repo = Repository::new(tmp.path());
    package(&db, 1, "hello").await;
    publishing_build(&db, 1, 1).await;
    let staged = stage(&repo, 1, "hello", "2.12.1-1");
    let size = std::fs::metadata(&staged).unwrap().len();

    publish_build(&db, &repo, 1).await;

    assert_eq!(status(&db, 1).await, Some(BuildStates::SUCCESSFUL_BUILD));
    let build = Builds::find_by_id(1).one(&db).await.unwrap().unwrap();
    // The version comes from the package file, the server's authority on what
    // was actually built.
    assert_eq!(build.version, "2.12.1-1");
    assert_eq!(build.size, Some(i64::try_from(size).unwrap()));
    assert!(build.end_time.is_some());
    assert_eq!(build.worker_id, Some(7), "still names who built it");

    let pkg = Packages::find_by_id(1).one(&db).await.unwrap().unwrap();
    assert_eq!(pkg.status, BuildStates::SUCCESSFUL_BUILD);
    assert_eq!(pkg.out_of_date, 0);
    assert_eq!(pkg.upstream_version.as_deref(), Some("2.12.1-1"));

    let rows = Files::find().all(&db).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].filename, filename("hello", "2.12.1-1"));
    assert_eq!(rows[0].size, Some(i64::try_from(size).unwrap()));

    assert_eq!(listed(tmp.path()), ["hello-2.12.1-1"]);
    assert!(
        tmp.path()
            .join("x86_64")
            .join(filename("hello", "2.12.1-1"))
            .exists()
    );
    assert!(!repo.staging_dir(1).exists(), "staging is cleared");
}

/// A new version replaces the old one everywhere: file, entry and row.
#[tokio::test]
async fn a_new_version_replaces_the_old_one() {
    let db = db().await;
    let tmp = tempfile::tempdir().unwrap();
    let repo = Repository::new(tmp.path());
    package(&db, 1, "hello").await;
    publishing_build(&db, 1, 1).await;
    stage(&repo, 1, "hello", "1.0-1");
    publish_build(&db, &repo, 1).await;

    publishing_build(&db, 2, 1).await;
    stage(&repo, 2, "hello", "1.1-1");
    publish_build(&db, &repo, 2).await;

    assert_eq!(status(&db, 2).await, Some(BuildStates::SUCCESSFUL_BUILD));
    assert_eq!(listed(tmp.path()), ["hello-1.1-1"]);
    assert!(
        !tmp.path()
            .join("x86_64")
            .join(filename("hello", "1.0-1"))
            .exists()
    );
    let rows = Files::find().all(&db).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].filename, filename("hello", "1.1-1"));
}

/// A file another live package publishes is refused -- and a refusal fails the
/// build while leaving the repository exactly as it was.
#[tokio::test]
async fn a_file_owned_by_a_live_package_fails_the_build_and_changes_nothing() {
    let db = db().await;
    let tmp = tempfile::tempdir().unwrap();
    let repo = Repository::new(tmp.path());
    package(&db, 1, "other").await;
    package(&db, 2, "hello").await;
    db.execute_unprepared(&format!(
        "INSERT INTO files (filename, platform, package_id) VALUES ('{}', 'x86_64', 1)",
        filename("hello", "2.12.1-1")
    ))
    .await
    .unwrap();
    publishing_build(&db, 1, 2).await;
    stage(&repo, 1, "hello", "2.12.1-1");

    publish_build(&db, &repo, 1).await;

    assert_eq!(status(&db, 1).await, Some(BuildStates::FAILED_BUILD));
    let pkg = Packages::find_by_id(2).one(&db).await.unwrap().unwrap();
    assert_eq!(pkg.status, BuildStates::FAILED_BUILD);
    let rows = Files::find().all(&db).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].package_id, 1, "the other package keeps its file");
    assert!(listed(tmp.path()).is_empty(), "nothing reached repo.db");
    assert!(
        !tmp.path()
            .join("x86_64")
            .join(filename("hello", "2.12.1-1"))
            .exists()
    );
    assert!(
        !repo.staging_dir(1).exists(),
        "staging is cleared on failure too"
    );
}

/// A `files` row left behind by a package that no longer exists must not block
/// the artifact being published again: it is a leftover, not a claim.
///
/// `files.package_id` is a foreign key, so the constraint has to be off to
/// write such a row at all -- which is exactly the database this covers.
#[tokio::test]
async fn a_file_whose_owner_is_gone_is_claimed() {
    let db = db().await;
    db.execute_unprepared("PRAGMA foreign_keys = OFF;")
        .await
        .unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let repo = Repository::new(tmp.path());
    package(&db, 2, "hello").await;
    db.execute_unprepared(&format!(
        "INSERT INTO files (filename, platform, package_id) VALUES ('{}', 'x86_64', 1)",
        filename("hello", "2.12.1-1")
    ))
    .await
    .unwrap();
    publishing_build(&db, 1, 2).await;
    stage(&repo, 1, "hello", "2.12.1-1");

    publish_build(&db, &repo, 1).await;

    assert_eq!(status(&db, 1).await, Some(BuildStates::SUCCESSFUL_BUILD));
    let rows = Files::find().all(&db).await.unwrap();
    assert_eq!(rows.len(), 1, "claimed, not duplicated");
    assert_eq!(rows[0].package_id, 2);
}

/// Artifacts named for another package are never published.
#[tokio::test]
async fn a_wrong_named_artifact_fails_the_build() {
    let db = db().await;
    let tmp = tempfile::tempdir().unwrap();
    let repo = Repository::new(tmp.path());
    package(&db, 1, "hello").await;
    publishing_build(&db, 1, 1).await;
    stage(&repo, 1, "openssh", "9.9-1");

    publish_build(&db, &repo, 1).await;

    assert_eq!(status(&db, 1).await, Some(BuildStates::FAILED_BUILD));
    assert!(Files::find().all(&db).await.unwrap().is_empty());
    assert!(listed(tmp.path()).is_empty());
}

/// A restart mid-publish leaves the build `PUBLISHING` with its upload staged;
/// publishing it again from there completes it.
#[tokio::test]
async fn an_interrupted_publication_is_resumed_from_staging() {
    let db = db().await;
    let tmp = tempfile::tempdir().unwrap();
    let repo = Repository::new(tmp.path());
    package(&db, 1, "hello").await;
    publishing_build(&db, 1, 1).await;
    stage(&repo, 1, "hello", "1.0-1");

    assert_eq!(interrupted(&db).await.unwrap(), [1]);
    for build_id in interrupted(&db).await.unwrap() {
        publish_build(&db, &repo, build_id).await;
    }

    assert_eq!(status(&db, 1).await, Some(BuildStates::SUCCESSFUL_BUILD));
    assert!(interrupted(&db).await.unwrap().is_empty());
    assert_eq!(listed(tmp.path()), ["hello-1.0-1"]);
}

/// A build no longer being published -- already done, or cancelled meanwhile --
/// is left alone.
#[tokio::test]
async fn a_build_not_publishing_is_left_alone() {
    let db = db().await;
    let tmp = tempfile::tempdir().unwrap();
    let repo = Repository::new(tmp.path());
    package(&db, 1, "hello").await;
    publishing_build(&db, 1, 1).await;
    db.execute_unprepared(&format!(
        "UPDATE builds SET status = {} WHERE id = 1",
        BuildStates::FAILED_BUILD
    ))
    .await
    .unwrap();
    let staged = stage(&repo, 1, "hello", "1.0-1");

    publish_build(&db, &repo, 1).await;

    assert_eq!(status(&db, 1).await, Some(BuildStates::FAILED_BUILD));
    assert!(staged.exists());
    assert!(listed(tmp.path()).is_empty());
}
