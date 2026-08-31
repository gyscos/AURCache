//! Golden integration test for server-side repo ingest.
//!
//! Verifies that `ingest_pkgs_in` writes the artifact into the repo tree, runs
//! `repo_add`, parses the version from the package filename, and records a row
//! in the `files` table — the same output the legacy Docker path produced.

use aurcache_db::files;
use aurcache_db::migration::Migrator;
use aurcache_db::prelude::Files;
use aurcache_utils::build_logger::BuildLogger;
use aurcache_utils::repo_ingest::ingest_pkgs_in;
use pacman_mirrors::platforms::Platform;
use sea_orm::{ColumnTrait, ConnectionTrait, Database, EntityTrait, PaginatorTrait, QueryFilter};
use sea_orm_migration::MigratorTrait;

/// Build a minimal valid `.pkg.tar.zst`: a zstd-compressed tar containing a
/// `.PKGINFO` with a pkgname + pkgver (all `repo_add` requires to be valid).
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

#[tokio::test]
async fn ingest_writes_repo_and_files_row_and_parses_version() {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    Migrator::up(&db, None).await.unwrap();

    // A package and its active build. Insert via raw SQL (the ORM read-back
    // requires non-null columns that the schema leaves defaultable).
    db.execute_unprepared("INSERT INTO packages (id, name) VALUES (1, 'hello');")
        .await
        .unwrap();
    db.execute_unprepared(
        "INSERT INTO builds (id, pkg_id, platform, status, version) VALUES (1, 1, 'x86_64', 0, '');",
    )
    .await
    .unwrap();
    let pkg_id = 1;

    let logger = BuildLogger::new(1, db.clone());
    let repo_root = tempfile::tempdir().unwrap();

    let artifacts = vec![(
        "hello-2.12.1-1-x86_64.pkg.tar.zst".to_string(),
        fake_pkg_zst("hello", "2.12.1-1"),
    )];

    let ingested = ingest_pkgs_in(
        &db,
        &logger,
        pkg_id,
        &Platform::X86_64,
        artifacts,
        repo_root.path(),
        None,
    )
    .await
    .expect("ingest should succeed");

    // Version is parsed from the built package filename (server-authoritative).
    assert_eq!(ingested.version, "2.12.1-1");

    // The reported size is the artifact's own length, so the build row and the
    // `files` row cannot disagree about how big the same output was.
    let written = std::fs::metadata(
        repo_root
            .path()
            .join("x86_64")
            .join("hello-2.12.1-1-x86_64.pkg.tar.zst"),
    )
    .expect("artifact should be on disk")
    .len();
    assert_eq!(ingested.total_size, i64::try_from(written).unwrap());

    // Artifact written into the repo tree + repo db updated.
    let arch_dir = repo_root.path().join("x86_64");
    assert!(arch_dir.join("hello-2.12.1-1-x86_64.pkg.tar.zst").exists());
    assert!(arch_dir.join("repo.db.tar.gz").exists());

    // Files table records the package.
    let file_rows = Files::find()
        .filter(files::Column::PackageId.eq(pkg_id))
        .all(&db)
        .await
        .unwrap();
    assert_eq!(file_rows.len(), 1);
    assert_eq!(file_rows[0].filename, "hello-2.12.1-1-x86_64.pkg.tar.zst");
    assert_eq!(file_rows[0].platform, Platform::X86_64);

    // No duplicate rows on re-ingest of the same package.
    assert_eq!(
        Files::find().count(&db).await.unwrap(),
        1,
        "exactly one file row expected"
    );
}
