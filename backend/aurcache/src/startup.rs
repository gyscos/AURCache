use std::env;
use std::path::Path;
use tokio::fs;

use aurcache_db::prelude::{Builds, Packages};
use aurcache_db::{builds, packages};
use aurcache_types::builder::BuildStates;
use aurcache_utils::job_config::{self, mirrorlist_dir, native_arch, shared_mirrorlist_path};
use pacman_mirrors::benchmark::gen_mirrorlist;
use pacman_mirrors::platforms::{Platform, Platforms};
use sea_orm::QueryFilter;
use sea_orm::prelude::Expr;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait};
use tracing::{error, info, warn};

const START_BANNER: &str = r"
          _    _ _____   _____           _
     /\  | |  | |  __ \ / ____|         | |
    /  \ | |  | | |__) | |     __ _  ___| |__   ___
   / /\ \| |  | |  _  /| |    / _` |/ __| '_ \ / _ \
  / ____ \ |__| | | \ \| |___| (_| | (__| | | |  __/
 /_/    \_\____/|_|  \_\\_____\__,_|\___|_| |_|\___|
";

pub fn pre_startup_tasks() {
    info!("{START_BANNER}");
    let latest_commit_sha = option_env!("LATEST_COMMIT_SHA").unwrap_or("dev");
    info!(
        "Version: {}#{}",
        env!("CARGO_PKG_VERSION"),
        latest_commit_sha
    );

    #[cfg(debug_assertions)]
    warn!("This is a dev build! Consider using a stable release.");

    for platform in Platforms {
        if let Err(e) = pacman_repo_utils::repo_init::init_repo(
            Path::new(&format!("./repo/{platform}")),
            "repo",
        ) {
            error!("Failed to initialize pacman repo: {e:?}");
        }
    }
}

/// The one architecture AURCache can rank mirrors for itself.
const RANKABLE_ARCH: &str = "x86_64";

/// Copy a mounted `mirrorlist` into this host's `mirrorlist.<arch>` slot.
///
/// Mirrorlists are stored per-architecture, but mounting a single file is both
/// the convenient thing to do and what deployments predating the arch-keyed
/// layout already have. Adopting at boot means every reader deals only with the
/// arch-keyed layout instead of each carrying a fallback.
///
/// The copy is unconditional, so editing the mounted file takes effect on the
/// next boot with no staleness bookkeeping to get wrong. Re-copying identical
/// bytes costs nothing at this size, which is a better trade than tracking
/// provenance hashes to avoid it.
///
/// Safe to clobber the target because mirror ranking stands down whenever a
/// mount owns that architecture's slot (see `aurcache_scheduler::mirror_ranking`),
/// so nothing else writes the file this would overwrite.
///
/// Best-effort — failure just leaves the mirrorlist unpopulated, which every
/// caller already tolerates.
async fn adopt_shared_mirrorlist() {
    let shared = shared_mirrorlist_path();
    if !fs::try_exists(&shared).await.unwrap_or(false) {
        return;
    }
    let target = job_config::mirrorlist_path(native_arch());
    match fs::copy(&shared, &target).await {
        Ok(_) => info!(
            "Adopted mounted mirrorlist {} as {}",
            shared.display(),
            target.display()
        ),
        Err(e) => warn!(
            "Failed to adopt mounted mirrorlist {}: {e}",
            shared.display()
        ),
    }
}

pub async fn post_startup_tasks(db: &DatabaseConnection) -> anyhow::Result<()> {
    // set all pending package status to failed
    Packages::update_many()
        .col_expr(
            packages::Column::Status,
            Expr::value(BuildStates::FAILED_BUILD),
        )
        .filter(
            packages::Column::Status
                .is_in([BuildStates::ACTIVE_BUILD, BuildStates::ENQUEUED_BUILD]),
        )
        .exec(db)
        .await?;

    // set all pending or failed package status to failed
    Builds::update_many()
        .col_expr(
            builds::Column::Status,
            Expr::value(BuildStates::FAILED_BUILD),
        )
        .filter(
            builds::Column::Status.is_in([BuildStates::ACTIVE_BUILD, BuildStates::ENQUEUED_BUILD]),
        )
        .exec(db)
        .await?;

    // todo arm mirrorlists unsupported for now!
    let mirrorlist_dir = mirrorlist_dir();
    if let Err(e) = fs::create_dir_all(&mirrorlist_dir).await {
        warn!(
            "Failed to create mirrorlist dir {}: {e}",
            mirrorlist_dir.display()
        );
    }
    // A single mounted `mirrorlist` is the convenient thing for an operator to
    // provide, and is what every deployment predating the arch-keyed layout
    // already has. Adopt it into this host's slot so every reader can assume
    // `mirrorlist.<arch>` and no fallback logic is needed anywhere else.
    adopt_shared_mirrorlist().await;

    // Mirror ranking only knows how to rank x86_64 mirrors, so that is the one
    // architecture AURCache can populate itself.
    let mirrorlist_file = job_config::mirrorlist_path(RANKABLE_ARCH);
    let mirrorlist_path = mirrorlist_file.display().to_string();

    // Check if mirrorlist servers are provided via env var (semicolon-separated)
    // Treat an empty var the same way as an unset var.
    if let Ok(servers) = env::var("MIRRORLIST_SERVERS_X86_64")
        && !servers.trim().is_empty()
    {
        info!("Using mirrorlist from MIRRORLIST_SERVERS_X86_64 env var");
        let mirrorlist = servers
            .split(';')
            .filter(|s| !s.is_empty())
            .map(|s| format!("Server = {s}\n"))
            .collect::<String>();
        fs::write(&mirrorlist_file, mirrorlist).await?;
        info!("Wrote mirrorlist to {mirrorlist_path}");
    } else if !fs::try_exists(&mirrorlist_file).await.unwrap_or(false) {
        info!("Perform initial load of pacman mirrorlist");
        match pacman_mirrors::get_status(Platform::X86_64).await {
            Ok(status) => {
                fs::write(&mirrorlist_file, gen_mirrorlist(&status.urls.0)).await?;
                info!("Wrote mirrorlist to {mirrorlist_path}");
            }
            Err(e) => {
                warn!("Failed to get mirror list: {e}");
            }
        }
    }

    Ok(())
}
