//! What a dump carries, and what it deliberately leaves behind.

use aurcache_common::api::dump::{DUMP_SCHEMA_VERSION, MANIFEST_FILE, PACKAGES_FILE};
use aurcache_common::source::GitSourceSpec;
use aurcache_db::migration::Migrator;
use aurcache_db::packages::{SourceData, SourceType};
use aurcache_db::prelude::Packages;
use aurcache_db::{packages, settings, workers};
use aurcache_utils::dump::{build_dump, write_archive};
use flate2::read::GzDecoder;
use sea_orm::{ActiveModelTrait, Database, DatabaseConnection, EntityTrait, Set};
use sea_orm_migration::MigratorTrait;
use std::collections::HashMap;
use std::io::Read;

async fn db() -> DatabaseConnection {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    Migrator::up(&db, None).await.unwrap();
    db
}

#[allow(clippy::too_many_arguments)]
async fn package(
    db: &DatabaseConnection,
    name: &str,
    directly_requested: bool,
    patch: Option<&str>,
) -> i32 {
    packages::ActiveModel {
        name: Set(name.to_string()),
        // Derived state, deliberately set to something a restore must not
        // carry over: the dump should describe the package, not its history.
        status: Set(1),
        out_of_date: Set(1),
        upstream_version: Set(Some("9.9.9".to_string())),
        latest_build: Set(None),
        build_flags: Set("--noconfirm;;--nocolor".to_string()),
        platforms: Set("x86_64;aarch64".to_string()),
        source_type: Set(SourceType::Aur),
        source_data: Set(SourceData::Aur {
            name: name.to_string(),
        }),
        directly_requested: Set(directly_requested),
        split_packages: Set(Some(r#"["a","b"]"#.to_string())),
        patch: Set(patch.map(str::to_string)),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap()
    .id
}

/// The headline: authored state in, derived state out.
#[tokio::test]
async fn a_dump_carries_configuration_and_not_history() {
    let db = db().await;
    package(&db, "hello", true, None).await;

    let dump = build_dump(&db, "test").await.unwrap();
    let pkg = dump.packages.get("hello").expect("package missing");

    assert_eq!(pkg.platforms, ["x86_64", "aarch64"]);
    // The empty entry from `--noconfirm;;--nocolor` is dropped: it means the
    // same set, and a restore should not inherit the difference.
    assert_eq!(pkg.build_flags, ["--noconfirm", "--nocolor"]);
    assert!(pkg.directly_requested);

    // Nothing derived reaches the file. Asserted against the serialised form,
    // because that is what a restore reads.
    let json = serde_json::to_string(&dump.packages).unwrap();
    for derived in [
        "status",
        "out_of_date",
        "upstream_version",
        "split_packages",
        "latest_build",
    ] {
        assert!(
            !json.contains(derived),
            "the dump carries derived field {derived}: {json}"
        );
    }
}

/// A dependency stays a dependency. Restoring everything as
/// directly-requested would turn packages nobody asked for into packages the
/// user now has to manage.
#[tokio::test]
async fn a_dependency_is_not_promoted_to_a_request() {
    let db = db().await;
    package(&db, "requested", true, None).await;
    package(&db, "pulled-in", false, None).await;

    let dump = build_dump(&db, "test").await.unwrap();
    assert!(dump.packages["requested"].directly_requested);
    assert!(!dump.packages["pulled-in"].directly_requested);
}

/// A patch travels as a file, and the flag mirrors it. The format's rule is
/// that a patch is present iff its file is, so the two cannot disagree.
#[tokio::test]
async fn a_patch_travels_as_a_file() {
    let db = db().await;
    package(&db, "patched", true, Some("--- a\n+++ b\n")).await;
    package(&db, "plain", true, None).await;

    let dump = build_dump(&db, "test").await.unwrap();
    assert_eq!(
        dump.patches.get("patched").map(String::as_str),
        Some("--- a\n+++ b\n")
    );
    assert!(!dump.patches.contains_key("plain"));
    assert!(dump.packages["patched"].has_patch);
    assert!(!dump.packages["plain"].has_patch);

    let files = archive_files(&write_archive(&dump).unwrap());
    assert!(files.contains_key("patches/patched.patch"));
    assert!(!files.contains_key("patches/plain.patch"));
}

/// Settings are re-keyed from row ids onto pkgbase, which is the only name a
/// dump uses.
#[tokio::test]
async fn settings_are_keyed_by_pkgbase() {
    let db = db().await;
    let pkg_id = package(&db, "hello", true, None).await;

    for (key, value, scope) in [
        // Global settings live under the sentinel id, not NULL.
        ("version_check_interval", "600", Some(-1)),
        ("build_flags", "--nocheck", Some(pkg_id)),
    ] {
        settings::ActiveModel {
            key: Set(key.to_string()),
            value: Set(Some(value.to_string())),
            pkg_id: Set(scope),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();
    }

    let dump = build_dump(&db, "test").await.unwrap();
    assert_eq!(
        dump.settings
            .global
            .get("version_check_interval")
            .map(String::as_str),
        Some("600")
    );
    assert_eq!(
        dump.settings.packages["hello"]
            .get("build_flags")
            .map(String::as_str),
        Some("--nocheck")
    );
}

/// Only approved workers. A pending enrolment is a decision nobody has made,
/// and restoring it would carry an unanswered question into the new instance.
#[tokio::test]
async fn only_approved_workers_are_carried() {
    let db = db().await;
    for (name, status) in [
        (
            "trusted",
            aurcache_common::api::worker::ApprovalStatus::Approved,
        ),
        (
            "waiting",
            aurcache_common::api::worker::ApprovalStatus::Pending,
        ),
    ] {
        workers::ActiveModel {
            name: Set(name.to_string()),
            status: Set(status),
            cert_fingerprint: Set(format!("fp-{name}")),
            native_arches: Set("x86_64".to_string()),
            emulated_arches: Set(String::new()),
            package_affinity: Set(String::new()),
            priority: Set(0),
            concurrency: Set(1),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();
    }

    let dump = build_dump(&db, "test").await.unwrap();
    let names: Vec<&str> = dump.workers.iter().map(|w| w.name.as_str()).collect();
    assert_eq!(names, ["trusted"]);
    // The fingerprint is the identity a re-enrolling worker is recognised by.
    assert_eq!(dump.workers[0].cert_fingerprint, "fp-trusted");
}

/// The archive is what an importer actually reads, so its shape is part of the
/// contract: the manifest first, and a schema version to refuse on.
#[tokio::test]
async fn the_archive_holds_the_documented_files() {
    let db = db().await;
    package(&db, "hello", true, None).await;

    let dump = build_dump(&db, "test").await.unwrap();
    let files = archive_files(&write_archive(&dump).unwrap());

    for expected in [
        MANIFEST_FILE,
        PACKAGES_FILE,
        "settings.json",
        "workers.json",
    ] {
        assert!(files.contains_key(expected), "missing {expected}");
    }
    let manifest: serde_json::Value = serde_json::from_str(&files[MANIFEST_FILE]).unwrap();
    assert_eq!(manifest["schema_version"], DUMP_SCHEMA_VERSION);
    assert_eq!(manifest["includes_secrets"], false);
}

/// Two dumps of an unchanged instance differ only by their timestamp, so one
/// kept in git shows real changes rather than reordering.
#[tokio::test]
async fn the_output_is_ordered_and_stable() {
    let db = db().await;
    for name in ["zzz", "aaa", "mmm"] {
        package(&db, name, true, None).await;
    }

    let first = build_dump(&db, "test").await.unwrap();
    let second = build_dump(&db, "test").await.unwrap();
    assert_eq!(
        serde_json::to_string(&first.packages).unwrap(),
        serde_json::to_string(&second.packages).unwrap()
    );
    let names: Vec<&str> = first.packages.keys().map(String::as_str).collect();
    assert_eq!(names, ["aaa", "mmm", "zzz"]);
}

/// A git-sourced package keeps its remote, ref and subfolder: that is the
/// whole of what identifies it, and none of it can be worked out again.
#[tokio::test]
async fn a_git_source_is_carried_whole() {
    let db = db().await;
    packages::ActiveModel {
        name: Set("mine".to_string()),
        status: Set(0),
        out_of_date: Set(0),
        build_flags: Set(String::new()),
        platforms: Set("x86_64".to_string()),
        source_type: Set(SourceType::Git),
        source_data: Set(SourceData::Git {
            spec: GitSourceSpec {
                url: "https://example.com/mine.git".to_string(),
                r#ref: "v2".to_string(),
                subfolder: "pkg".to_string(),
            },
        }),
        directly_requested: Set(true),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();

    let dump = build_dump(&db, "test").await.unwrap();
    match &dump.packages["mine"].source_data {
        SourceData::Git { spec } => {
            assert_eq!(spec.url, "https://example.com/mine.git");
            assert_eq!(spec.r#ref, "v2");
            assert_eq!(spec.subfolder, "pkg");
        }
        other => panic!("git source became {other:?}"),
    }
}

/// Read a `.tar.gz` into path -> contents.
fn archive_files(bytes: &[u8]) -> HashMap<String, String> {
    let mut archive = tar::Archive::new(GzDecoder::new(bytes));
    archive
        .entries()
        .unwrap()
        .map(|entry| {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().into_owned();
            let mut content = String::new();
            entry.read_to_string(&mut content).unwrap();
            (path, content)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Restore
// ---------------------------------------------------------------------------

use aurcache_common::api::dump::{ExistingPackagePolicy, RestoreOptions, RestoreOutcome};
use aurcache_utils::restore::{load_dump, preview};

/// A dump of whatever is in `db`, as bytes.
async fn dump_bytes(db: &DatabaseConnection) -> Vec<u8> {
    write_archive(&build_dump(db, "test").await.unwrap()).unwrap()
}

/// A dump round-trips: what comes out describes what went in.
#[tokio::test]
async fn a_dump_reloads_as_what_it_described() {
    let source = db().await;
    package(&source, "hello", true, Some("--- a\n")).await;
    package(&source, "dep", false, None).await;

    let loaded = load_dump(&dump_bytes(&source).await).unwrap();
    assert_eq!(loaded.packages.len(), 2);
    assert!(loaded.packages["hello"].directly_requested);
    assert!(!loaded.packages["dep"].directly_requested);
    assert_eq!(
        loaded.patches.get("hello").map(String::as_str),
        Some("--- a\n")
    );
}

/// A preview says what would happen and changes nothing -- which is the only
/// useful thing to know before importing over a live instance.
#[tokio::test]
async fn a_preview_reports_without_applying() {
    let source = db().await;
    package(&source, "already-here", true, None).await;
    package(&source, "brand-new", true, None).await;
    let bytes = dump_bytes(&source).await;

    let target = db().await;
    package(&target, "already-here", true, None).await;

    let loaded = load_dump(&bytes).unwrap();
    let entries = preview(&target, &loaded, &RestoreOptions::default())
        .await
        .unwrap();

    let by_name: HashMap<&str, &RestoreOutcome> = entries
        .iter()
        .map(|e| (e.pkgbase.as_str(), &e.outcome))
        .collect();
    assert!(matches!(by_name["already-here"], RestoreOutcome::Skipped));
    assert!(matches!(by_name["brand-new"], RestoreOutcome::Imported));

    // Nothing was written: the package the dump would have added is still absent.
    let count = Packages::find().all(&target).await.unwrap().len();
    assert_eq!(count, 1, "a preview wrote to the database");
}

/// Overwrite is opt-in, and it changes what the preview promises.
#[tokio::test]
async fn overwrite_is_reported_as_overwrite() {
    let source = db().await;
    package(&source, "shared", true, None).await;
    let bytes = dump_bytes(&source).await;

    let target = db().await;
    package(&target, "shared", true, None).await;

    let loaded = load_dump(&bytes).unwrap();
    let entries = preview(
        &target,
        &loaded,
        &RestoreOptions {
            dry_run: true,
            on_existing: ExistingPackagePolicy::Overwrite,
        },
    )
    .await
    .unwrap();
    assert!(matches!(entries[0].outcome, RestoreOutcome::Overwritten));
}

/// The invariant the whole ordering rests on: an imported row must land in a
/// status `resolve_local_dependency_resolutions` actually queries. In any other
/// state the row is invisible to resolution, and a dependency on it would fall
/// through to the AUR and adopt a package in place of the one just imported --
/// exactly what importing it was meant to prevent.
#[tokio::test]
async fn an_imported_row_is_visible_to_dependency_resolution() {
    let source = db().await;
    package(&source, "hello", true, None).await;
    let bytes = dump_bytes(&source).await;

    let target = db().await;
    let loaded = load_dump(&bytes).unwrap();
    aurcache_utils::restore::write_rows(&target, &loaded, &RestoreOptions::default())
        .await
        .unwrap();

    let row = Packages::find().one(&target).await.unwrap().unwrap();
    // The set `resolve_local_dependency_resolutions` filters on.
    let visible = [
        aurcache_common::builder::BuildStates::ACTIVE_BUILD,
        aurcache_common::builder::BuildStates::SUCCESSFUL_BUILD,
        aurcache_common::builder::BuildStates::ENQUEUED_BUILD,
    ];
    assert!(
        visible.contains(&row.status),
        "an imported package is invisible to dependency resolution (status {})",
        row.status
    );
}

/// Skip leaves an existing package exactly as it was. The default has to be
/// the one that cannot destroy configuration nobody meant to replace.
#[tokio::test]
async fn skip_leaves_the_existing_configuration_alone() {
    let source = db().await;
    package(&source, "shared", true, None).await;
    let bytes = dump_bytes(&source).await;

    let target = db().await;
    let id = package(&target, "shared", true, None).await;
    // Something the dump does not carry, so a wrongly-applied overwrite shows.
    let mut existing: packages::ActiveModel = Packages::find_by_id(id)
        .one(&target)
        .await
        .unwrap()
        .unwrap()
        .into();
    existing.platforms = Set("aarch64".to_string());
    existing.update(&target).await.unwrap();

    let loaded = load_dump(&bytes).unwrap();
    let applied = aurcache_utils::restore::write_rows(&target, &loaded, &RestoreOptions::default())
        .await
        .unwrap();

    let row = Packages::find_by_id(id)
        .one(&target)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.platforms, "aarch64", "skip overwrote the target");
    assert!(
        applied.touched.is_empty(),
        "a skipped package should not be re-resolved: it was already here"
    );
}

/// Overwrite replaces the configuration the dump owns.
#[tokio::test]
async fn overwrite_replaces_the_configuration() {
    let source = db().await;
    package(&source, "shared", true, None).await;
    let bytes = dump_bytes(&source).await;

    let target = db().await;
    let id = package(&target, "shared", true, None).await;
    let mut existing: packages::ActiveModel = Packages::find_by_id(id)
        .one(&target)
        .await
        .unwrap()
        .unwrap()
        .into();
    existing.platforms = Set("aarch64".to_string());
    existing.update(&target).await.unwrap();

    let loaded = load_dump(&bytes).unwrap();
    aurcache_utils::restore::write_rows(
        &target,
        &loaded,
        &RestoreOptions {
            dry_run: false,
            on_existing: ExistingPackagePolicy::Overwrite,
        },
    )
    .await
    .unwrap();

    let row = Packages::find_by_id(id)
        .one(&target)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.platforms, "x86_64;aarch64", "overwrite did not apply");
}

/// Settings come back under the right package, having travelled by name.
#[tokio::test]
async fn settings_are_restored_against_the_right_package() {
    let source = db().await;
    let pkg_id = package(&source, "hello", true, None).await;
    for (key, value, scope) in [
        ("version_check_interval", "600", Some(-1)),
        ("build_flags", "--nocheck", Some(pkg_id)),
    ] {
        settings::ActiveModel {
            key: Set(key.to_string()),
            value: Set(Some(value.to_string())),
            pkg_id: Set(scope),
            ..Default::default()
        }
        .insert(&source)
        .await
        .unwrap();
    }
    let bytes = dump_bytes(&source).await;

    let target = db().await;
    let loaded = load_dump(&bytes).unwrap();
    aurcache_utils::restore::write_rows(&target, &loaded, &RestoreOptions::default())
        .await
        .unwrap();

    let new_id = Packages::find().one(&target).await.unwrap().unwrap().id;
    let rows = settings::Entity::find().all(&target).await.unwrap();
    let global = rows
        .iter()
        .find(|r| r.key == "version_check_interval")
        .unwrap();
    assert_eq!(global.pkg_id, Some(-1));
    let scoped = rows.iter().find(|r| r.key == "build_flags").unwrap();
    assert_eq!(
        scoped.pkg_id,
        Some(new_id),
        "a per-package setting landed on the wrong package"
    );
}
