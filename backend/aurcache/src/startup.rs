use std::env;
use std::path::PathBuf;
use tokio::fs;

use aurcache_db::prelude::{Builds, Packages};
use aurcache_db::{builds, packages};
use aurcache_types::builder::BuildStates;
use aurcache_utils::job_config::mirrorlist_dir;
use pacman_mirrors::benchmark::Bench;
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

pub async fn pre_startup_tasks() {
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
            &PathBuf::from(format!("./repo/{platform}")),
            "repo",
        ) {
            error!("Failed to initialize pacman repo: {e:?}");
        }
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
                .is_in(vec![BuildStates::ACTIVE_BUILD, BuildStates::ENQUEUED_BUILD]),
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
            builds::Column::Status
                .is_in(vec![BuildStates::ACTIVE_BUILD, BuildStates::ENQUEUED_BUILD]),
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
    let mirrorlist_path = mirrorlist_dir.display().to_string();
    let mirrorlist_file = mirrorlist_dir.join("mirrorlist");

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
            .collect::<Vec<_>>()
            .join("");
        fs::write(&mirrorlist_file, mirrorlist).await?;
        info!("Wrote mirrorlist to {mirrorlist_path}");
    } else if std::fs::metadata(&mirrorlist_file).is_err() {
        info!("Perform initial load of pacman mirrorlist");
        match pacman_mirrors::get_status(Platform::X86_64).await {
            Ok(status) => {
                let urls = status.urls;
                let mirrorlist = urls.gen_mirrorlist(urls.0.clone())?;
                fs::write(&mirrorlist_file, mirrorlist).await?;
                info!("Wrote mirrorlist to {mirrorlist_path}");
            }
            Err(e) => {
                warn!("Failed to get mirror list: {e}");
            }
        }
    }

    Ok(())
}
