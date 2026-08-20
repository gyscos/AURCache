use aurcache_utils::job_config::mirrorlist_dir;
use chrono::Utc;
use cron::Schedule;
use pacman_mirrors::benchmark::{Bench, gen_mirrorlist};
use pacman_mirrors::platforms::Platform;
use std::env;
use std::str::FromStr;
use std::time::Duration;
use tokio::fs;
use tokio::task::JoinHandle;
use tracing::{info, warn};

pub fn start_mirror_rank_job() -> anyhow::Result<JoinHandle<()>> {
    let cron_str = env::var("MIRROR_RANK_SCHEDULE").unwrap_or_else(|_| "0 0 2 * * 1".to_string());
    // This parses the string following this spec: https://www.quartz-scheduler.org/documentation/quartz-2.3.0/tutorials/crontrigger.html
    let schedule = Schedule::from_str(cron_str.as_str())?;

    Ok(tokio::spawn(async move {
        // The scheduled job normally only runs on its cron cadence (e.g.
        // weekly), which would leave dependency resolution against the
        // official repos broken until then on a fresh install. If no
        // mirrorlist exists yet, rank one immediately so official-repo
        // lookups (used to distinguish AUR deps from `pacman`/`glibc`/etc.)
        // work right away.
        if !mirrorlist_exists() {
            info!("No mirrorlist found yet; ranking one immediately at startup");
            match update_mirrorlist().await {
                Ok(()) => info!("Initial mirror ranking finished"),
                Err(e) => warn!("Initial mirror ranking failed: {e}"),
            }
        }

        let mut upcoming = schedule.upcoming(Utc);
        loop {
            // Get the next occurrence from now
            if let Some(next_time) = upcoming.next() {
                let now = Utc::now();
                // A negative delta (clock jump) just means "run now".
                let duration = next_time
                    .signed_duration_since(now)
                    .to_std()
                    .unwrap_or(Duration::ZERO);
                info!(
                    "Waiting for scheduled mirror ranking until {} ({} seconds)",
                    next_time,
                    duration.as_secs()
                );

                // Wait until the scheduled time
                tokio::time::sleep(duration).await;

                // Execute your scheduled code
                match update_mirrorlist().await {
                    Ok(()) => {
                        info!("Mirror ranking finished");
                    }
                    Err(e) => {
                        warn!("Mirror ranking failed: {e}");
                    }
                }
            } else {
                // If there is no upcoming occurrence (unlikely with cron), wait a default duration before retrying.
                warn!("Your defined cron-job doesn't have a future schedule: '{cron_str}'");
                tokio::time::sleep(Duration::from_secs(60 * 30)).await;
            }
        }
    }))
}

async fn update_mirrorlist() -> anyhow::Result<()> {
    info!("Executing mirror ranking job at: {}", Utc::now());
    let urls = pacman_mirrors::get_status(Platform::X86_64).await?.urls;

    info!("Ranking mirrorlist");
    let mirrorlist = gen_mirrorlist(&urls.rank().await?);

    let dir = mirrorlist_dir();
    fs::create_dir_all(&dir).await?;
    let mirrorlist_path = dir.join("mirrorlist");
    fs::write(&mirrorlist_path, mirrorlist).await?;
    info!("Wrote mirrorlist to {}", mirrorlist_path.display());
    Ok(())
}

/// Returns `true` if a `mirrorlist` file is already present at the path the
/// scheduled job would write to.
fn mirrorlist_exists() -> bool {
    mirrorlist_dir().join("mirrorlist").exists()
}
