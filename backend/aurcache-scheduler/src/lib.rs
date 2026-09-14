pub mod auto_update;
pub mod download_flush;
pub mod lease_reaper;
pub mod mirror_ranking;
pub mod official_repos;
pub mod retired_packages;
pub mod update_version_check;

use chrono::{DateTime, Utc};
use std::time::Duration;
use tracing::info;

/// Sleep until the next cron fire, logging how long that is.
///
/// Returns `false` when the schedule has no upcoming occurrence. A negative
/// delta (clock jump) just means "run now".
pub(crate) async fn sleep_until_next_fire(
    upcoming: &mut impl Iterator<Item = DateTime<Utc>>,
    what: &str,
) -> bool {
    match upcoming.next() {
        Some(next_time) => {
            let duration = next_time
                .signed_duration_since(Utc::now())
                .to_std()
                .unwrap_or(Duration::ZERO);
            info!(
                "Waiting for scheduled {what} until {} ({} seconds)",
                next_time,
                duration.as_secs()
            );
            tokio::time::sleep(duration).await;
            true
        }
        None => false,
    }
}
