use aurcache_utils::job_config::{
    mirrorlist_dir, mirrorlist_path, native_arch, shared_mirrorlist_path,
};
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

/// The architecture `pacman_mirrors` can rank for; mirrorlists are per-arch.
const RANKED_ARCH: &str = "x86_64";

/// Whether a mounted mirrorlist owns the slot ranking would write.
///
/// A mounted `mirrorlist` is adopted into the *native* architecture's slot at
/// startup, and ranking writes [`RANKED_ARCH`]. They are the same file only on
/// an x86_64 host, which is the sole case where the two writers would fight —
/// ranking overwriting the operator's mirrors, startup copying them back on the
/// next boot, forever.
///
/// Pure so the precedence rule is testable without a filesystem.
fn mount_owns_ranked_slot(shared_mirrorlist_exists: bool, native_arch: &str) -> bool {
    shared_mirrorlist_exists && native_arch == RANKED_ARCH
}

/// Rank mirrors and write them to [`RANKED_ARCH`]'s mirrorlist.
///
/// Stands down when the operator mounted their own mirrorlist for this
/// architecture: AURCache cannot currently rank a supplied list (ranking starts
/// from Arch's mirror-status API and needs per-mirror metadata a mirrorlist file
/// does not carry), so the only coherent options are "use their mirrors" or
/// "replace them", and an explicit mount is the stronger signal.
async fn update_mirrorlist() -> anyhow::Result<()> {
    if mount_owns_ranked_slot(shared_mirrorlist_path().exists(), native_arch()) {
        info!(
            "Skipping mirror ranking: {} is provided and adopted as the {RANKED_ARCH} mirrorlist",
            shared_mirrorlist_path().display()
        );
        return Ok(());
    }

    info!("Executing mirror ranking job at: {}", Utc::now());
    let urls = pacman_mirrors::get_status(Platform::X86_64).await?.urls;

    info!("Ranking mirrorlist");
    let mirrorlist = gen_mirrorlist(&urls.rank().await?);

    fs::create_dir_all(mirrorlist_dir()).await?;
    // Ranking is x86_64-only (see `Platform::X86_64` above), so this writes the
    // x86_64 slot specifically rather than the host's native one.
    let target = mirrorlist_path(RANKED_ARCH);
    fs::write(&target, mirrorlist).await?;
    info!("Wrote mirrorlist to {}", target.display());
    Ok(())
}

/// Returns `true` if a `mirrorlist` file is already present at the path the
/// scheduled job would write to.
fn mirrorlist_exists() -> bool {
    mirrorlist_path(RANKED_ARCH).exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// On x86_64 the mounted list and the ranked list are the same file, so
    /// ranking must stand down or the two writers overwrite each other forever.
    #[test]
    fn a_mounted_mirrorlist_stops_ranking_on_the_native_arch() {
        assert!(mount_owns_ranked_slot(true, "x86_64"));
    }

    /// On any other host the mount lands in a different arch's slot, so ranking
    /// x86_64 clobbers nothing and should still run.
    #[test]
    fn a_mount_for_another_arch_does_not_stop_ranking() {
        assert!(!mount_owns_ranked_slot(true, "aarch64"));
        assert!(!mount_owns_ranked_slot(true, "armv7h"));
    }

    #[test]
    fn ranking_runs_when_nothing_is_mounted() {
        assert!(!mount_owns_ranked_slot(false, "x86_64"));
        assert!(!mount_owns_ranked_slot(false, "aarch64"));
    }
}
