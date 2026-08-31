//! Lease reaper: a periodic scheduler job that reclaims build jobs whose remote
//! worker went silent, and backstops builds that have run implausibly long.
//!
//! Liveness model (see design doc §"Build liveness & failure handling"): each
//! `ACTIVE` build carries a `lease_expires_at` that a worker renews via
//! heartbeats. When a worker crashes/partitions, the lease lapses and this job
//! requeues the build (bounded by `attempt_count`) or terminally fails it once
//! the retry budget is exhausted. Explicit failures are handled synchronously in
//! the `complete` endpoint and are never seen here.

use aurcache_common::settings::{ApplicationSettings, Setting, SettingsEntry};
use aurcache_db::helpers::time::now_secs;
use aurcache_db::helpers::worker_jobs::reap_expired_builds;
use aurcache_utils::settings::general::SettingsTraits;
use sea_orm::DatabaseConnection;
use std::env;
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::{info, warn};

fn env_i64(key: &str, default: i64) -> i64 {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Spawn the reaper loop. Runs every `REAP_INTERVAL` seconds (default 20s).
pub fn start_lease_reaper(db: DatabaseConnection) -> JoinHandle<()> {
    let interval = Duration::from_secs(env_i64("REAP_INTERVAL", 20).max(1) as u64);
    let max_attempts = env_i64("MAX_ATTEMPTS", 3) as i32;
    // Backstop grace beyond a build's own timeout before we forcibly reclaim a
    // still-heartbeating but hung build.
    let grace = env_i64("REAP_BACKSTOP_GRACE", 300);

    tokio::spawn(async move {
        info!(
            "Lease reaper started (every {}s, max_attempts={max_attempts})",
            interval.as_secs()
        );
        loop {
            tokio::time::sleep(interval).await;

            // `MAX_BUILD_DURATION` reuses the configurable JobTimeout setting.
            let job_timeout: SettingsEntry<u32> =
                ApplicationSettings::get(Setting::JobTimeout, None, &db).await;
            let max_build_age = i64::from(job_timeout.value) + grace;

            match reap_expired_builds(&db, now_secs(), max_attempts, max_build_age).await {
                Ok(out) => {
                    if !out.requeued.is_empty() || !out.failed.is_empty() {
                        warn!(
                            "Lease reaper: requeued {:?}, gave up on {:?} (silent/hung workers)",
                            out.requeued, out.failed
                        );
                    }
                }
                Err(e) => warn!("Lease reaper pass failed: {e}"),
            }
        }
    })
}
