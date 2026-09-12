//! Lease reaper: a periodic scheduler job that reclaims build jobs whose remote
//! worker went silent, and backstops builds that have run implausibly long.
//!
//! Liveness model (see design doc §"Build liveness & failure handling"): each
//! `ACTIVE` build carries a `lease_expires_at` that a worker renews via
//! heartbeats. When a worker crashes/partitions, the lease lapses and this job
//! abandons the build: the row is written terminal `FAILED` (naming the worker
//! that ran it) and a *fresh* build is queued while the derived retry budget
//! allows — never a requeue of the same row. Explicit failures are handled
//! synchronously in the `complete` endpoint and are never seen here.

use aurcache_common::build_state::EndReasons;
use aurcache_common::settings::{ApplicationSettings, Setting, SettingsEntry};
use aurcache_db::helpers::time::now_secs;
use aurcache_db::helpers::worker_jobs::reap_expired_builds;
use aurcache_db::prelude::{Builds, Packages};
use aurcache_db::{builds, packages};
use aurcache_utils::build_logger::append_build_output;
use aurcache_utils::settings::general::SettingsTraits;
use sea_orm::{
    ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QuerySelect,
};
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
                    if !out.retried.is_empty() || !out.failed.is_empty() {
                        warn!(
                            "Lease reaper: retried {:?}, gave up on {:?} (silent/hung workers)",
                            out.retried, out.failed
                        );
                    }

                    // Explain each abandoned build in its own log. The row is
                    // terminal, so this is the last line it ever gets.
                    let mut ids: Vec<i32> = out.retried.iter().map(|(old, _)| *old).collect();
                    ids.extend(out.failed.iter().copied());
                    for id in ids {
                        explain_abandoned(&db, id).await;
                    }
                }
                Err(e) => warn!("Lease reaper pass failed: {e}"),
            }
        }
    })
}

/// Append why build `id` was abandoned to its log file. Best-effort by design:
/// the row is already terminal, so nothing downstream depends on this
/// succeeding, and a deleted package simply yields no line.
async fn explain_abandoned(db: &DatabaseConnection, id: i32) {
    // The pkgbase and the build number together locate the log file; the row's
    // own number is always the public identity of the attempt.
    let pkgbase: Option<String> = Packages::find()
        .select_only()
        .column(packages::Column::Name)
        .join(
            sea_orm::JoinType::InnerJoin,
            packages::Entity::belongs_to(builds::Entity)
                .from(packages::Column::Id)
                .to(builds::Column::PkgId)
                .into(),
        )
        .filter(builds::Column::Id.eq(id))
        .into_tuple::<String>()
        .one(db)
        .await
        .ok()
        .flatten();
    let Some(pkgbase) = pkgbase else {
        return;
    };

    let Ok(Some(build)) = Builds::find_by_id(id).one(db).await else {
        return;
    };
    let reason = match build.end_reason {
        Some(EndReasons::MAX_DURATION) => "build exceeded its maximum duration",
        Some(EndReasons::LEASE_EXPIRED) => "owner worker stopped heartbeating",
        // Worker-reported terminal outcomes (OOM, timeout, nonzero exit) come
        // through the `complete` path, never here; treat anything unexpected as
        // an abandonment anyway so the log still says something truthful.
        _ => "abandoned by the lease reaper",
    };
    let text = format!("Timeout: {reason}.\n");
    if let Err(e) = append_build_output(&pkgbase, build.number, &text).await {
        warn!("could not append abandonment log for build {id}: {e}");
    }
}