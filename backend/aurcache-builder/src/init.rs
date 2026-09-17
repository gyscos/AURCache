//! Lightweight build-queue coordinator.
//!
//! In the remote-worker model the server does not build packages; workers poll
//! for `ENQUEUED` builds and claim them. This coordinator therefore only:
//!
//! * seeds buildable packages into the queue on startup, and
//! * translates a user `Cancel` action into a terminal database state that the
//!   owning worker observes via the heartbeat answer / `job_status` and aborts.
//!
//! `Action::Build` is just a low-latency wakeup hint; workers also poll on an
//! interval, so there is nothing to do for it here.

use aurcache_common::build_state::EndReasons;
use aurcache_common::builder::BuildStates;
use aurcache_db::action::Action;
use aurcache_db::builds;
use aurcache_db::helpers::time::now_secs;
use aurcache_db::helpers::worker_jobs::{STATUS_ACTIVE, STATUS_ENQUEUED, STATUS_WAITING_FOR_DEPS};
use aurcache_db::prelude::{Builds, Packages};
use aurcache_utils::build_logger::append_build_output;
use aurcache_utils::package::enqueue::enqueue_missing_buildable_packages;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, TransactionTrait};
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
                Ok(Action::Build(..)) => {}
                Ok(Action::Cancel(build_id)) => {
                    if let Err(e) = cancel_build(&db, build_id).await {
                        warn!("Failed to cancel build #{build_id}: {e}");
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    // Every sender is gone; `recv()` would fail immediately
                    // forever, so staying in the loop is a busy-loop.
                    break;
                }
                Err(e) => {
                    // Lagged: keep the coordinator alive.
                    warn!("Build action channel error: {e}");
                }
            }
        }
    })
}

/// Cancel a build, one write as the decision point (design §3):
///
/// * A `FAILED` row carries `end_reason = canceled`, `end_time`, and keeps the
///   `worker_id` of whoever ran it — the record of the legitimate aborted
///   attempt, which a later `complete` ack checks against.
/// * It is CAS-guarded on `status IN (ACTIVE, ENQUEUED, WAITING_FOR_DEPS)` so a
///   cancel racing a completion can never retroactively fail a finished build:
///   a successful completion's `ACTIVE -> STATUS_SUCCESS` in the gap makes this
///   a no-op (0 rows) and everything rolls back.
/// * The package mirror happens in the same transaction, and only when the
///   package still points at this build (`latest_build`), exactly like
///   abandonment. No fresh build is queued — this is an operator decision.
///
/// An `ENQUEUED`/`WAITING_FOR_DEPS` build can no longer be claimed; an `ACTIVE`
/// build leaves `ACTIVE`, so its owning worker sees the cancel on its next
/// heartbeat answer / status poll and aborts. The lease reaper never touches it
/// (it only reclaims `ACTIVE` builds, and a cancelled one is no longer ACTIVE).
async fn cancel_build(db: &DatabaseConnection, build_id: i32) -> anyhow::Result<()> {
    let Some(build) = Builds::find_by_id(build_id).one(db).await? else {
        anyhow::bail!("no build with id {build_id}");
    };
    if !matches!(
        build.status,
        Some(STATUS_ACTIVE | STATUS_ENQUEUED | STATUS_WAITING_FOR_DEPS)
    ) {
        anyhow::bail!(
            "build #{build_id} is not cancellable (status {:?})",
            build.status
        );
    }

    let now = now_secs();
    let txn = db.begin().await?;
    let cas = Builds::update_many()
        .col_expr(builds::Column::Status, BuildStates::FAILED_BUILD.into())
        .col_expr(builds::Column::EndTime, Some(now).into())
        .col_expr(builds::Column::EndReason, Some(EndReasons::CANCELED).into())
        .col_expr(builds::Column::LeaseExpiresAt, Option::<i64>::None.into())
        .filter(builds::Column::Id.eq(build_id))
        .filter(builds::Column::Status.is_in([
            Some(STATUS_ACTIVE),
            Some(STATUS_ENQUEUED),
            Some(STATUS_WAITING_FOR_DEPS),
        ]))
        .exec(&txn)
        .await?;
    if cas.rows_affected == 0 {
        txn.rollback().await?;
        anyhow::bail!("build #{build_id} was already resolved");
    }

    // Mirror the package status the same way abandonment does, but only when
    // the package still points at this exact build.
    let pkg = Packages::find_by_id(build.pkg_id).one(&txn).await?;
    if let Some(pkg) = &pkg
        && pkg.latest_build == Some(build_id)
    {
        // One column, not the whole row (which carries the large
        // `source_data` JSON).
        aurcache_db::packages::Entity::update_many()
            .col_expr(
                aurcache_db::packages::Column::Status,
                BuildStates::FAILED_BUILD.into(),
            )
            .filter(aurcache_db::packages::Column::Id.eq(pkg.id))
            .exec(&txn)
            .await?;
    }
    txn.commit().await?;

    // The row is terminal now; appending the reason is best-effort and last.
    if let Some(pkg) = pkg
        && let Err(e) =
            append_build_output(&pkg.name, build.number, "Cancelled by operator.\n").await
    {
        warn!("could not append cancel note to build #{build_id}: {e}");
    }
    info!("Cancelled build #{build_id}");
    Ok(())
}
