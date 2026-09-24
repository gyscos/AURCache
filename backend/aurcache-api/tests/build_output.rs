//! The build log route, which reads a file rather than a column.
//!
//! Logs moved out of `builds.output` because appending to a row rewrites the
//! whole value; the route now seeks into a file by byte offset. The e2e covers
//! a worker *writing* logs, but on a passing run it never reads one back — the
//! CLI fetch lives in its failure handler — so the read path needs pinning
//! down here.

use std::sync::Arc;

use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_db::action::Action;
use aurcache_db::builds;
use aurcache_db::migration::Migrator;
use aurcache_db::packages;
use aurcache_db::packages::{SourceData, SourceType};
use aurcache_db::prelude::{Builds, Packages};
use aurcache_utils::snapshot::SnapshotStore;
use pacman_mirrors::platforms::Platform;
use rocket::http::Status;
use rocket::local::asynchronous::Client;
use rocket::tokio::sync::broadcast;
use sea_orm::ActiveValue::Set;
use sea_orm::{Database, DatabaseConnection, EntityTrait};
use sea_orm_migration::MigratorTrait;

/// The log root is read from the environment per call and every test's first
/// build gets id 1, so two tests running at once would share a log file.
///
/// A tokio mutex rather than a std one: the guard is held across awaits, which
/// is what tokio's is for and what clippy rightly refuses for std's.
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn test_client(log_root: &std::path::Path) -> (Client, DatabaseConnection) {
    unsafe { std::env::set_var("AURCACHE_BUILD_LOG_PATH", log_root) };

    let db = Database::connect("sqlite::memory:").await.unwrap();
    Migrator::up(&db, None).await.unwrap();

    let checkouts = tempfile::tempdir().expect("tempdir");
    let rocket = rocket::build()
        .manage(db.clone())
        .manage(ActivityLog::discarding())
        .manage(broadcast::channel::<Action>(16).0)
        // Routes that act on packages take the bundle; these tests never reach
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
        .manage(aurcache_api::init::ServerVersion("test".to_string()))
        .manage(aurcache_api::init::CaDirectory(std::path::PathBuf::from(
            "/nonexistent-ca-dir",
        )))
        .mount("/api", aurcache_api::backend::build_api());
    std::mem::forget(checkouts);
    (Client::tracked(rocket).await.unwrap(), db)
}

/// Seed one package with one build. Logs are keyed by `<pkgbase>/<number>`, so
/// the row id never comes into it.
async fn seed(db: &DatabaseConnection) {
    let pkg_id = Packages::insert(packages::ActiveModel {
        name: Set("hello".to_string()),
        source_type: Set(SourceType::Aur),
        source_data: Set(SourceData::Aur {
            name: "hello".to_string(),
        }),
        ..Default::default()
    })
    .exec(db)
    .await
    .unwrap()
    .last_insert_id;

    Builds::insert(builds::ActiveModel {
        number: Set(1),
        pkg_id: Set(pkg_id),
        status: Set(Some(2)),
        platform: Set(Platform::X86_64),
        version: Set("1.0".to_string()),
        ..Default::default()
    })
    .exec(db)
    .await
    .unwrap();
}

async fn get(client: &Client, url: &str) -> (Status, String) {
    let response = client.get(url).dispatch().await;
    let status = response.status();
    (status, response.into_string().await.unwrap_or_default())
}

/// The raw-body variant: the `/output` route answers in bytes, and a test that
/// swims mid-character needs to see them, not a lossy, possibly empty, String.
async fn get_bytes(client: &Client, url: &str) -> (Status, Vec<u8>) {
    let response = client.get(url).dispatch().await;
    let status = response.status();
    (status, response.into_bytes().await.unwrap_or_default())
}

#[rocket::async_test]
async fn reads_the_log_from_a_byte_offset() {
    let _guard = ENV_LOCK.lock().await;
    let root = tempfile::tempdir().unwrap();
    let (client, db) = test_client(root.path()).await;
    seed(&db).await;

    // Named the way the API is: <pkgbase>/<number>.log.
    std::fs::create_dir_all(root.path().join("hello")).unwrap();
    std::fs::write(root.path().join("hello/1.log"), "one\ntwo\n").unwrap();

    let (status, body) = get(&client, "/api/package/hello/build/1/output").await;
    assert_eq!(status, Status::Ok);
    assert_eq!(body, "one\ntwo\n");

    // The offset a caller replays is the byte length of what it already has.
    let (status, body) = get(&client, "/api/package/hello/build/1/output?offset=4").await;
    assert_eq!(status, Status::Ok);
    assert_eq!(body, "two\n");

    // Caller is already up to date.
    let (_, body) = get(&client, "/api/package/hello/build/1/output?offset=8").await;
    assert_eq!(body, "");
}

/// A build with no log file is a normal answer, not a 500: it may have produced
/// nothing yet, or its log may have been removed. The UI renders the difference.
#[rocket::async_test]
async fn a_missing_log_is_empty_rather_than_an_error() {
    let _guard = ENV_LOCK.lock().await;
    let root = tempfile::tempdir().unwrap();
    let (client, db) = test_client(root.path()).await;
    seed(&db).await;

    let (status, body) = get(&client, "/api/package/hello/build/1/output").await;
    assert_eq!(status, Status::Ok);
    assert_eq!(body, "");
}

/// An unknown build is still a 404. Without the row lookup it would be
/// indistinguishable from a build that has logged nothing.
#[rocket::async_test]
async fn an_unknown_build_is_not_found() {
    let _guard = ENV_LOCK.lock().await;
    let root = tempfile::tempdir().unwrap();
    let (client, db) = test_client(root.path()).await;
    seed(&db).await;

    let (status, _) = get(&client, "/api/package/hello/build/99/output").await;
    assert_eq!(status, Status::NotFound);
    let (status, _) = get(&client, "/api/package/nope/build/1/output").await;
    assert_eq!(status, Status::NotFound);
}

/// gcc quotes its diagnostics with multi-byte characters, so an offset landing
/// inside one is reachable in practice. The server answers with the exact raw
/// bytes from the offset — a mid-character cut is a client-side alignment
/// problem, handled by the shared `align` helper, never a server-side guess.
#[rocket::async_test]
async fn an_offset_inside_a_character_returns_raw_bytes() {
    let _guard = ENV_LOCK.lock().await;
    let root = tempfile::tempdir().unwrap();
    let (client, db) = test_client(root.path()).await;
    seed(&db).await;

    std::fs::create_dir_all(root.path().join("hello")).unwrap();

    std::fs::write(root.path().join("hello/1.log"), "option ‘-fno_char8_t’\n").unwrap();

    // "option " is 7 bytes; the quote that follows is 3 (U+2018: e2 80 98).
    // Offset 8 lands on the quote's second byte, which will decode to
    // nothing by itself — but the server does not decode.
    let (status, body) = get_bytes(&client, "/api/package/hello/build/1/output?offset=8").await;
    assert_eq!(status, Status::Ok);
    assert_eq!(body, b"\x80\x98-fno_char8_t\xE2\x80\x99\n");
    assert_eq!(body.len(), 18, "one page, byte-exact");
}

/// The raw byte slice a mid-character offset produces still aligns cleanly:
/// the shared `align` helper is the API consumer's way back to text, and it
/// must eat exactly the two continuation bytes this test wrote.
#[rocket::async_test]
async fn the_output_aligns_after_a_mid_character_offset() {
    let _guard = ENV_LOCK.lock().await;
    let root = tempfile::tempdir().unwrap();
    let (client, db) = test_client(root.path()).await;
    seed(&db).await;

    std::fs::create_dir_all(root.path().join("hello")).unwrap();

    std::fs::write(root.path().join("hello/1.log"), "option ‘-fno_char8_t’\n").unwrap();

    let (_, body) = get_bytes(&client, "/api/package/hello/build/1/output?offset=8").await;
    let (front_skip, back_drop) = aurcache_common::api::build_log::align(&body);
    assert_eq!((front_skip, back_drop), (2, 0));
    let text = String::from_utf8_lossy(&body[front_skip..]);
    assert_eq!(text, "-fno_char8_t’\n");
}
