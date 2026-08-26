//! The frontend is a single-page app, so `/builds` is a route it renders, not
//! a file. These tests pin the server half of that: unknown UI paths get the
//! app shell, while the API keeps its own 404s.
//!
//! Needs the `static` feature, which is what embeds the frontend and mounts the
//! asset handler. Run with `cargo test -p aurcache-api --features static`.

#![cfg(feature = "static")]

use std::sync::Arc;

use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_db::action::Action;
use aurcache_db::migration::Migrator;
use aurcache_db::packages;
use aurcache_db::packages::{SourceData, SourceType};
use aurcache_db::prelude::Packages;
use aurcache_utils::snapshot::SnapshotStore;
use rocket::http::Status;
use rocket::local::asynchronous::Client;
use rocket::tokio::sync::broadcast;
use sea_orm::ActiveValue::Set;
use sea_orm::{Database, DatabaseConnection, EntityTrait};
use sea_orm_migration::MigratorTrait;

/// Mounts the API *and* the asset handler, in the same order and at the same
/// paths production does. Mount order is the point: the fallback must not
/// shadow the API.
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
        .mount("/api", aurcache_api::backend::build_api())
        .mount("/", aurcache_api::embed::CustomHandler);
    std::mem::forget(checkouts);
    (Client::tracked(rocket).await.unwrap(), db)
}

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

/// The reason the fallback exists. Without it these 404 on reload or when the
/// link is pasted fresh, while working when reached by clicking.
#[rocket::async_test]
async fn frontend_routes_are_served_the_app_shell() {
    let (client, _db) = test_client().await;

    for path in [
        "/builds",
        "/build/12",
        "/packages",
        "/package/hello",
        "/package/hello/source/PKGBUILD",
        "/settings",
        // Dots in a package name must not be read as a file extension.
        "/package/2048.c",
        "/package/python-3.11",
    ] {
        let response = client.get(path).dispatch().await;
        assert_eq!(
            response.status(),
            Status::Ok,
            "GET {path} should serve the app"
        );
        let body = response.into_string().await.expect("body");
        assert!(
            body.contains("<!doctype html>") || body.contains("<!DOCTYPE html>"),
            "GET {path} did not return the app shell: {body}"
        );
    }
}

/// The risk the fallback introduces: a catch-all that swallows the API. A real
/// API route has to keep winning over it.
#[rocket::async_test]
async fn the_fallback_does_not_shadow_the_api() {
    let (client, db) = test_client().await;
    seed(&db, "hello").await;

    let response = client.get("/api/package/hello").dispatch().await;
    assert_eq!(response.status(), Status::Ok);
    let body = response.into_string().await.expect("body");
    assert!(
        body.contains("\"name\":\"hello\""),
        "the API returned something else — the fallback may be shadowing it: {body}"
    );
}

/// An unmatched API path stays a 404. Returning the shell would hand a client
/// HTML where it expects JSON, turning a clear status into a parse error.
#[rocket::async_test]
async fn unmatched_api_paths_still_404() {
    let (client, _db) = test_client().await;

    for path in ["/api/nope", "/api/package/hello/not-a-thing", "/api"] {
        let response = client.get(path).dispatch().await;
        assert_eq!(
            response.status(),
            Status::NotFound,
            "GET {path} should be a 404, not the app shell"
        );
    }
}

/// A package that does not exist is still a valid *frontend* route: the app
/// loads and renders its own "not found". Only the API knows the difference.
#[rocket::async_test]
async fn an_unknown_package_is_still_a_frontend_route() {
    let (client, _db) = test_client().await;

    let response = client.get("/package/does-not-exist").dispatch().await;
    assert_eq!(response.status(), Status::Ok);

    let api = client.get("/api/package/does-not-exist").dispatch().await;
    assert_ne!(
        api.status(),
        Status::Ok,
        "the API must still report an unknown package"
    );
}
