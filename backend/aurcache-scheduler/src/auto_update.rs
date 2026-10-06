use crate::{Wake, sleep_until_next_fire};
use aurcache_activitylog::events::Event;
use aurcache_common::settings::{ApplicationSettings, Setting, SettingsEntry};
use aurcache_utils::package::update::package_update_all_outdated;
use aurcache_utils::scheduled::{Job, schedule};
use aurcache_utils::services::Services;
use aurcache_utils::settings::general::SettingsTraits;
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::info;

#[must_use]
pub fn start_auto_update_job(services: Services) -> JoinHandle<()> {
    tokio::spawn(async move {
        // What the log was last told about the schedule. Re-checked every
        // quarter hour, and recording the same complaint each time would bury
        // everything else; a schedule that is fixed and broken again says so.
        let mut reported: Option<String> = None;
        let mut report = |error: String| {
            if reported.as_deref() != Some(error.as_str()) {
                services.activity.emit(Event::ScheduleInvalid {
                    job: "auto_update".to_string(),
                    error: error.clone(),
                });
                reported = Some(error);
            }
        };
        loop {
            // check everytime in loop since it may change per user setting
            let interval: SettingsEntry<Option<String>> =
                ApplicationSettings::get(Setting::AutoUpdateInterval, None, &services.db).await;
            let expr = interval.value.filter(|expr| !expr.trim().is_empty());
            match expr.as_deref().map(|expr| schedule(Job::AutoUpdate, expr)) {
                None => {
                    // Auto update disabled
                    tokio::time::sleep(Duration::from_hours(1)).await;
                }
                Some(Err(e)) => {
                    report(e.to_string());
                    tokio::time::sleep(Duration::from_mins(15)).await;
                }
                Some(Ok(schedule)) => {
                    if sleep_until_next_fire(&schedule, "update").await == Wake::Fired {
                        info!("Executing scheduled auto-update");
                        if let Err(e) = package_update_all_outdated(&services).await {
                            services.activity.emit(Event::UpdateQueueFailed {
                                error: format!("{e:#}"),
                            });
                        }
                    } else {
                        report("the schedule never fires again".to_string());
                        tokio::time::sleep(Duration::from_mins(30)).await;
                    }
                }
            }
        }
    })
}
