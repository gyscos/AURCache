pub mod activity_retention;
pub mod auto_update;
pub mod lease_reaper;
pub mod mirror_ranking;
pub mod official_repos;
pub mod retired_packages;
pub mod update_version_check;

use chrono::{DateTime, Local};
use std::time::Duration;
use tracing::info;

/// Sleep until the next cron fire, logging how long that is.
///
/// Schedules are read in the server's local timezone (`TZ`, else
/// `/etc/localtime`), so `0 0 3 * * *` means 3 am where the server is, which
/// is what someone writing it expects. The settings page says which timezone
/// that is.
///
/// A negative delta (clock jump) just means "run now".
pub(crate) async fn sleep_until_next_fire(
    upcoming: &mut impl Iterator<Item = DateTime<Local>>,
    what: &str,
) -> Wake {
    match upcoming.next() {
        Some(next_time) => {
            let duration = next_time
                .signed_duration_since(Local::now())
                .to_std()
                .unwrap_or(Duration::ZERO);
            info!(
                "Waiting for scheduled {what} until {} ({} seconds)",
                next_time,
                duration.as_secs()
            );
            tokio::time::sleep(duration).await;
            Wake::Fired
        }
        None => Wake::Exhausted,
    }
}

/// Why [`sleep_until_next_fire`] returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Wake {
    /// The schedule's next occurrence is now: run the job.
    Fired,
    /// The schedule has no upcoming occurrence, so there was nothing to sleep
    /// until.
    Exhausted,
}
