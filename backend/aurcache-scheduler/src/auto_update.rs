use crate::sleep_until_next_fire;
use aurcache_common::settings::{ApplicationSettings, Setting, SettingsEntry};
use aurcache_utils::package::update::package_update_all_outdated;
use aurcache_utils::services::Services;
use aurcache_utils::settings::general::SettingsTraits;
use chrono::Utc;
use cron::Schedule;
use std::str::FromStr;
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::{info, warn};

#[must_use]
pub fn start_auto_update_job(services: Services) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            // check everytime in loop since it may change per user setting
            let interval: SettingsEntry<Option<String>> =
                ApplicationSettings::get(Setting::AutoUpdateInterval, None, &services.db).await;
            match interval.value.as_deref().map(Schedule::from_str) {
                None => {
                    // Auto update disabled
                    tokio::time::sleep(Duration::from_hours(1)).await;
                }
                Some(Err(e)) => {
                    warn!("Invalid cron expression: {e} -- Retry in 15min");
                    tokio::time::sleep(Duration::from_mins(15)).await;
                }
                Some(Ok(schedule)) => {
                    let mut upcoming = schedule.upcoming(Utc);

                    if sleep_until_next_fire(&mut upcoming, "update").await {
                        info!("Executing scheduled job at: {}", Utc::now());
                        if let Err(e) = package_update_all_outdated(&services).await {
                            warn!("Failed to trigger update of all outdated packages: {e}");
                        }
                    } else {
                        warn!("Your defined cron-job doesn't have a future schedule");
                        tokio::time::sleep(Duration::from_mins(30)).await;
                    }
                }
            }
        }
    })
}
