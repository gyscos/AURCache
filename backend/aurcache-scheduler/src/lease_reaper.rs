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

use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_activitylog::failure_activity::WorkerReapedActivity;
use aurcache_common::build_state::EndReasons;
use aurcache_common::settings::{ApplicationSettings, Setting, SettingsEntry};
use aurcache_db::activities::ActivityType;
use aurcache_db::helpers::time::now_secs;
use aurcache_db::helpers::worker_jobs::{Abandoned, reap_expired_builds};
use aurcache_utils::build_logger::append_build_output;
use aurcache_utils::settings::general::SettingsTraits;
use sea_orm::DatabaseConnection;
use std::env;
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::{info, warn};

fn env_i64(key: &str, default: i64) -> i64 {
    env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

/// Spawn the reaper loop. Runs every `REAP_INTERVAL` seconds (default 20s).
pub fn start_lease_reaper(db: DatabaseConnection, activity: ActivityLog) -> JoinHandle<()> {
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
                        // One entry for the pass, not one per build: the reaper
                        // finds them together and they have one cause.
                        activity.record(
                            WorkerReapedActivity {
                                // The build the operator was watching, not
                                // the fresh row standing in for it: the
                                // replacement is a number they have never
                                // seen.
                                retried: out
                                    .retried
                                    .iter()
                                    .map(|&(abandoned, _replacement)| abandoned)
                                    .collect(),
                                failed: out.failed.clone(),
                            },
                            ActivityType::WorkerReaped,
                            None,
                        );
                    }

                    // Explain each abandoned build in its own log. The row is
                    // terminal, so this is the last line it ever gets.
                    for abandoned in &out.abandoned {
                        explain_abandoned(abandoned).await;
                    }
                }
                Err(e) => warn!("Lease reaper pass failed: {e}"),
            }
        }
    })
}

/// Append why a build was abandoned to its log file. Best-effort by design:
/// the row is already terminal, so nothing downstream depends on this
/// succeeding, and a deleted package simply yields no line.
async fn explain_abandoned(abandoned: &Abandoned) {
    let Some(pkgbase) = &abandoned.pkgbase else {
        return;
    };
    let reason = match abandoned.end_reason {
        EndReasons::MAX_DURATION => "build exceeded its maximum duration",
        EndReasons::LEASE_EXPIRED => "owner worker stopped heartbeating",
        // The reaper records only the two above; anything else still reads as
        // an abandonment, so the log says something truthful.
        _ => "abandoned by the lease reaper",
    };
    let text = format!("Timeout: {reason}.\n");
    if let Err(e) = append_build_output(pkgbase, abandoned.number, &text).await {
        warn!(
            "could not append abandonment log for build {}: {e}",
            abandoned.build_id
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Env values with surrounding whitespace still parse: env files and
    /// container runtimes add it more often than anyone admits, and without
    /// the trim the value silently falls back to the default.
    #[test]
    fn env_i64_tolerates_surrounding_whitespace() {
        // Unique to this test: nothing else in the process reads it, so the
        // set/remove cannot race a parallel test.
        let key = "AURCACHE_TEST_TRIM_PROBE_LEASE_REAPER";
        // SAFETY: the key is unique to this test (see above).
        unsafe {
            std::env::set_var(key, "  42\t");
        }
        assert_eq!(env_i64(key, 3), 42);
        // SAFETY: same key, same reasoning.
        unsafe {
            std::env::remove_var(key);
        }
        assert_eq!(env_i64(key, 3), 3);
    }
}
