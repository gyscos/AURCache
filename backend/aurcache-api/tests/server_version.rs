//! The sidebar shows the running server's version, so this pins the endpoint
//! it reads: `GET /api/version` answers the managed `ServerVersion` in the
//! shape the client and the server share.

use std::sync::Arc;

use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_db::action::Action;
use aurcache_db::migration::Migrator;
use aurcache_utils::snapshot::SnapshotStore;
use rocket::http::Status;
use rocket::local::asynchronous::Client;
use rocket::tokio::sync::broadcast;
use sea_orm::Database;
use sea_orm_migration::MigratorTrait;

/// Mounts the *real* route table, so the test exercises the same URL matching
/// production does rather than a reduced stand-in. That means supplying every
/// piece of state the mounted routes declare, even the ones this test never
/// calls — Rocket verifies that up front.
async fn test_client() -> Client {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    Migrator::up(&db, None).await.unwrap();

    let checkouts = tempfile::tempdir().expect("tempdir");
    let rocket = rocket::build()
        .manage(db.clone())
        .manage(ActivityLog::discarding())
        .manage(broadcast::channel::<Action>(16).0)
        // Routes that act on packages take the bundle; this test never reaches
        // one, but Rocket refuses to launch with an unmanaged type.
        .manage(Arc::new(SnapshotStore::with_checkout_root(
            checkouts.path().to_path_buf(),
        )))
        .manage(aurcache_utils::services::Services::new(
            db.clone(),
            broadcast::channel::<Action>(16).0,
            Arc::new(SnapshotStore::with_checkout_root(
                checkouts.path().to_path_buf(),
            )),
            Arc::new(aurcache_deps::AurClient::new()),
            Arc::new(aurcache_utils::repository::Repository::new(
                checkouts.path().join("repo"),
            )),
            ActivityLog::discarding(),
        ))
        // The version under test: the route must report this, not any crate's
        // own compile-time version.
        .manage(aurcache_api::init::ServerVersion(
            "0.5.0+g8afa04a.dirty".to_string(),
        ))
        // Dump and restore both move the CA's files, so both need to know
        // where they are. Rocket's sentinels refuse to launch without it.
        .manage(aurcache_api::init::CaDirectory(std::path::PathBuf::from(
            "/nonexistent-ca-dir",
        )))
        .mount("/api", aurcache_api::backend::build_api());
    // The checkout root only has to outlive Rocket's construction; nothing in
    // this test fetches sources.
    std::mem::forget(checkouts);
    Client::tracked(rocket).await.unwrap()
}

/// The version route reports the running server in the shape the client and
/// the server share — deserialized as the client's own type, not a string
/// match, so the two cannot silently drift apart.
#[rocket::async_test]
async fn version_reports_the_running_server() {
    let client = test_client().await;
    let response = client.get("/api/version").dispatch().await;
    assert_eq!(response.status(), Status::Ok);
    let body = response.into_string().await.expect("body");
    let info: aurcache_client::ServerInfo =
        serde_json::from_str(&body).expect("the version shape the client shares");
    assert_eq!(info.version, "0.5.0+g8afa04a.dirty");
}
