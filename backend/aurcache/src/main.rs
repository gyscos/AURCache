use crate::logger::init_logger;
use crate::startup::{post_startup_tasks, pre_startup_tasks};
use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_activitylog::server_start_activity::ServerStartActivity;
use aurcache_api::init::{CaDirectory, ServerVersion, init_api, init_repo, init_worker_api};
use aurcache_builder::init::init_build_queue;
use aurcache_db::action::Action;
use aurcache_db::activities::ActivityType;
use aurcache_db::helpers::downloads::DownloadCounter;
use aurcache_db::init::init_db;
use aurcache_deps::AurClient;
use aurcache_scheduler::activity_retention::start_activity_retention;
use aurcache_scheduler::auto_update::start_auto_update_job;
use aurcache_scheduler::download_flush::start_download_flush;
use aurcache_scheduler::lease_reaper::start_lease_reaper;
use aurcache_scheduler::mirror_ranking::start_mirror_rank_job;
use aurcache_scheduler::official_repos::start_official_repo_refresh;
use aurcache_scheduler::retired_packages::start_retired_package_sweep;
use aurcache_scheduler::update_version_check::start_update_version_checking;
use aurcache_utils::repository::{REPO_ROOT, Repository};
use aurcache_utils::services::Services;
use aurcache_utils::snapshot::SnapshotStore;
use dotenvy::dotenv;
use std::env;
use std::sync::Arc;
use tokio::sync::broadcast;
use tracing::warn;

mod logger;
mod startup;

#[tokio::main]
async fn main() {
    _ = dotenv();
    init_logger();
    pre_startup_tasks();

    let (tx, _) = broadcast::channel::<Action>(32);
    let db = init_db().await.expect("failed to initialize database");

    if let Err(e) = post_startup_tasks(&db).await {
        warn!("Startup cleanup did not complete: {e}");
    }

    // A line in the log for the process starting. It says which version came
    // up, which is what lines a deploy up against whatever happened after it --
    // and it is the marker a "since this boot" view would count back to.
    //
    // Best effort: a server that cannot write its own start entry should still
    // start.
    if let Err(e) = ActivityLog::new(db.clone())
        .add(
            ServerStartActivity {
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
            ActivityType::ServerStart,
            None,
        )
        .await
    {
        warn!("Could not record the server start in the activity log: {e}");
    }

    // Load (or create on first run) the internal CA used to authenticate remote
    // build workers over mutual TLS. Persisted under the data directory.
    let ca_dir = std::path::PathBuf::from(
        env::var("AURCACHE_CA_DIR").unwrap_or_else(|_| "./data/ca".to_string()),
    );
    let ca = aurcache_ca::Ca::load_or_create(&ca_dir).expect("failed to initialize internal CA");
    if let Ok(fp) = ca.ca_cert_fingerprint() {
        tracing::info!("Worker CA fingerprint (pin this on workers): {fp}");
    }

    // A single, long-lived `SnapshotStore` shared by every path that resolves
    // package sources: the version-check loop, the auto-update job, the API/UI
    // listener (source browsing + patch editing), and the worker protocol
    // listener (job descriptors built during `claim`). Its persistent on-disk
    // git checkouts and `refresh()` incremental-fetch model make this safe:
    // repeat requests reuse the same checkout instead of re-cloning/
    // re-downloading. Handing any of these its own instance would put two
    // stores on the same checkout directories with no shared locking.
    // The database comes along so the store can read `parse_network` when it
    // parses a PKGBUILD; without it every parse would use the default.
    let store = Arc::new(SnapshotStore::new().with_db(db.clone()));

    // One AUR client for the whole server, for the same reason as the store
    // above: it owns the cached official repository databases and the names
    // read from them, and a second instance would fetch the same databases
    // again and hold a second answer to the same question.
    //
    // It starts with nothing read. The refresh job below fills it in and keeps
    // it current; until the first pass succeeds, dependency resolution says so
    // rather than reporting an empty repository -- so the API and the UI come
    // up either way and can report why an add failed.
    let client = Arc::new(AurClient::new());
    let official_repo_handle = start_official_repo_refresh(Arc::clone(&client));

    // The pacman repository. One instance, because its lock is what keeps two
    // changes to it from interleaving: the worker protocol publishing builds,
    // package removals and restores all go through this one.
    let repo = Arc::new(Repository::new(REPO_ROOT));

    // The things a package operation acts through, from here on passed as one.
    // Cloning is a few refcount bumps, so a job or a request handler takes its
    // own handle on the same instances.
    let services = Services::new(
        db.clone(),
        tx.clone(),
        Arc::clone(&store),
        client,
        Arc::clone(&repo),
    );

    // Builds a restart caught mid-publish pick up where they were.
    startup::resume_publishing(&db, &repo).await;

    // Before anything else can resolve a source: a prune cannot distinguish a
    // clone in flight from a stranded one.
    startup::prune_source_checkouts(&db, &store).await;

    let build_queue_handle = init_build_queue(db.clone(), tx.clone());
    let version_check_handle = start_update_version_checking(services.clone());
    let auto_update_handle = start_auto_update_job(services.clone());

    let mirrorlist_override =
        env::var("MIRRORLIST_SERVERS_X86_64").is_ok_and(|s| !s.trim().is_empty());

    if !mirrorlist_override && let Err(e) = start_mirror_rank_job() {
        warn!("mirror_rank job not properly configured: {e}");
    }

    // Reclaim build jobs whose remote worker went silent (lease liveness).
    let lease_reaper_handle = start_lease_reaper(db.clone());

    // Delete package files the repository stopped listing, once clients with
    // an older repo.db have had time to fetch them.
    let retired_sweep_handle = start_retired_package_sweep(Arc::clone(&repo));

    // Keep the log from growing for ever. Nothing has ever pruned it, and it
    // now records restarts and failures as well as what people did.
    let activity_retention_handle = start_activity_retention(db.clone());

    // Repository downloads are counted in memory by the file server and folded
    // into the database from here, so serving a package costs no write. Both
    // sides share this one buffer; a second instance would count into a map
    // nothing flushes.
    let downloads = Arc::new(DownloadCounter::new());
    let download_flush_handle = start_download_flush(db.clone(), Arc::clone(&downloads));

    let api_handle = init_api(
        services.clone(),
        Arc::clone(&downloads),
        ServerVersion(env!("CARGO_PKG_VERSION").to_string()),
        CaDirectory(ca_dir.clone()),
    );
    let worker_api_handle = init_worker_api(db, ca, store, Arc::clone(&repo));
    let repo_handle = init_repo(downloads, repo);

    tokio::select! {
        _ = version_check_handle => {
            warn!("Version check handle exited");
        }
        _ = auto_update_handle => {
            warn!("Auto update handle exited");
        }
        _ = build_queue_handle => {
            warn!("Build queue handle exited");
        }
        _ = lease_reaper_handle => {
            warn!("Lease reaper handle exited");
        }
        _ = retired_sweep_handle => {
            warn!("Retired package sweep handle exited");
        }
        _ = activity_retention_handle => {
            warn!("Activity log retention handle exited");
        }
        _ = official_repo_handle => {
            warn!("Official repository refresh handle exited");
        }
        _ = download_flush_handle => {
            warn!("Download flush handle exited");
        }
        _ = repo_handle => {
            warn!("Repo web server handle exited");
        }
        _ = api_handle => {
            warn!("API web server handle exited");
        }
        _ = worker_api_handle => {
            warn!("Worker protocol listener exited");
        }
    }
}
