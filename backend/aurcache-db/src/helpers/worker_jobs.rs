//! Database helpers for the remote-worker job lifecycle: atomic claim, lease
//! renewal via heartbeat, and requeue/fail of dropped builds.
//!
//! Build status integers mirror `aurcache_types::builder::BuildStates`
//! (0=active, 1=success, 2=failed, 3=enqueued, 4=waiting-for-deps); the db
//! crate keeps its own copy to avoid a dependency on the types crate here.

use crate::builds;
use crate::prelude::Builds;
use sea_orm::{
    ColumnTrait, ConnectionTrait, DbErr, EntityTrait, QueryFilter, QueryOrder, QuerySelect,
};
use std::time::{SystemTime, UNIX_EPOCH};

pub const STATUS_ACTIVE: i32 = 0;
pub const STATUS_SUCCESS: i32 = 1;
pub const STATUS_FAILED: i32 = 2;
pub const STATUS_ENQUEUED: i32 = 3;

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Atomically claim the next buildable job, preferring native arches over
/// emulated ones and reserving foreign-arch jobs for native workers.
///
/// Routing (see design doc §Arch-aware routing):
/// 1. Try the worker's **native** arches first (oldest job wins).
/// 2. Only if none, try **emulated** arches — but skip any platform that some
///    approved worker can build *natively*, so a foreign-arch job stays reserved
///    for the native worker instead of being emulated slowly elsewhere.
///
/// The transition is a conditional `UPDATE ... WHERE status = ENQUEUED`, so two
/// workers racing for the same build see exactly one `rows_affected == 1`.
pub async fn claim_job<C: ConnectionTrait>(
    db: &C,
    worker_id: i32,
    native_arches: &[String],
    emulated_arches: &[String],
    lease_ttl_secs: i64,
) -> Result<Option<builds::Model>, DbErr> {
    // 1. Native work first.
    if let Some(build) = claim_among(db, worker_id, native_arches, lease_ttl_secs).await? {
        return Ok(Some(build));
    }

    // 2. Emulated work, minus any arch reserved for a native worker.
    if !emulated_arches.is_empty() {
        let reserved = arches_with_native_worker(db).await?;
        let emulatable: Vec<String> = emulated_arches
            .iter()
            .filter(|a| !reserved.contains(*a))
            .cloned()
            .collect();
        if let Some(build) = claim_among(db, worker_id, &emulatable, lease_ttl_secs).await? {
            return Ok(Some(build));
        }
    }
    Ok(None)
}

/// Claim the oldest enqueued build among the given platforms, atomically.
async fn claim_among<C: ConnectionTrait>(
    db: &C,
    worker_id: i32,
    arches: &[String],
    lease_ttl_secs: i64,
) -> Result<Option<builds::Model>, DbErr> {
    if arches.is_empty() {
        return Ok(None);
    }

    let candidates: Vec<i32> = Builds::find()
        .select_only()
        .column(builds::Column::Id)
        .filter(builds::Column::Status.eq(STATUS_ENQUEUED))
        .filter(builds::Column::Platform.is_in(arches.iter().map(String::as_str)))
        .order_by_asc(builds::Column::StartTime)
        .order_by_asc(builds::Column::Id)
        .into_tuple()
        .all(db)
        .await?;

    let now = now_secs();
    for id in candidates {
        let res = Builds::update_many()
            .col_expr(builds::Column::Status, STATUS_ACTIVE.into())
            .col_expr(builds::Column::WorkerId, worker_id.into())
            .col_expr(
                builds::Column::LeaseExpiresAt,
                (now + lease_ttl_secs).into(),
            )
            .col_expr(builds::Column::StartTime, now.into())
            .filter(builds::Column::Id.eq(id))
            .filter(builds::Column::Status.eq(STATUS_ENQUEUED))
            .exec(db)
            .await?;
        if res.rows_affected == 1 {
            return Builds::find_by_id(id).one(db).await;
        }
    }
    Ok(None)
}

/// Set of platform strings that at least one *approved* worker can build
/// natively — these are reserved from emulated claims.
async fn arches_with_native_worker<C: ConnectionTrait>(
    db: &C,
) -> Result<std::collections::HashSet<String>, DbErr> {
    use crate::prelude::Workers;
    use crate::workers;

    let rows: Vec<String> = Workers::find()
        .select_only()
        .column(workers::Column::NativeArches)
        .filter(workers::Column::Status.eq("approved"))
        .into_tuple()
        .all(db)
        .await?;

    let mut set = std::collections::HashSet::new();
    for row in rows {
        for a in row.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            set.insert(a.to_string());
        }
    }
    Ok(set)
}

/// Result of processing a heartbeat: builds that were reconciled away from the
/// worker because it stopped reporting them while still alive.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct HeartbeatOutcome {
    /// Ids whose lease was renewed.
    pub renewed: Vec<i32>,
    /// Ids that were owned + active but absent from the report; requeued.
    pub dropped: Vec<i32>,
}

/// Renew leases for the builds a worker reports as active, and reconcile any
/// build still owned + active by this worker that it *didn't* report (the
/// worker silently dropped it) by requeueing it immediately.
pub async fn heartbeat<C: ConnectionTrait>(
    db: &C,
    worker_id: i32,
    active_build_ids: &[i32],
    lease_ttl_secs: i64,
    max_attempts: i32,
) -> Result<HeartbeatOutcome, DbErr> {
    let now = now_secs();
    let mut outcome = HeartbeatOutcome::default();

    // Every build this worker currently owns in the ACTIVE state.
    let owned: Vec<builds::Model> = Builds::find()
        .filter(builds::Column::WorkerId.eq(worker_id))
        .filter(builds::Column::Status.eq(STATUS_ACTIVE))
        .all(db)
        .await?;

    for build in owned {
        let id = build.id;
        if active_build_ids.contains(&id) {
            Builds::update_many()
                .col_expr(
                    builds::Column::LeaseExpiresAt,
                    (now + lease_ttl_secs).into(),
                )
                .filter(builds::Column::Id.eq(id))
                .filter(builds::Column::WorkerId.eq(worker_id))
                .filter(builds::Column::Status.eq(STATUS_ACTIVE))
                .exec(db)
                .await?;
            outcome.renewed.push(id);
        } else if requeue_or_fail(db, &build, max_attempts).await? != RequeueOutcome::Unchanged {
            outcome.dropped.push(id);
        }
    }
    Ok(outcome)
}

/// What a [`requeue_or_fail`] attempt actually did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequeueOutcome {
    /// `ACTIVE -> ENQUEUED`: the build gets another attempt.
    Requeued,
    /// `ACTIVE -> FAILED`: the `max_attempts` budget was exhausted.
    Failed,
    /// The row moved on between the caller's read and this write (the worker
    /// completed it, renewed its lease, or another pass already reclaimed it).
    /// Nothing was written.
    Unchanged,
}

/// Requeue a build after its worker was lost, or fail it once the retry budget
/// is exhausted. Transitions `ACTIVE -> ENQUEUED` (clearing the lease/owner and
/// bumping `attempt_count`), or `-> FAILED` after `max_attempts` requeues.
///
/// The write is an optimistic compare-and-swap pinned to the exact row revision
/// the caller observed (`status`, `worker_id`, `lease_expires_at`,
/// `attempt_count`), mirroring the CAS guards in `record_built_version` /
/// `complete_success`. Callers necessarily select a snapshot of candidates and
/// act on it afterwards; without this pin, a build that completed in that gap
/// would be forced back to `ENQUEUED` after its package status was already
/// updated and its dependents promoted. Any concurrent change instead yields
/// [`RequeueOutcome::Unchanged`] and the row is left alone.
pub async fn requeue_or_fail<C: ConnectionTrait>(
    db: &C,
    observed: &builds::Model,
    max_attempts: i32,
) -> Result<RequeueOutcome, DbErr> {
    let requeue = observed.attempt_count < max_attempts;

    let mut update = Builds::update_many()
        .col_expr(builds::Column::WorkerId, Option::<i32>::None.into())
        .col_expr(builds::Column::LeaseExpiresAt, Option::<i64>::None.into());
    update = if requeue {
        update
            .col_expr(builds::Column::Status, STATUS_ENQUEUED.into())
            .col_expr(
                builds::Column::AttemptCount,
                (observed.attempt_count + 1).into(),
            )
    } else {
        update
            .col_expr(builds::Column::Status, STATUS_FAILED.into())
            .col_expr(builds::Column::EndTime, Some(now_secs()).into())
    };

    let res = update
        .filter(builds::Column::Id.eq(observed.id))
        .filter(builds::Column::Status.eq(STATUS_ACTIVE))
        .filter(match observed.worker_id {
            Some(w) => builds::Column::WorkerId.eq(w),
            None => builds::Column::WorkerId.is_null(),
        })
        .filter(match observed.lease_expires_at {
            Some(l) => builds::Column::LeaseExpiresAt.eq(l),
            None => builds::Column::LeaseExpiresAt.is_null(),
        })
        .filter(builds::Column::AttemptCount.eq(observed.attempt_count))
        .exec(db)
        .await?;

    if res.rows_affected == 0 {
        return Ok(RequeueOutcome::Unchanged);
    }
    Ok(if requeue {
        RequeueOutcome::Requeued
    } else {
        RequeueOutcome::Failed
    })
}

/// Result of a reaper pass: which owned+active builds were requeued vs. given up.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReapOutcome {
    /// Builds re-enqueued for another attempt (budget remaining).
    pub requeued: Vec<i32>,
    /// Builds terminally failed (budget exhausted or backstop timeout).
    pub failed: Vec<i32>,
}

/// Reaper pass: reclaim `ACTIVE` builds whose owning worker went silent, and
/// backstop builds that have run implausibly long.
///
/// A build is reaped when either:
/// * its lease has expired (`lease_expires_at < now`, or `NULL` — no live
///   lease) — the worker missed enough heartbeats that we treat it as lost; or
/// * it has been `ACTIVE` past the backstop deadline
///   (`start_time + max_build_age < now`) even if a lease still appears fresh —
///   covers a hung build whose worker keeps heartbeating.
///
/// Each reaped build goes through [`requeue_or_fail`], so the `attempt_count`
/// budget bounds retries before a terminal `FAILED`.
///
/// `now` and `max_build_age` (`MAX_BUILD_DURATION + grace`, in seconds) are
/// passed in so callers stay testable and can source them from settings.
pub async fn reap_expired_builds<C: ConnectionTrait>(
    db: &C,
    now: i64,
    max_attempts: i32,
    max_build_age: i64,
) -> Result<ReapOutcome, DbErr> {
    let backstop_before = now - max_build_age;
    let candidates: Vec<builds::Model> = Builds::find()
        .filter(builds::Column::Status.eq(STATUS_ACTIVE))
        .filter(
            sea_orm::Condition::any()
                .add(builds::Column::LeaseExpiresAt.lt(now))
                .add(builds::Column::LeaseExpiresAt.is_null())
                .add(builds::Column::StartTime.lt(backstop_before)),
        )
        .all(db)
        .await?;

    let mut outcome = ReapOutcome::default();
    for build in candidates {
        // `requeue_or_fail` pins the CAS to the revision observed here, so a
        // build a concurrent heartbeat renewed or a completion resolved in the
        // gap since the select above is skipped rather than clobbered.
        match requeue_or_fail(db, &build, max_attempts).await? {
            RequeueOutcome::Requeued => outcome.requeued.push(build.id),
            RequeueOutcome::Failed => outcome.failed.push(build.id),
            RequeueOutcome::Unchanged => {}
        }
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::Migrator;
    use sea_orm::{Database, DatabaseConnection};
    use sea_orm_migration::MigratorTrait;

    async fn setup() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        db
    }

    async fn enqueue(db: &DatabaseConnection, id: i32, platform: &str, start: i64) {
        // A partial unique index forbids two pending builds for the same
        // (pkg_id, platform); give every build its own package.
        db.execute_unprepared(&format!(
            "INSERT INTO packages (id, name) VALUES ({id}, 'p{id}')"
        ))
        .await
        .unwrap();
        db.execute_unprepared(&format!(
            "INSERT INTO builds (id, pkg_id, status, start_time, platform, version, attempt_count) \
             VALUES ({id}, {id}, {STATUS_ENQUEUED}, {start}, '{platform}', '1.0', 0)"
        ))
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn claims_oldest_matching_arch() {
        let db = setup().await;
        enqueue(&db, 10, "x86_64", 200).await;
        enqueue(&db, 11, "x86_64", 100).await;
        enqueue(&db, 12, "aarch64", 50).await;

        let claimed = claim_job(&db, 7, &["x86_64".to_string()], &[], 60)
            .await
            .unwrap()
            .unwrap();
        // Oldest x86_64 job wins; the older aarch64 job is not buildable.
        assert_eq!(claimed.id, 11);
        assert_eq!(claimed.status, Some(STATUS_ACTIVE));
        assert_eq!(claimed.worker_id, Some(7));
        assert!(claimed.lease_expires_at.is_some());
    }

    #[tokio::test]
    async fn claim_is_atomic_no_double_handout() {
        let db = setup().await;
        enqueue(&db, 20, "x86_64", 100).await;
        let a = claim_job(&db, 1, &["x86_64".to_string()], &[], 60)
            .await
            .unwrap();
        let b = claim_job(&db, 2, &["x86_64".to_string()], &[], 60)
            .await
            .unwrap();
        assert!(a.is_some());
        assert!(b.is_none());
    }

    async fn approved_worker(db: &DatabaseConnection, id: i32, native: &str) {
        db.execute_unprepared(&format!(
            "INSERT INTO workers (id, name, status, cert_fingerprint, native_arches, emulated_arches) \
             VALUES ({id}, 'w{id}', 'approved', 'fp{id}', '{native}', '')"
        ))
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn native_preferred_over_emulated() {
        let db = setup().await;
        enqueue(&db, 30, "aarch64", 50).await; // older, emulatable
        enqueue(&db, 31, "x86_64", 100).await; // newer, native

        // Worker is native x86_64 and can emulate aarch64. No native aarch64
        // worker exists, so it *may* emulate — but native work comes first.
        let claimed = claim_job(
            &db,
            1,
            &["x86_64".to_string()],
            &["aarch64".to_string()],
            60,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(claimed.id, 31);
    }

    #[tokio::test]
    async fn foreign_arch_reserved_for_native_worker() {
        let db = setup().await;
        enqueue(&db, 40, "aarch64", 50).await;
        // A native aarch64 worker is registered (id=2) -> aarch64 is reserved.
        approved_worker(&db, 2, "aarch64").await;

        // An x86_64 worker that can emulate aarch64 must NOT grab the reserved job.
        let claimed = claim_job(
            &db,
            1,
            &["x86_64".to_string()],
            &["aarch64".to_string()],
            60,
        )
        .await
        .unwrap();
        assert!(claimed.is_none());

        // The native aarch64 worker claims it.
        let native = claim_job(&db, 2, &["aarch64".to_string()], &[], 60)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(native.id, 40);
    }

    #[tokio::test]
    async fn emulated_claim_when_no_native_worker() {
        let db = setup().await;
        enqueue(&db, 50, "armv7h", 50).await;
        // No native armv7h worker -> an emulating worker may take it.
        let claimed = claim_job(&db, 1, &["x86_64".to_string()], &["armv7h".to_string()], 60)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claimed.id, 50);
    }

    #[tokio::test]
    async fn heartbeat_renews_reported_and_requeues_dropped() {
        let db = setup().await;
        enqueue(&db, 30, "x86_64", 100).await;
        enqueue(&db, 31, "x86_64", 100).await;
        claim_job(&db, 5, &["x86_64".to_string()], &[], 60)
            .await
            .unwrap();
        claim_job(&db, 5, &["x86_64".to_string()], &[], 60)
            .await
            .unwrap();

        // Report only build 30 as active -> 31 was dropped and should requeue.
        let out = heartbeat(&db, 5, &[30], 60, 3).await.unwrap();
        assert_eq!(out.renewed, vec![30]);
        assert_eq!(out.dropped, vec![31]);

        let b31 = Builds::find_by_id(31).one(&db).await.unwrap().unwrap();
        assert_eq!(b31.status, Some(STATUS_ENQUEUED));
        assert_eq!(b31.worker_id, None);
        assert_eq!(b31.attempt_count, 1);
    }

    #[tokio::test]
    async fn requeue_budget_eventually_fails() {
        let db = setup().await;
        enqueue(&db, 40, "x86_64", 100).await;
        claim_job(&db, 9, &["x86_64".to_string()], &[], 60)
            .await
            .unwrap();

        // attempts 0 -> requeue (count 1), claim again, etc.
        for expected in [
            RequeueOutcome::Requeued,
            RequeueOutcome::Requeued,
            RequeueOutcome::Requeued,
            RequeueOutcome::Failed,
        ] {
            // ensure it's active + owned before requeue
            claim_job(&db, 9, &["x86_64".to_string()], &[], 60)
                .await
                .unwrap();
            let observed = Builds::find_by_id(40).one(&db).await.unwrap().unwrap();
            let outcome = requeue_or_fail(&db, &observed, 3).await.unwrap();
            assert_eq!(outcome, expected);
        }
        let b = Builds::find_by_id(40).one(&db).await.unwrap().unwrap();
        assert_eq!(b.status, Some(STATUS_FAILED));
    }

    #[tokio::test]
    async fn reaper_requeues_expired_lease_only() {
        let db = setup().await;
        enqueue(&db, 60, "x86_64", 100).await;
        enqueue(&db, 61, "x86_64", 100).await;
        // Claim both with a 60s lease so lease_expires_at = claim_now + 60.
        claim_job(&db, 3, &["x86_64".to_string()], &[], 60)
            .await
            .unwrap();
        claim_job(&db, 3, &["x86_64".to_string()], &[], 60)
            .await
            .unwrap();

        // "now" far in the future: both leases are expired -> both requeued.
        let far = now_secs() + 10_000;
        let out = reap_expired_builds(&db, far, 3, 100_000).await.unwrap();
        assert_eq!(out.requeued.len(), 2);
        assert!(out.failed.is_empty());
        for id in [60, 61] {
            let b = Builds::find_by_id(id).one(&db).await.unwrap().unwrap();
            assert_eq!(b.status, Some(STATUS_ENQUEUED));
            assert_eq!(b.worker_id, None);
            assert_eq!(b.attempt_count, 1);
        }
    }

    /// A build that completes between the reaper's candidate select and its
    /// per-row write must not be dragged back out of its terminal state.
    #[tokio::test]
    async fn requeue_skips_build_resolved_after_observation() {
        let db = setup().await;
        enqueue(&db, 90, "x86_64", 100).await;
        claim_job(&db, 7, &["x86_64".to_string()], &[], 60)
            .await
            .unwrap();

        // What the reaper would have seen in its candidate select.
        let observed = Builds::find_by_id(90).one(&db).await.unwrap().unwrap();

        // ...meanwhile the owning worker completes the build.
        Builds::update_many()
            .col_expr(builds::Column::Status, STATUS_SUCCESS.into())
            .col_expr(builds::Column::WorkerId, Option::<i32>::None.into())
            .col_expr(builds::Column::LeaseExpiresAt, Option::<i64>::None.into())
            .filter(builds::Column::Id.eq(90))
            .exec(&db)
            .await
            .unwrap();

        // The stale observation must not clobber the SUCCESS row.
        let outcome = requeue_or_fail(&db, &observed, 3).await.unwrap();
        assert_eq!(outcome, RequeueOutcome::Unchanged);

        let b = Builds::find_by_id(90).one(&db).await.unwrap().unwrap();
        assert_eq!(b.status, Some(STATUS_SUCCESS));
        assert_eq!(b.attempt_count, 0);
    }

    /// Same guard, but for a lease renewed by a heartbeat in the gap.
    #[tokio::test]
    async fn requeue_skips_build_whose_lease_was_renewed() {
        let db = setup().await;
        enqueue(&db, 91, "x86_64", 100).await;
        claim_job(&db, 8, &["x86_64".to_string()], &[], 60)
            .await
            .unwrap();
        let observed = Builds::find_by_id(91).one(&db).await.unwrap().unwrap();

        // Worker heartbeats: lease pushed out, build still ACTIVE and owned.
        heartbeat(&db, 8, &[91], 600, 3).await.unwrap();

        let outcome = requeue_or_fail(&db, &observed, 3).await.unwrap();
        assert_eq!(outcome, RequeueOutcome::Unchanged);

        let b = Builds::find_by_id(91).one(&db).await.unwrap().unwrap();
        assert_eq!(b.status, Some(STATUS_ACTIVE));
        assert_eq!(b.worker_id, Some(8));
    }

    #[tokio::test]
    async fn reaper_leaves_fresh_leases_alone() {
        let db = setup().await;
        enqueue(&db, 70, "x86_64", 100).await;
        claim_job(&db, 4, &["x86_64".to_string()], &[], 600)
            .await
            .unwrap();

        // "now" is right after the claim: lease is fresh, build is young.
        let out = reap_expired_builds(&db, now_secs(), 3, 100_000)
            .await
            .unwrap();
        assert!(out.requeued.is_empty());
        assert!(out.failed.is_empty());
        let b = Builds::find_by_id(70).one(&db).await.unwrap().unwrap();
        assert_eq!(b.status, Some(STATUS_ACTIVE));
    }

    #[tokio::test]
    async fn reaper_backstop_fires_despite_fresh_lease() {
        let db = setup().await;
        // start_time = 100 (long ago); claim gives a fresh far-future lease.
        enqueue(&db, 80, "x86_64", 100).await;
        claim_job(&db, 6, &["x86_64".to_string()], &[], 1_000_000)
            .await
            .unwrap();

        // Backstop: max_build_age small so start_time(=claim now) is "too old"
        // relative to a `now` well past it, even though the lease is fresh.
        let now = now_secs() + 10_000;
        let out = reap_expired_builds(&db, now, 3, 1).await.unwrap();
        assert_eq!(out.requeued, vec![80]);
    }

    #[tokio::test]
    async fn reaper_gives_up_after_budget() {
        let db = setup().await;
        enqueue(&db, 90, "x86_64", 100).await;
        claim_job(&db, 2, &["x86_64".to_string()], &[], 60)
            .await
            .unwrap();
        // Pre-exhaust the budget.
        db.execute_unprepared("UPDATE builds SET attempt_count = 3 WHERE id = 90")
            .await
            .unwrap();

        let far = now_secs() + 10_000;
        let out = reap_expired_builds(&db, far, 3, 100_000).await.unwrap();
        assert_eq!(out.failed, vec![90]);
        assert!(out.requeued.is_empty());
        let b = Builds::find_by_id(90).one(&db).await.unwrap().unwrap();
        assert_eq!(b.status, Some(STATUS_FAILED));
    }
}
