//! Lightweight build-queue coordinator.
//!
//! In the remote-worker model the server does not build packages; workers poll
//! for `ENQUEUED` builds and claim them. This coordinator therefore only:
//!
//! * seeds buildable packages into the queue on startup, and
//! * translates a user `Cancel` action into a terminal database state that the
//!   owning worker observes via `GET /worker/jobs/{id}/status` and aborts.
//!
//! `Action::Build` is just a low-latency wakeup hint; workers also poll on an
//! interval, so there is nothing to do for it here.

use aurcache_db::prelude::Builds;
use aurcache_types::builder::{Action, BuildStates};
use aurcache_utils::package::enqueue::enqueue_missing_buildable_packages;
use sea_orm::ActiveValue::Set;
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, IntoActiveModel};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast::Sender;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

#[must_use]
pub fn init_build_queue(db: DatabaseConnection, tx: Sender<Action>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut rx = tx.subscribe();

        if let Err(e) = enqueue_missing_buildable_packages(&db, &tx).await {
            error!("Failed to enqueue buildable packages during startup: {e}");
        }

        loop {
            match rx.recv().await {
                // Workers poll for enqueued builds; the wakeup needs no action.
                Ok(Action::Build(_, _)) => {}
                Ok(Action::Cancel(build_id)) => {
                    if let Err(e) = cancel_build(&db, build_id).await {
                        warn!("Failed to cancel build #{build_id}: {e}");
                    }
                }
                Err(e) => {
                    // Lagged/closed channel: keep the coordinator alive.
                    warn!("Build action channel error: {e}");
                }
            }
        }
    })
}

/// Cancel a build by moving it to a terminal `FAILED` state.
///
/// * An `ENQUEUED` build can no longer be claimed by a worker.
/// * An `ACTIVE` build leaves the `ACTIVE` state, so its owning worker sees
///   `cancel_requested` on its next status poll and aborts; the lease reaper
///   never requeues it (it only reclaims `ACTIVE` builds).
async fn cancel_build(db: &DatabaseConnection, build_id: i32) -> anyhow::Result<()> {
    let Some(build) = Builds::find_by_id(build_id).one(db).await? else {
        anyhow::bail!("no build with id {build_id}");
    };
    let mut active = build.into_active_model();
    active.status = Set(Some(BuildStates::FAILED_BUILD));
    active.worker_id = Set(None);
    active.lease_expires_at = Set(None);
    active.end_time = Set(Some(now_secs()));
    active.update(db).await?;
    info!("Cancelled build #{build_id}");
    Ok(())
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}
