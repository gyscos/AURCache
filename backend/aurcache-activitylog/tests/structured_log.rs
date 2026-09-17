//! The structured log end to end: emit through the queue, read it back.
//!
//! Against a real (in-memory) database rather than a mock, because what is
//! being tested is the storage: the entity index written beside each entry, the
//! filters built on it, and the links resolved when a page is read.

use aurcache_activitylog::activity_utils::spawn;
use aurcache_activitylog::event::LogEvent;
use aurcache_activitylog::log_store::{LogFilter, LogStore};
use aurcache_common::api::activity::Severity;
use aurcache_common::api::log::{BuildRef, EntityRef, PackageRef, WorkerRef};
use aurcache_common::source::SourceData;
use aurcache_db::migration::Migrator;
use aurcache_db::packages::SourceType;
use aurcache_db::prelude::LogEntities;
use aurcache_db::{builds, packages};
use pacman_mirrors::platforms::Platform;
use sea_orm::ActiveValue::Set;
use sea_orm::{ActiveModelTrait, Database, DatabaseConnection, EntityTrait, PaginatorTrait};
use sea_orm_migration::MigratorTrait;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// A few events to log
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DepsReplaced {
    dependent: PackageRef,
    old: PackageRef,
    new: PackageRef,
}

impl LogEvent for DepsReplaced {
    const KIND: &'static str = "deps.replaced";
    const SEVERITY: Severity = Severity::Info;
    fn message(&self) -> String {
        format!(
            "Replaced {} with {} as dependency of {}",
            self.old, self.new, self.dependent
        )
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PublishFailed {
    build: BuildRef,
    error: String,
}

impl LogEvent for PublishFailed {
    const KIND: &'static str = "publish.failed";
    const SEVERITY: Severity = Severity::Error;
    fn message(&self) -> String {
        format!("publishing {} failed: {}", self.build, self.error)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerReaped {
    worker: WorkerRef,
    builds: Vec<BuildRef>,
}

impl LogEvent for WorkerReaped {
    const KIND: &'static str = "worker.reaped";
    const SEVERITY: Severity = Severity::Warning;
    fn message(&self) -> String {
        format!("{} stopped answering", self.worker)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServerStart {
    version: String,
}

impl LogEvent for ServerStart {
    const KIND: &'static str = aurcache_activitylog::kinds::SERVER_START;
    const SEVERITY: Severity = Severity::Info;
    fn message(&self) -> String {
        format!("AURCache {} started", self.version)
    }
}

// ---------------------------------------------------------------------------

async fn db() -> DatabaseConnection {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    Migrator::up(&db, None).await.unwrap();
    db
}

/// Give the entities the log refers to something to resolve against.
async fn seed_package(db: &DatabaseConnection, name: &str) -> i32 {
    packages::ActiveModel {
        name: Set(name.to_string()),
        status: Set(3),
        out_of_date: Set(0),
        upstream_version: Set(None),
        latest_build: Set(None),
        build_flags: Set(String::new()),
        platforms: Set("x86_64".to_string()),
        source_type: Set(SourceType::Aur),
        source_data: Set(SourceData::Aur { name: name.into() }),
        directly_requested: Set(true),
        split_packages: Set(None),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap()
    .id
}

async fn seed_build(db: &DatabaseConnection, pkg_id: i32, number: i32) {
    builds::ActiveModel {
        pkg_id: Set(pkg_id),
        number: Set(number),
        platform: Set(Platform::X86_64),
        version: Set("1-1".to_string()),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap();
}

#[tokio::test]
async fn an_event_reaches_the_log_with_its_index_and_links() {
    let db = db().await;
    seed_package(&db, "baz").await;
    seed_package(&db, "bar").await;
    // `foo` deliberately absent: the package a replacement removes.

    let (log, writer) = spawn(db.clone());
    log.emit_by(
        DepsReplaced {
            dependent: "baz".into(),
            old: "foo".into(),
            new: "bar".into(),
        },
        Some("alice".to_string()),
    );
    drop(log);
    writer.await.unwrap();

    let page = LogStore::new(db)
        .page(50, 0, &LogFilter::default())
        .await
        .unwrap();

    assert_eq!(page.total, 1);
    let entry = &page.entries[0];
    assert_eq!(entry.kind, "deps.replaced");
    assert_eq!(entry.severity, Severity::Info);
    assert_eq!(entry.user.as_deref(), Some("alice"));
    assert_eq!(
        entry.message,
        "Replaced pkg:foo with pkg:bar as dependency of pkg:baz"
    );
    assert_eq!(entry.data["old"], "pkg:foo");

    // The whole point of resolving at read time: `foo` was removed by the very
    // request that recorded this, so it has no page, while the other two do.
    assert_eq!(entry.hrefs["dependent"], vec![Some("/package/baz".into())]);
    assert_eq!(entry.hrefs["new"], vec![Some("/package/bar".into())]);
    assert_eq!(entry.hrefs["old"], vec![None]);
}

/// "Everything about foo" finds the row whatever role foo played, and the
/// query names no kind -- it works for kinds that did not exist when it was
/// written.
#[tokio::test]
async fn the_entity_filter_finds_a_row_in_any_role() {
    let db = db().await;
    let (log, writer) = spawn(db.clone());
    log.emit(DepsReplaced {
        dependent: "baz".into(),
        old: "foo".into(),
        new: "bar".into(),
    });
    log.emit(DepsReplaced {
        dependent: "unrelated".into(),
        old: "other".into(),
        new: "another".into(),
    });
    drop(log);
    writer.await.unwrap();

    let store = LogStore::new(db);
    for role_played in ["baz", "foo", "bar"] {
        let page = store
            .page(
                50,
                0,
                &LogFilter {
                    entity: Some(PackageRef::from(role_played).into()),
                    ..LogFilter::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(page.total, 1, "{role_played}");
    }

    // And narrowed to one role, only that role matches.
    let as_dependent = store
        .page(
            50,
            0,
            &LogFilter {
                entity: Some(PackageRef::from("foo").into()),
                role: Some("dependent".to_string()),
                ..LogFilter::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(as_dependent.total, 0);
}

/// A role naming several entities is indexed once per entity.
#[tokio::test]
async fn a_list_role_is_indexed_for_each_entity() {
    let db = db().await;
    let (log, writer) = spawn(db.clone());
    log.emit(WorkerReaped {
        worker: "builder-01".into(),
        builds: vec![
            BuildRef {
                pkgbase: "hello".into(),
                number: 7,
            },
            BuildRef {
                pkgbase: "yay".into(),
                number: 3,
            },
        ],
    });
    drop(log);
    writer.await.unwrap();

    let store = LogStore::new(db);
    for entity in [
        EntityRef::from(WorkerRef::from("builder-01")),
        EntityRef::from(BuildRef {
            pkgbase: "hello".into(),
            number: 7,
        }),
        EntityRef::from(BuildRef {
            pkgbase: "yay".into(),
            number: 3,
        }),
    ] {
        let page = store
            .page(
                50,
                0,
                &LogFilter {
                    entity: Some(entity.clone()),
                    ..LogFilter::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(page.total, 1, "{entity}");
    }
}

/// Severity is stored as an ordered number, so the filter is "this and worse"
/// without knowing which kinds exist.
#[tokio::test]
async fn severity_narrows_to_this_and_worse() {
    let db = db().await;
    let (log, writer) = spawn(db.clone());
    log.emit(DepsReplaced {
        dependent: "baz".into(),
        old: "foo".into(),
        new: "bar".into(),
    });
    log.emit(WorkerReaped {
        worker: "builder-01".into(),
        builds: vec![],
    });
    log.emit(PublishFailed {
        build: BuildRef {
            pkgbase: "hello".into(),
            number: 7,
        },
        error: "no space left on device".to_string(),
    });
    drop(log);
    writer.await.unwrap();

    let store = LogStore::new(db);
    let all = store.page(50, 0, &LogFilter::default()).await.unwrap();
    assert_eq!(all.total, 3);

    let warnings = store
        .page(
            50,
            0,
            &LogFilter {
                severity: Some(Severity::Warning),
                ..LogFilter::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(warnings.total, 2);
    assert!(
        warnings
            .entries
            .iter()
            .all(|e| e.severity >= Severity::Warning)
    );

    let errors = store
        .page(
            50,
            0,
            &LogFilter {
                severity: Some(Severity::Error),
                ..LogFilter::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(errors.total, 1);
    assert_eq!(errors.entries[0].kind, "publish.failed");
}

/// A scoped handle files everything it records under one entity, so a build's
/// page can ask for both what was said about it and what happened during it.
#[tokio::test]
async fn a_scope_is_indexed_like_any_other_reference() {
    let db = db().await;
    let build = BuildRef {
        pkgbase: "hello".into(),
        number: 7,
    };

    let (log, writer) = spawn(db.clone());
    // Emitted *during* the build: the payload names it nowhere.
    log.scoped(build.clone()).emit(DepsReplaced {
        dependent: "baz".into(),
        old: "foo".into(),
        new: "bar".into(),
    });
    drop(log);
    writer.await.unwrap();

    let page = LogStore::new(db)
        .page(
            50,
            0,
            &LogFilter {
                entity: Some(build.clone().into()),
                ..LogFilter::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(page.total, 1);
    assert_eq!(page.entries[0].scope, Some(build.into()));
}

/// "Since the last restart" counts back to the newest start entry, and a log
/// with no start entry yet shows everything rather than nothing.
#[tokio::test]
async fn since_boot_cuts_at_the_last_start() {
    let db = db().await;
    let (log, writer) = spawn(db.clone());
    log.emit(WorkerReaped {
        worker: "builder-01".into(),
        builds: vec![],
    });
    drop(log);
    writer.await.unwrap();

    let store = LogStore::new(db.clone());
    let no_marker = store
        .page(
            50,
            0,
            &LogFilter {
                since_boot: true,
                ..LogFilter::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(no_marker.total, 1, "no marker means no narrowing");

    // Now a restart, and something after it.
    let (log, writer) = spawn(db.clone());
    log.emit(ServerStart {
        version: "0.5.0".to_string(),
    });
    drop(log);
    writer.await.unwrap();

    let since = store
        .page(
            50,
            0,
            &LogFilter {
                since_boot: true,
                ..LogFilter::default()
            },
        )
        .await
        .unwrap();
    // The restart itself belongs to the boot it starts. The earlier entry may
    // share its whole-second timestamp, so this asserts the marker is included
    // rather than counting rows.
    assert!(
        since
            .entries
            .iter()
            .any(|e| e.kind == aurcache_activitylog::kinds::SERVER_START),
        "{:?}",
        since.entries
    );
}

/// A build links only when its package is still there, since the page is
/// reached through it.
#[tokio::test]
async fn a_build_links_only_while_its_package_exists() {
    let db = db().await;
    let hello = seed_package(&db, "hello").await;
    seed_build(&db, hello, 7).await;

    let (log, writer) = spawn(db.clone());
    log.emit(PublishFailed {
        build: BuildRef {
            pkgbase: "hello".into(),
            number: 7,
        },
        error: "disk full".to_string(),
    });
    log.emit(PublishFailed {
        build: BuildRef {
            pkgbase: "gone".into(),
            number: 1,
        },
        error: "disk full".to_string(),
    });
    drop(log);
    writer.await.unwrap();

    let page = LogStore::new(db)
        .page(50, 0, &LogFilter::default())
        .await
        .unwrap();
    let mut links: Vec<_> = page
        .entries
        .iter()
        .map(|e| e.hrefs["build"][0].clone())
        .collect();
    links.sort();
    assert_eq!(
        links,
        vec![None, Some("/package/hello/build/7".to_string())]
    );
}

/// One kind covering several cases keeps the difference in a column, so it can
/// be filtered on without splitting into kinds nobody would ask apart -- and
/// without reading into the payload.
#[tokio::test]
async fn a_subkind_narrows_within_a_kind() {
    use aurcache_activitylog::events::source::{RefreshFailed, RefreshTarget};

    let db = db().await;
    let (log, writer) = spawn(db.clone());
    log.emit(RefreshFailed {
        pkg: "hello".into(),
        target: RefreshTarget::Git,
        error: "host is unreachable".to_string(),
    });
    log.emit(RefreshFailed {
        pkg: "yay".into(),
        target: RefreshTarget::Snapshot,
        error: "checksum mismatch".to_string(),
    });
    drop(log);
    writer.await.unwrap();

    let store = LogStore::new(db);
    let kind = RefreshFailed::KIND_STR.to_string();

    let both = store
        .page(
            50,
            0,
            &LogFilter {
                kind: Some(kind.clone()),
                ..LogFilter::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(both.total, 2);

    let git = store
        .page(
            50,
            0,
            &LogFilter {
                kind: Some(kind),
                subkind: Some("git".to_string()),
                ..LogFilter::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(git.total, 1);
    assert_eq!(git.entries[0].subkind.as_deref(), Some("git"));
    // And the message says which, so the row reads without the column.
    assert!(
        git.entries[0].message.contains("git source"),
        "{:?}",
        git.entries[0].message
    );
}

/// Pruning takes the index with it, or the filter would keep finding entries
/// that are gone.
#[tokio::test]
async fn pruning_removes_the_index_too() {
    let db = db().await;
    let (log, writer) = spawn(db.clone());
    log.emit(DepsReplaced {
        dependent: "baz".into(),
        old: "foo".into(),
        new: "bar".into(),
    });
    drop(log);
    writer.await.unwrap();

    let store = LogStore::new(db.clone());
    let now = aurcache_db::helpers::time::now_secs();
    assert_eq!(store.prune(3600, now + 7200).await.unwrap(), 1);

    let orphans = LogEntities::find().count(&db).await.unwrap();
    assert_eq!(orphans, 0, "the cascade should have taken the index rows");
}
