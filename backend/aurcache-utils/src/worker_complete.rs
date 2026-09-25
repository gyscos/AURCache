//! Server-side handling of a worker's build completion: terminal status
//! transitions, version reconciliation, and promotion of dependent builds.
//!
//! This is the Docker-free counterpart of the logic that used to live in the
//! in-process builder (`aurcache-builder`'s `post_build`/`trigger_dependents`).
//! Workers poll for enqueued jobs, so promoting a dependent from
//! `WAITING_FOR_DEPS` to `ENQUEUED` is all that is required to dispatch it.

use aurcache_common::api::log::BuildRef;
use aurcache_common::builder::BuildStates;
use aurcache_db::builds;
use aurcache_db::dependencies;
use aurcache_db::helpers::build_enqueue::{demote_enqueued_build, promote_waiting_build};
use aurcache_db::helpers::time::now_secs;
use aurcache_db::prelude::{Builds, Dependencies, Packages};
use pacman_mirrors::platforms::Platform;
use sea_orm::ActiveValue::Set;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DbErr, EntityTrait, IntoActiveModel,
    QueryFilter, QuerySelect, TransactionSession, TransactionTrait,
};
use std::collections::HashMap;

/// Confirm the given worker currently holds the active lease on the build.
/// Uploads and completions are only accepted from the owning worker while the
/// build is `ACTIVE`.
pub async fn assert_owned_active<C: ConnectionTrait>(
    db: &C,
    worker_id: i32,
    build_id: i32,
) -> Result<builds::Model, DbErr> {
    let build = Builds::find_by_id(build_id)
        .one(db)
        .await?
        .ok_or_else(|| DbErr::Custom(format!("build {build_id} not found")))?;
    check_owned_active(worker_id, &build)?;
    Ok(build)
}

/// As [`assert_owned_active`], without requiring the build still be `ACTIVE`.
///
/// For reports that are not a claim on the build's state, only a comment
/// about it: a worker's problem report can arrive just after the build's
/// terminal state lands (cleanup, an upload still finishing), and discarding
/// it there would lose exactly the report an operator most wants to see.
pub async fn assert_owned<C: ConnectionTrait>(
    db: &C,
    worker_id: i32,
    build_id: i32,
) -> Result<builds::Model, DbErr> {
    let build = Builds::find_by_id(build_id)
        .one(db)
        .await?
        .ok_or_else(|| DbErr::Custom(format!("build {build_id} not found")))?;
    if build.worker_id != Some(worker_id) {
        return Err(DbErr::Custom(format!(
            "build {build_id} is not owned by worker {worker_id}"
        )));
    }
    Ok(build)
}

/// The ownership half of [`assert_owned_active`], for callers that already
/// hold the row: completions fetch the build first for their idempotency
/// checks, and re-querying it here would pay a second point lookup on the
/// hottest worker endpoint for a row that cannot usefully change between the
/// two reads (a concurrent state change fails the CAS further down anyway).
pub fn check_owned_active(worker_id: i32, build: &builds::Model) -> Result<(), DbErr> {
    let build_id = build.id;
    if build.status != Some(BuildStates::ACTIVE_BUILD) {
        return Err(DbErr::Custom(format!("build {build_id} is not active")));
    }
    if build.worker_id != Some(worker_id) {
        return Err(DbErr::Custom(format!(
            "build {build_id} is not owned by worker {worker_id}"
        )));
    }
    Ok(())
}

/// Record the peak memory a worker reported for a build.
///
/// Written before the success/failure branch, because a build that was
/// OOM-killed is exactly when the number is worth having and that build will
/// never reach the success path.
///
/// Best-effort rather than compare-and-swap: unlike the version and the size,
/// this publishes nothing and decides nothing, so a build whose lease was
/// reclaimed mid-flight is not worth failing a completion over. A stale write
/// records a measurement that did happen, for a build that did run.
pub async fn record_peak_memory<C: ConnectionTrait>(
    db: &C,
    build_id: i32,
    peak_memory_bytes: i64,
) -> Result<(), DbErr> {
    Builds::update_many()
        .col_expr(builds::Column::PeakMemory, Some(peak_memory_bytes).into())
        .filter(builds::Column::Id.eq(build_id))
        .exec(db)
        .await?;
    Ok(())
}

/// Record the disk a build used on its worker, part by part. Best-effort, for
/// the same reason as [`record_peak_memory`], and for a failed build too: one
/// that ran out of disk is the one whose figures matter most.
pub async fn record_disk_usage<C: ConnectionTrait>(
    db: &C,
    build_id: i32,
    usage: &aurcache_common::api::builds::DiskUsage,
) -> Result<(), DbErr> {
    Builds::update_many()
        .col_expr(builds::Column::DiskChroot, usage.chroot.into())
        .col_expr(builds::Column::DiskWorkdir, usage.workdir.into())
        .col_expr(builds::Column::DiskSources, usage.sources.into())
        .col_expr(builds::Column::DiskBuildTree, usage.build_tree.into())
        .filter(builds::Column::Id.eq(build_id))
        .exec(db)
        .await?;
    Ok(())
}

/// A build the worker was building is no longer `ACTIVE`-and-owned by it (the
/// lease was reclaimed by the reaper and possibly re-handed to another worker).
/// The late completion must be discarded rather than clobbering the new owner.
fn lease_lost(build_id: i32, worker_id: i32) -> DbErr {
    DbErr::Custom(format!(
        "build {build_id} is no longer owned+active by worker {worker_id} (lease lost); completion discarded"
    ))
}

/// Move a build and its package to a terminal status in one transaction,
/// releasing the lease. The build write is a compare-and-swap on
/// `status = ACTIVE AND worker_id = ?`, so a completion arriving after the
/// reaper reclaimed the build is discarded rather than clobbering the new owner.
///
/// Returns the completed build.
async fn finish_build<C: ConnectionTrait + TransactionTrait>(
    db: &C,
    build_id: i32,
    worker_id: i32,
    status: i32,
) -> Result<builds::Model, DbErr> {
    let build = Builds::find_by_id(build_id)
        .one(db)
        .await?
        .ok_or_else(|| DbErr::Custom(format!("build {build_id} not found")))?;

    let txn = db.begin().await?;
    let res = Builds::update_many()
        .col_expr(builds::Column::Status, status.into())
        // `worker_id` is kept. On a finished build it is no longer a claim,
        // it is the record of which machine produced the package -- which the
        // workers page promises ("the row is kept so old builds still name the
        // machine that ran them") and, until now, could not deliver, because
        // this cleared it the moment the build ended. Nothing mistakes it for
        // ownership: every path that treats a build as owned -- heartbeat,
        // requeue_worker_builds, the lease reaper, claim_job -- also requires
        // a non-terminal status.
        //
        // The lease is a different thing and does end here.
        .col_expr(builds::Column::LeaseExpiresAt, Option::<i64>::None.into())
        .col_expr(builds::Column::EndTime, Some(now_secs()).into())
        .filter(builds::Column::Id.eq(build_id))
        .filter(builds::Column::Status.eq(BuildStates::ACTIVE_BUILD))
        .filter(builds::Column::WorkerId.eq(worker_id))
        .exec(&txn)
        .await?;
    if res.rows_affected == 0 {
        txn.rollback().await?;
        return Err(lease_lost(build_id, worker_id));
    }

    if let Some(pkg) = Packages::find_by_id(build.pkg_id).one(&txn).await? {
        let mut pkg = pkg.into_active_model();
        pkg.status = Set(status);
        if status == BuildStates::SUCCESSFUL_BUILD {
            pkg.out_of_date = Set(0);
        }
        pkg.update(&txn).await?;
    }
    txn.commit().await?;
    Ok(build)
}

/// Take a finished build over from the worker that built it, to publish.
///
/// The worker's artifacts are all uploaded and it has said so: from here on
/// nothing it could do would change the outcome, so it is released. The build
/// moves from `ACTIVE` to `PUBLISHING` and its lease ends -- which is also what
/// takes it out of the reach of the heartbeat, the reaper and a revocation, all
/// of which only look at `ACTIVE` builds. `worker_id` stays, as the record of
/// who built it.
///
/// A compare-and-swap on `status = ACTIVE AND worker_id = ?`, like every other
/// completion: a build this worker has already lost is not taken over.
pub async fn accept_for_publishing<C: ConnectionTrait + TransactionTrait>(
    db: &C,
    build_id: i32,
    worker_id: i32,
) -> Result<(), DbErr> {
    let build = Builds::find_by_id(build_id)
        .one(db)
        .await?
        .ok_or_else(|| DbErr::Custom(format!("build {build_id} not found")))?;

    let txn = db.begin().await?;
    let res = Builds::update_many()
        .col_expr(builds::Column::Status, BuildStates::PUBLISHING.into())
        .col_expr(builds::Column::LeaseExpiresAt, Option::<i64>::None.into())
        .filter(builds::Column::Id.eq(build_id))
        .filter(builds::Column::Status.eq(BuildStates::ACTIVE_BUILD))
        .filter(builds::Column::WorkerId.eq(worker_id))
        .exec(&txn)
        .await?;
    if res.rows_affected == 0 {
        txn.rollback().await?;
        return Err(lease_lost(build_id, worker_id));
    }
    if let Some(pkg) = Packages::find_by_id(build.pkg_id).one(&txn).await? {
        let mut pkg = pkg.into_active_model();
        pkg.status = Set(BuildStates::PUBLISHING);
        pkg.update(&txn).await?;
    }
    txn.commit().await?;
    Ok(())
}

/// Mark a build (and its package) as failed. Deterministic failures reported by
/// the worker are terminal — no re-enqueue.
pub async fn complete_failure<C: ConnectionTrait + TransactionTrait>(
    db: &C,
    build_id: i32,
    worker_id: i32,
) -> Result<(), DbErr> {
    finish_build(db, build_id, worker_id, BuildStates::FAILED_BUILD).await?;
    Ok(())
}

/// After a successful build, promote any dependent whose dependencies are now
/// all satisfied from `WAITING_FOR_DEPS` to `ENQUEUED` so a worker can claim it.
///
/// Returns the builds it promoted, for the caller to log.
pub async fn trigger_dependents<C: ConnectionTrait>(
    db: &C,
    pkg_id: i32,
    platform: Platform,
) -> Result<Vec<BuildRef>, DbErr> {
    let deps_by_dependent = load_dependencies_for_dependents_of(db, pkg_id).await?;
    let mut promoted = vec![];
    for (dependent_id, all_deps) in &deps_by_dependent {
        if dependencies_ready(db, all_deps, platform).await?
            && let Some(build) = promote_dependent(db, *dependent_id, platform).await?
        {
            promoted.push(build);
        }
    }
    Ok(promoted)
}

/// Re-decide whether one package's queued build can still start, after its
/// dependencies changed.
///
/// Repointing or dropping a dependency edge changes the answer in *both*
/// directions, which is why this is not just [`trigger_dependents`]:
///
/// * a build held at `WAITING_FOR_DEPS` may now be free to go, because the
///   dependency that was holding it up is no longer one of this package's --
///   this is what makes "replace the dependency" unblock a build rather than
///   leave it waiting on a package it no longer needs;
/// * a build already `ENQUEUED` may now have to wait, because the replacement
///   it was pointed at has not been built yet. Leaving it queued sends it to a
///   worker that cannot resolve its dependencies, and the build fails for a
///   reason the queue already knew about.
///
/// Every platform the package has a pending build on, since a dependency may be
/// satisfied on one and not another. `ACTIVE` builds are left alone: they are
/// running, and the queue has nothing left to say about them.
///
/// Returns the ids of the builds it let start, for the caller to log.
pub async fn resync_pending_builds<C: ConnectionTrait>(
    db: &C,
    pkg_id: i32,
) -> Result<Vec<i32>, DbErr> {
    let pending: Vec<builds::Model> = Builds::find()
        .filter(builds::Column::PkgId.eq(pkg_id))
        .filter(builds::Column::Status.is_in([
            Some(BuildStates::ENQUEUED_BUILD),
            Some(BuildStates::WAITING_FOR_DEPS),
        ]))
        .all(db)
        .await?;
    if pending.is_empty() {
        return Ok(vec![]);
    }
    let mut unblocked = vec![];

    let deps = Dependencies::find()
        .filter(dependencies::Column::DependentId.eq(pkg_id))
        .all(db)
        .await?;

    for build in pending {
        let platform = build.platform;
        let ready = dependencies_ready(db, &deps, platform).await?;
        match (build.status, ready) {
            (Some(BuildStates::WAITING_FOR_DEPS), true) => {
                if let Some(promoted) = promote_waiting_build(db, pkg_id, platform).await? {
                    tracing::info!(
                        "Build #{} on {platform} can start: its dependencies changed and are now satisfied",
                        promoted.id
                    );
                    unblocked.push(promoted.id);
                }
            }
            (Some(BuildStates::ENQUEUED_BUILD), false)
                if demote_enqueued_build(db, pkg_id, platform).await? =>
            {
                tracing::info!(
                    "Build #{} on {platform} must wait: its dependencies changed and are not satisfied yet",
                    build.id
                );
            }
            _ => {}
        }
    }
    Ok(unblocked)
}

async fn load_dependencies_for_dependents_of<C: ConnectionTrait>(
    db: &C,
    pkg_id: i32,
) -> Result<HashMap<i32, Vec<dependencies::Model>>, DbErr> {
    let dependent_ids: Vec<i32> = Dependencies::find()
        .filter(dependencies::Column::DependeeId.eq(pkg_id))
        .select_only()
        .column(dependencies::Column::DependentId)
        .into_tuple()
        .all(db)
        .await?;
    if dependent_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let deps = Dependencies::find()
        .filter(dependencies::Column::DependentId.is_in(dependent_ids))
        .all(db)
        .await?;
    let mut map: HashMap<i32, Vec<dependencies::Model>> = HashMap::new();
    for dep in deps {
        map.entry(dep.dependent_id).or_default().push(dep);
    }
    Ok(map)
}

async fn dependencies_ready<C: ConnectionTrait>(
    db: &C,
    all_deps: &[dependencies::Model],
    platform: Platform,
) -> Result<bool, DbErr> {
    for dep in all_deps {
        if !aurcache_db::helpers::builds::dependency_satisfied(
            db,
            dep.dependee_id,
            platform.as_str(),
            &dep.version_constraint,
        )
        .await?
        {
            return Ok(false);
        }
    }
    Ok(true)
}

async fn promote_dependent<C: ConnectionTrait>(
    db: &C,
    dependent_id: i32,
    platform: Platform,
) -> Result<Option<BuildRef>, DbErr> {
    let Some(pkg) = Packages::find_by_id(dependent_id).one(db).await? else {
        return Ok(None);
    };
    if !pkg.platforms.trim().is_empty()
        && !Platform::parse_many(&pkg.platforms).any(|r| r.is_ok_and(|p| p == platform))
    {
        return Ok(None);
    }
    Ok(promote_waiting_build(db, pkg.id, platform)
        .await?
        .map(|promoted| BuildRef {
            pkgbase: pkg.name,
            number: promoted.number,
        }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aurcache_db::migration::Migrator;
    use sea_orm::{Database, DatabaseConnection};
    use sea_orm_migration::MigratorTrait;

    async fn setup() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        db
    }

    async fn pkg(db: &DatabaseConnection, id: i32) {
        db.execute_unprepared(&format!(
            "INSERT INTO packages \
             (id, name, status, out_of_date, build_flags, platforms, source_type, source_data, directly_requested) \
             VALUES ({id}, 'p{id}', 0, 0, '', 'x86_64', 'aur', '{{\"type\":\"aur\",\"name\":\"p{id}\"}}', 1)"
        ))
        .await
        .unwrap();
    }

    async fn build(db: &DatabaseConnection, id: i32, pkg_id: i32, status: i32, worker: &str) {
        db.execute_unprepared(&format!(
            "INSERT INTO builds (id, pkg_id, status, start_time, platform, version, worker_id, attempt_count) \
             VALUES ({id}, {pkg_id}, {status}, 0, 'x86_64', '1.0', {worker}, 0)"
        ))
        .await
        .unwrap();
    }

    /// A finished build has to keep saying which machine produced it. The
    /// workers page tells operators the row is kept "so old builds still name
    /// the machine that ran them", and every per-worker figure -- how many
    /// builds it has finished, how many failed, its share of the fleet -- is
    /// counted from this column. Clearing it on completion left every one of
    /// them at zero for a fleet that had built everything in the repository.
    /// The figure has to survive to the row, and it must land for a *failed*
    /// build too: an OOM kill is the case it exists for, and that build never
    /// reaches the success path.
    #[tokio::test]
    async fn peak_memory_is_recorded_for_a_failed_build() {
        let db = setup().await;
        pkg(&db, 1).await;
        build(&db, 1, 1, BuildStates::ACTIVE_BUILD, "1").await;

        record_peak_memory(&db, 1, 6 * 1024 * 1024 * 1024)
            .await
            .unwrap();
        complete_failure(&db, 1, 1).await.unwrap();

        let row = Builds::find_by_id(1).one(&db).await.unwrap().unwrap();
        assert_eq!(row.peak_memory, Some(6 * 1024 * 1024 * 1024));
        assert_eq!(row.status, Some(BuildStates::FAILED_BUILD));
    }

    /// A build's disk usage is kept part by part, and a part the worker did
    /// not measure stays unknown rather than becoming 0.
    #[tokio::test]
    async fn disk_usage_is_recorded_part_by_part() {
        let db = setup().await;
        pkg(&db, 1).await;
        build(&db, 1, 1, BuildStates::ACTIVE_BUILD, "1").await;

        let usage = aurcache_common::api::builds::DiskUsage {
            chroot: Some(700),
            workdir: Some(50),
            sources: Some(9),
            build_tree: None,
        };
        record_disk_usage(&db, 1, &usage).await.unwrap();

        let row = Builds::find_by_id(1).one(&db).await.unwrap().unwrap();
        assert_eq!(row.disk_chroot, Some(700));
        assert_eq!(row.disk_workdir, Some(50));
        assert_eq!(row.disk_sources, Some(9));
        assert_eq!(row.disk_build_tree, None, "not measured is not zero");
    }

    #[tokio::test]
    async fn a_finished_build_still_names_the_worker_that_ran_it() {
        let db = setup().await;
        pkg(&db, 1).await;
        build(&db, 10, 1, BuildStates::ACTIVE_BUILD, "5").await;

        accept_for_publishing(&db, 10, 5).await.unwrap();

        let finished = Builds::find_by_id(10).one(&db).await.unwrap().unwrap();
        assert_eq!(finished.status, Some(BuildStates::PUBLISHING));
        assert_eq!(
            finished.worker_id,
            Some(5),
            "the build no longer names the worker that produced it"
        );
        // The lease is a claim on a running build and does end here.
        assert_eq!(finished.lease_expires_at, None);
    }

    async fn dep(db: &DatabaseConnection, dependent: i32, dependee: i32) {
        db.execute_unprepared(&format!(
            "INSERT INTO dependencies (dependent_id, dependee_id, version_constraint) \
             VALUES ({dependent}, {dependee}, '')"
        ))
        .await
        .unwrap();
    }

    async fn status_of(db: &DatabaseConnection, build_id: i32) -> Option<i32> {
        Builds::find_by_id(build_id)
            .one(db)
            .await
            .unwrap()
            .unwrap()
            .status
    }

    /// The point of repointing a dependency: the build waiting on the old one
    /// can go, because the old one is no longer what it needs.
    #[tokio::test]
    async fn a_waiting_build_starts_once_its_dependency_is_repointed() {
        let db = setup().await;
        pkg(&db, 1).await; // the dependent
        pkg(&db, 2).await; // the replacement, already built
        build(&db, 20, 2, BuildStates::SUCCESSFUL_BUILD, "1").await;
        build(&db, 10, 1, BuildStates::WAITING_FOR_DEPS, "NULL").await;
        dep(&db, 1, 2).await;

        resync_pending_builds(&db, 1).await.unwrap();

        assert_eq!(status_of(&db, 10).await, Some(BuildStates::ENQUEUED_BUILD));
    }

    /// A promotion is an all-of, not a check of the edge that just changed.
    ///
    /// Repointing one dependency says nothing about the others: the build is
    /// only free to start when *every* dependency is satisfied, and promoting on
    /// the strength of the one that was touched would send a build to a worker
    /// that still cannot resolve the rest.
    #[tokio::test]
    async fn a_waiting_build_stays_waiting_while_another_dependency_is_unbuilt() {
        let db = setup().await;
        pkg(&db, 1).await; // the dependent
        pkg(&db, 2).await; // the repointed dependency, built
        pkg(&db, 3).await; // a second dependency, never built
        build(&db, 20, 2, BuildStates::SUCCESSFUL_BUILD, "1").await;
        build(&db, 10, 1, BuildStates::WAITING_FOR_DEPS, "NULL").await;
        dep(&db, 1, 2).await;
        dep(&db, 1, 3).await;

        resync_pending_builds(&db, 1).await.unwrap();

        assert_eq!(
            status_of(&db, 10).await,
            Some(BuildStates::WAITING_FOR_DEPS),
            "promoted on one satisfied dependency while another is unbuilt"
        );

        // ... and it goes as soon as the last one lands.
        build(&db, 30, 3, BuildStates::SUCCESSFUL_BUILD, "1").await;
        resync_pending_builds(&db, 1).await.unwrap();
        assert_eq!(status_of(&db, 10).await, Some(BuildStates::ENQUEUED_BUILD));
    }

    /// And the other direction, which is the one a promotion-only pass misses:
    /// a build already queued against the old dependency has to wait when the
    /// replacement has not been built.
    #[tokio::test]
    async fn a_queued_build_waits_when_its_new_dependency_is_not_built() {
        let db = setup().await;
        pkg(&db, 1).await;
        pkg(&db, 2).await; // the replacement, never built
        build(&db, 10, 1, BuildStates::ENQUEUED_BUILD, "NULL").await;
        dep(&db, 1, 2).await;

        resync_pending_builds(&db, 1).await.unwrap();

        assert_eq!(
            status_of(&db, 10).await,
            Some(BuildStates::WAITING_FOR_DEPS)
        );
    }

    /// A build a worker is already running is none of the queue's business.
    #[tokio::test]
    async fn a_running_build_is_left_alone() {
        let db = setup().await;
        pkg(&db, 1).await;
        pkg(&db, 2).await;
        build(&db, 10, 1, BuildStates::ACTIVE_BUILD, "5").await;
        dep(&db, 1, 2).await;

        resync_pending_builds(&db, 1).await.unwrap();

        assert_eq!(status_of(&db, 10).await, Some(BuildStates::ACTIVE_BUILD));
    }

    /// Dropping the last dependency leaves nothing to wait for.
    #[tokio::test]
    async fn a_waiting_build_starts_when_its_last_dependency_is_dropped() {
        let db = setup().await;
        pkg(&db, 1).await;
        build(&db, 10, 1, BuildStates::WAITING_FOR_DEPS, "NULL").await;

        resync_pending_builds(&db, 1).await.unwrap();

        assert_eq!(status_of(&db, 10).await, Some(BuildStates::ENQUEUED_BUILD));
    }

    #[tokio::test]
    async fn owned_active_guard() {
        let db = setup().await;
        pkg(&db, 1).await;
        build(&db, 10, 1, BuildStates::ACTIVE_BUILD, "5").await;
        assert!(assert_owned_active(&db, 5, 10).await.is_ok());
        assert!(assert_owned_active(&db, 6, 10).await.is_err());
    }

    /// Accepting a completion releases the worker: the build is the server's to
    /// publish, with no lease left for anything to police, and nothing recorded
    /// as finished until it is published.
    #[tokio::test]
    async fn an_accepted_build_is_published_without_its_worker() {
        let db = setup().await;
        pkg(&db, 1).await;
        build(&db, 10, 1, BuildStates::ACTIVE_BUILD, "5").await;
        db.execute_unprepared("UPDATE builds SET lease_expires_at = 999 WHERE id = 10")
            .await
            .unwrap();

        accept_for_publishing(&db, 10, 5).await.unwrap();

        let b = Builds::find_by_id(10).one(&db).await.unwrap().unwrap();
        assert_eq!(b.status, Some(BuildStates::PUBLISHING));
        assert_eq!(b.lease_expires_at, None);
        assert_eq!(b.worker_id, Some(5));
        assert_eq!(b.end_time, None, "not finished until published");
        let p = Packages::find_by_id(1).one(&db).await.unwrap().unwrap();
        assert_eq!(p.status, BuildStates::PUBLISHING);

        // Accepted once: a repeat is no longer an ACTIVE build of this worker.
        assert!(accept_for_publishing(&db, 10, 5).await.is_err());
    }

    #[tokio::test]
    async fn failure_is_terminal_not_requeued() {
        let db = setup().await;
        pkg(&db, 1).await;
        build(&db, 10, 1, BuildStates::ACTIVE_BUILD, "5").await;
        complete_failure(&db, 10, 5).await.unwrap();
        let b = Builds::find_by_id(10).one(&db).await.unwrap().unwrap();
        assert_eq!(b.status, Some(BuildStates::FAILED_BUILD));
        assert_eq!(b.attempt_count, 0);
        // A failed build produced nothing to measure. `None` says that; a `0`
        // would claim it produced an empty package.
        assert_eq!(b.size, None);
    }

    #[tokio::test]
    async fn completion_by_non_owner_is_discarded() {
        let db = setup().await;
        pkg(&db, 1).await;
        // Build reclaimed and re-handed to worker 6.
        build(&db, 10, 1, BuildStates::ACTIVE_BUILD, "6").await;
        // Late completion from the original owner (worker 5) must not clobber it.
        assert!(accept_for_publishing(&db, 10, 5).await.is_err());
        assert!(complete_failure(&db, 10, 5).await.is_err());
        let b = Builds::find_by_id(10).one(&db).await.unwrap().unwrap();
        assert_eq!(b.status, Some(BuildStates::ACTIVE_BUILD));
        assert_eq!(b.worker_id, Some(6));
    }
}
