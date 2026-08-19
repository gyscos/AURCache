//! Hermetic fake-worker protocol test (tier-2, `fake-worker-test`).
//!
//! Boots the real remote-worker mTLS protocol listener in-process
//! (`aurcache_api::init::init_worker_api`) against an in-memory database and a
//! throwaway internal CA, then drives a synthetic worker through the full wire
//! protocol using the production [`aurcache_worker`] client:
//!
//!   enroll (token auto-approve) -> claim -> upload artifact -> complete.
//!
//! It asserts the server wired the pieces together correctly (mTLS cert ->
//! approved worker mapping, claim -> `JobDescriptor`, artifact staging, and
//! `complete{success}` -> repo ingest + `files` table) and that the safety
//! rails fire (wrong artifact name rejected, revoked worker refused).
//!
//! It is fully offline: the seeded package points at a nonexistent local git
//! path so the server's best-effort `.SRCINFO` fetch fails instantly (empty
//! `pgp_keys`) instead of reaching the network, and the worker uploads a
//! hand-crafted minimal `*.pkg.tar.zst` rather than building one. The real
//! chroot build + source download are covered by the tier-3 e2e
//! (`scripts/test-e2e.sh`).

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use aurcache_db::builds;
use aurcache_db::files;
use aurcache_db::helpers::worker_jobs::{STATUS_ACTIVE, STATUS_SUCCESS};
use aurcache_db::helpers::worker_store;
use aurcache_db::migration::Migrator;
use aurcache_types::worker::{ClaimRequest, CompleteReport};
use aurcache_worker::client::{WorkerClient, fetch_and_pin_ca};
use aurcache_worker::config::Config;
use aurcache_worker::enroll::ensure_enrolled;
use aurcache_worker::identity::Identity;
use sea_orm::{ColumnTrait, ConnectionTrait, Database, DatabaseConnection, EntityTrait, QueryFilter};
use sea_orm_migration::MigratorTrait;

/// Craft a minimal but structurally valid `*.pkg.tar.zst`: a zstd-compressed
/// tar containing a single `.PKGINFO` with the fields `repo_add` requires
/// (`pkgname` + `pkgver` non-empty). Returned as (filename, bytes).
fn make_pkg(name: &str, ver: &str) -> (String, Vec<u8>) {
    let pkginfo = format!(
        "pkgname = {name}\npkgbase = {name}\npkgver = {ver}\npkgdesc = fake test package\n\
         url = https://example.invalid\nbuilddate = 0\npackager = test <test@localhost>\n\
         size = 0\narch = x86_64\n"
    );

    let mut tar = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_path(".PKGINFO").unwrap();
    header.set_size(pkginfo.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    tar.append(&header, pkginfo.as_bytes()).unwrap();
    let tar_bytes = tar.into_inner().unwrap();

    let mut enc = zstd::Encoder::new(Vec::new(), 0).unwrap();
    enc.write_all(&tar_bytes).unwrap();
    let bytes = enc.finish().unwrap();

    (format!("{name}-{ver}-x86_64.pkg.tar.zst"), bytes)
}

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

async fn seed_build(db: &DatabaseConnection, id: i32, platform: &str, start: i64) {
    // Nonexistent local git path -> the server's best-effort `.SRCINFO` fetch
    // during `claim` fails instantly (offline, empty pgp_keys) rather than
    // hitting the AUR.
    let source = r#"{"type":"git","url":"/nonexistent-aurcache-fake-worker","ref":"HEAD","subfolder":""}"#;
    db.execute_unprepared(&format!(
        "INSERT INTO packages (id, name, build_flags, source_type, source_data, platforms) \
         VALUES ({id}, 'p{id}', '', 'git', '{source}', '{platform}')"
    ))
    .await
    .unwrap();
    db.execute_unprepared(&format!(
        "INSERT INTO builds (id, pkg_id, status, start_time, platform, version, attempt_count) \
         VALUES ({id}, {id}, 3, {start}, '{platform}', '1.0', 0)"
    ))
    .await
    .unwrap();
}

async fn build_status(db: &DatabaseConnection, id: i32) -> i32 {
    builds::Entity::find_by_id(id)
        .one(db)
        .await
        .unwrap()
        .unwrap()
        .status
        .unwrap()
}

#[tokio::test]
async fn fake_worker_protocol_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let ca_dir = tmp.path().join("ca");
    let worker_data = tmp.path().join("worker");
    std::env::set_current_dir(tmp.path()).unwrap();

    let port = free_port();
    let token = "s3cr3t-enroll-token";

    // SAFETY: single-threaded setup phase before any worker/server threads read
    // these; each `tests/*.rs` integration test is its own process so there is
    // no cross-test env contention.
    unsafe {
        std::env::set_var("AURCACHE_WORKER_PORT", port.to_string());
        std::env::set_var("AURCACHE_TLS_SANS", "localhost");
        std::env::set_var("AURCACHE_ENROLLMENT_TOKEN", token);
        std::env::remove_var("AURCACHE_ENROLLMENT_DIR");
        std::env::remove_var("AURCACHE_PREAPPROVED_WORKERS");
        // Worker config:
        std::env::set_var("AURCACHE_URL", format!("https://localhost:{port}"));
        std::env::set_var("WORKER_DATA_DIR", worker_data.display().to_string());
        std::env::set_var("WORKER_ARCHES", "x86_64");
        std::env::set_var("WORKER_NAME", "fake-worker");
    }

    // In-memory DB with the full schema, seeded with two enqueued x86_64 builds.
    let db = Database::connect("sqlite::memory:").await.unwrap();
    Migrator::up(&db, None).await.unwrap();
    seed_build(&db, 1, "x86_64", 1000).await;
    seed_build(&db, 2, "x86_64", 2000).await;

    // Boot the real worker protocol listener (HTTPS + optional mTLS).
    let ca = aurcache_ca::Ca::load_or_create(&ca_dir).unwrap();
    let _server = aurcache_api::init::init_worker_api(db.clone(), ca);

    // Wait for the TLS listener to accept and serve the CA.
    let base = format!("https://localhost:{port}");
    let mut up = false;
    for _ in 0..80 {
        if fetch_and_pin_ca(&base, None).await.is_ok() {
            up = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(up, "worker protocol listener did not come up on :{port}");

    // Enroll a synthetic worker with the production client + enrollment flow.
    // The shared enrollment token makes the server auto-approve it.
    let cfg = Config::from_env();
    let identity = Identity::load_or_create(&cfg.data_dir).unwrap();
    let client: WorkerClient = ensure_enrolled(&cfg, &identity)
        .await
        .expect("worker should enroll and be auto-approved via token");

    let claim_req = ClaimRequest {
        native_arches: vec!["x86_64".to_string()],
        emulated_arches: vec![],
    };

    // --- Happy path: claim build 1 (oldest), upload a good artifact, complete.
    let job = client
        .claim(&claim_req)
        .await
        .unwrap()
        .expect("a job should be claimable");
    assert_eq!(job.build_id, 1);
    assert_eq!(job.pkgbase, "p1");
    assert_eq!(job.arch, "x86_64");
    assert!(job.pgp_keys.is_empty(), "offline source -> no pgp keys");

    client
        .append_log(1, "fake build starting\n")
        .await
        .unwrap();

    let (fname, bytes) = make_pkg("p1", "1.0-1");
    client.upload_artifact(1, &fname, bytes).await.unwrap();
    client
        .complete(
            1,
            &CompleteReport {
                success: true,
                exit_code: Some(0),
                reason: None,
                canceled: false,
            },
        )
        .await
        .expect("complete{success} should ingest the artifact");

    assert_eq!(build_status(&db, 1).await, STATUS_SUCCESS);
    let repo_db = Path::new("repo").join("x86_64").join("repo.db.tar.gz");
    assert!(repo_db.exists(), "repo db should be written at {repo_db:?}");
    let file_rows = files::Entity::find()
        .filter(files::Column::PackageId.eq(1))
        .all(&db)
        .await
        .unwrap();
    assert!(!file_rows.is_empty(), "a files row should be recorded");

    // --- Safety rail: a wrong-named artifact is rejected at complete.
    let job2 = client
        .claim(&claim_req)
        .await
        .unwrap()
        .expect("second job claimable");
    assert_eq!(job2.build_id, 2);

    let (bad_name, bad_bytes) = make_pkg("evil", "9.9-1");
    client.upload_artifact(2, &bad_name, bad_bytes).await.unwrap();
    let rejected = client
        .complete(
            2,
            &CompleteReport {
                success: true,
                exit_code: Some(0),
                reason: None,
                canceled: false,
            },
        )
        .await;
    assert!(rejected.is_err(), "wrong pkgname must be rejected");
    // Build 2 was not ingested and remains active (still owned, not completed).
    assert_eq!(build_status(&db, 2).await, STATUS_ACTIVE);

    // --- Safety rail: a revoked worker is refused at the mTLS auth guard.
    let workers = worker_store::list_workers(&db).await.unwrap();
    let worker_id = workers.first().expect("one enrolled worker").id;
    worker_store::revoke_worker(&db, worker_id).await.unwrap();
    let after_revoke = client.claim(&claim_req).await;
    assert!(
        after_revoke.is_err(),
        "a revoked worker's cert must be rejected: {after_revoke:?}"
    );
}
