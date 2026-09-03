use std::collections::HashMap;
use std::env;
use std::path::Path;
use tokio::fs;

use aurcache_common::builder::BuildStates;
use aurcache_db::helpers::operations;
use aurcache_db::prelude::{Builds, Files, Packages};
use aurcache_db::{builds, files, packages};
use aurcache_utils::job_config::{self, mirrorlist_dir, native_arch, shared_mirrorlist_path};
use pacman_mirrors::benchmark::gen_mirrorlist;
use pacman_mirrors::platforms::{Platform, Platforms};
use sea_orm::prelude::Expr;
use sea_orm::{ActiveModelTrait, ActiveValue::Set, ColumnTrait, DatabaseConnection, EntityTrait};
use sea_orm::{QueryFilter, QueryOrder};
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
        let repo_dir = Path::new("./repo").join(platform.to_string());
        if let Err(e) = pacman_repo_utils::repo_init::init_repo(&repo_dir, "repo") {
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

    // Fail builds that were mid-flight when the server stopped. Waiting-for-deps
    // builds are deliberately left alone: nothing was running, and the
    // dependency that promotes them may still complete.
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

    backfill_file_sizes(db).await;
    backfill_build_sizes(db).await;
    close_orphaned_operations(db).await;

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
        info!("Wrote mirrorlist to {}", mirrorlist_file.display());
    } else if !fs::try_exists(&mirrorlist_file).await.unwrap_or(false) {
        info!("Perform initial load of pacman mirrorlist");
        match pacman_mirrors::get_status(Platform::X86_64).await {
            Ok(status) => {
                fs::write(&mirrorlist_file, gen_mirrorlist(&status.urls.0)).await?;
                info!("Wrote mirrorlist to {}", mirrorlist_file.display());
            }
            Err(e) => {
                warn!("Failed to get mirror list: {e}");
            }
        }
    }

    Ok(())
}

/// Fill in `files.size` for rows that predate the column.
///
/// `repo_ingest` records the size from the bytes it already holds, so this only
/// covers rows written before that existed. Best-effort throughout: a file that
/// is gone stays `NULL` and the page reports its size as unknown, which beats
/// failing startup over a display field. Idempotent, so it also repairs a row
/// whose file was replaced out of band.
async fn backfill_file_sizes(db: &DatabaseConnection) {
    let rows = match Files::find()
        .filter(files::Column::Size.is_null())
        .all(db)
        .await
    {
        Ok(rows) => rows,
        Err(e) => {
            warn!("could not look up files needing a size backfill: {e}");
            return;
        }
    };
    if rows.is_empty() {
        return;
    }

    info!("Backfilling size for {} package files", rows.len());
    let mut filled = 0;
    for row in rows {
        let path = Path::new("./repo")
            .join(row.platform.to_string())
            .join(&row.filename);
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        let size = i64::try_from(meta.len()).unwrap_or(i64::MAX);
        let active = files::ActiveModel {
            id: Set(row.id),
            size: Set(Some(size)),
            ..Default::default()
        };
        match active.update(db).await {
            Ok(_) => filled += 1,
            Err(e) => warn!("could not record size for {}: {e}", row.filename),
        }
    }
    info!("Recorded size for {filled} package files");
}

/// Fill in `builds.size` for the newest successful build of each package and
/// platform, from the artifacts currently in the repository.
///
/// Runs after [`backfill_file_sizes`], which is where those sizes come from.
/// Only the newest successful build can be recovered: it is the one whose
/// output is still on disk, and older builds' artifacts were replaced by it, so
/// they keep a `NULL` that honestly says the size is not known rather than a
/// number borrowed from a different build.
///
/// A group with any unrecorded file size is skipped entirely, matching what the
/// package page and list do with a partial total.
async fn backfill_build_sizes(db: &DatabaseConnection) {
    let rows = match Files::find().all(db).await {
        Ok(rows) => rows,
        Err(e) => {
            warn!("could not load files for the build-size backfill: {e}");
            return;
        }
    };

    let mut totals: HashMap<(i32, Platform), Option<i64>> = HashMap::new();
    for row in rows {
        let entry = totals
            .entry((row.package_id, row.platform))
            .or_insert(Some(0));
        *entry = entry.and_then(|acc| row.size.map(|size| acc + size));
    }

    let mut filled = 0;
    for ((pkg_id, platform), total) in totals {
        let Some(total) = total else { continue };
        let newest = Builds::find()
            .filter(builds::Column::PkgId.eq(pkg_id))
            .filter(builds::Column::Platform.eq(platform))
            .filter(builds::Column::Status.eq(BuildStates::SUCCESSFUL_BUILD))
            .filter(builds::Column::Size.is_null())
            .order_by_desc(builds::Column::Number)
            .one(db)
            .await;
        let build = match newest {
            Ok(Some(build)) => build,
            Ok(None) => continue,
            Err(e) => {
                warn!("could not find a build to record a size for: {e}");
                continue;
            }
        };
        let active = builds::ActiveModel {
            id: Set(build.id),
            size: Set(Some(total)),
            ..Default::default()
        };
        match active.update(db).await {
            Ok(_) => filled += 1,
            Err(e) => warn!("could not record size for build {}: {e}", build.id),
        }
    }
    if filled > 0 {
        info!("Recorded output size for {filled} builds");
    }
}

/// Close out operations that were running when the server stopped.
///
/// See [`operations::close_orphaned`]; this only reports what it did.
async fn close_orphaned_operations(db: &DatabaseConnection) {
    match operations::close_orphaned(db).await {
        Ok(0) => {}
        Ok(closed) => warn!("Closed {closed} operation(s) left running by a restart"),
        Err(e) => warn!("could not close interrupted operations: {e}"),
    }
}
