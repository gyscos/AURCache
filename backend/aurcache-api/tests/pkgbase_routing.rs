//! The public API keys packages by pkgbase, and a pkgbase may contain `+`.
//!
//! 187 of the 161,057 packages in the AUR have one — `a+`, `aewm++`,
//! `antlr3-c++-devel`. That character is the reason these routes take the
//! pkgbase as a **path segment** rather than a query parameter: Rocket decodes
//! a path with `percent_decode_lossy` (where `+` is literal) but decodes a
//! query value with `url_decode`, which replaces `+` with a space first. A
//! `?pkgbase=aewm++` filter therefore looks up `aewm  ` and quietly finds
//! nothing.
//!
//! These tests pin that down, because it is invisible until someone adds one of
//! those packages.

use std::sync::Arc;

use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_db::migration::Migrator;
use aurcache_db::packages;
use aurcache_db::packages::{SourceData, SourceType};
use aurcache_db::prelude::Packages;
use aurcache_types::builder::Action;
use aurcache_utils::snapshot::SnapshotStore;
use rocket::http::Status;
use rocket::local::asynchronous::Client;
use rocket::tokio::sync::broadcast;
use sea_orm::ActiveValue::Set;
use sea_orm::{Database, DatabaseConnection, EntityTrait};
use sea_orm_migration::MigratorTrait;

/// Names that exercise every character the AUR actually uses, plus the two
/// fully-numeric names that exist (`1337`, `67`) — those are why the routes
/// cannot accept an id *or* a name and guess between them.
const NAMES: [&str; 6] = [
    "aewm++",
    "a+",
    "antlr3-c++-devel",
    "2048.c",
    "gtk_theme-git",
    "1337",
];

async fn seed(db: &DatabaseConnection, name: &str) -> i32 {
    let model = packages::ActiveModel {
        name: Set(name.to_string()),
        status: Set(0),
        out_of_date: Set(0),
        build_flags: Set(String::new()),
        platforms: Set("x86_64".to_string()),
        source_type: Set(SourceType::Aur),
        source_data: Set(SourceData::Aur {
            name: name.to_string(),
        }),
        directly_requested: Set(true),
        ..Default::default()
    };
    Packages::insert(model)
        .exec(db)
        .await
        .expect("insert package")
        .last_insert_id
}

/// Mounts the *real* route table, so the test exercises the same URL matching
/// and decoding production does rather than a reduced stand-in. That means
/// supplying every piece of state the mounted routes declare, even the ones
/// these tests never call — Rocket verifies that up front.
async fn test_client() -> (Client, DatabaseConnection) {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    Migrator::up(&db, None).await.unwrap();

    let checkouts = tempfile::tempdir().expect("tempdir");
    let rocket = rocket::build()
        .manage(db.clone())
        .manage(ActivityLog::new(db.clone()))
        .manage(broadcast::channel::<Action>(16).0)
        .manage(Arc::new(SnapshotStore::with_checkout_root(
            checkouts.path().to_path_buf(),
        )))
        .mount("/api", aurcache_api::backend::build_api());
    // The checkout root only has to outlive Rocket's construction; nothing in
    // these tests fetches sources.
    std::mem::forget(checkouts);
    (Client::tracked(rocket).await.unwrap(), db)
}

/// A pkgbase containing `+` must resolve to *that* package, not to a
/// space-mangled name and not to a different row.
#[rocket::async_test]
async fn packages_with_special_characters_resolve_by_name() {
    let (client, db) = test_client().await;
    for name in NAMES {
        seed(&db, name).await;
    }

    for name in NAMES {
        let response = client.get(format!("/api/package/{name}")).dispatch().await;
        assert_eq!(
            response.status(),
            Status::Ok,
            "GET /api/package/{name} should resolve"
        );
        let body = response.into_string().await.expect("body");
        assert!(
            body.contains(&format!("\"name\":\"{name}\"")),
            "GET /api/package/{name} returned the wrong package: {body}"
        );
    }
}

/// The numeric names are the reason there is no id fallback: with one, `1337`
/// would resolve to whatever row happens to hold that id.
#[rocket::async_test]
async fn a_numeric_pkgbase_is_a_name_not_an_id() {
    let (client, db) = test_client().await;
    // Seed something else first so ids and names cannot coincide by accident.
    seed(&db, "first-package").await;
    let numeric_id = seed(&db, "1337").await;
    assert_ne!(numeric_id, 1337, "test would be vacuous if the id matched");

    let response = client.get("/api/package/1337").dispatch().await;
    assert_eq!(response.status(), Status::Ok);
    let body = response.into_string().await.expect("body");
    assert!(
        body.contains("\"name\":\"1337\""),
        "numeric pkgbase resolved to something else: {body}"
    );
}

/// Sub-resources take the pkgbase in the path for the same reason the package
/// route does, so they need the same guarantee.
#[rocket::async_test]
async fn sub_resources_accept_a_plus_in_the_pkgbase() {
    let (client, db) = test_client().await;
    seed(&db, "aewm++").await;

    for path in [
        "/api/package/aewm++/settings",
        "/api/package/aewm++/builds",
        "/api/package/aewm++/source/files",
    ] {
        let status = client.get(path).dispatch().await.status();
        assert_ne!(
            status,
            Status::NotFound,
            "{path} 404'd, so the pkgbase did not survive the URL"
        );
    }
}

/// An unknown pkgbase is a 404, not a wrong package.
#[rocket::async_test]
async fn an_unknown_pkgbase_is_not_found() {
    let (client, db) = test_client().await;
    seed(&db, "aewm++").await;

    // `aewm  ` is what `aewm++` decodes to if it is ever put through
    // form-urlencoded decoding, so this is the failure mode being guarded.
    let response = client.get("/api/package/aewm%20%20").dispatch().await;
    assert_eq!(response.status(), Status::NotFound);
}
