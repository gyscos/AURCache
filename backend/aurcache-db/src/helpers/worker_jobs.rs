//! Database helpers for the remote-worker job lifecycle: atomic claim, lease
//! renewal via heartbeat, and requeue/fail of dropped builds.
//!
//! Build status integers mirror `aurcache_types::builder::BuildStates`
//! (0=active, 1=success, 2=failed, 3=enqueued, 4=waiting-for-deps); the db
//! crate keeps its own copy to avoid a dependency on the types crate here.

use crate::builds;
use crate::prelude::Builds;
use sea_orm::ActiveValue::Set;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DbErr, EntityTrait, IntoActiveModel,
    QueryFilter, QueryOrder, QuerySelect,
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
            .col_expr(builds::Column::LeaseExpiresAt, (now + lease_ttl_secs).into())
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
    let owned: Vec<i32> = Builds::find()
        .select_only()
        .column(builds::Column::Id)
        .filter(builds::Column::WorkerId.eq(worker_id))
        .filter(builds::Column::Status.eq(STATUS_ACTIVE))
        .into_tuple()
        .all(db)
        .await?;

    for id in owned {
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
        } else {
            requeue_or_fail(db, id, max_attempts).await?;
            outcome.dropped.push(id);
        }
    }
    Ok(outcome)
}

/// Requeue a build after its worker was lost, or fail it once the retry budget
/// is exhausted. Transitions `ACTIVE -> ENQUEUED` (clearing the lease/owner and
/// bumping `attempt_count`), or `-> FAILED` after `max_attempts` requeues.
///
/// Returns `true` if the build was requeued, `false` if it was failed.
pub async fn requeue_or_fail<C: ConnectionTrait>(
    db: &C,
    build_id: i32,
    max_attempts: i32,
) -> Result<bool, DbErr> {
    let Some(build) = Builds::find_by_id(build_id).one(db).await? else {
        return Ok(false);
    };
    let attempts = build.attempt_count;
    let mut active = build.into_active_model();
    if attempts < max_attempts {
        active.status = Set(Some(STATUS_ENQUEUED));
        active.worker_id = Set(None);
        active.lease_expires_at = Set(None);
        active.attempt_count = Set(attempts + 1);
        active.update(db).await?;
        Ok(true)
    } else {
        active.status = Set(Some(STATUS_FAILED));
        active.worker_id = Set(None);
        active.lease_expires_at = Set(None);
        active.end_time = Set(Some(now_secs()));
        active.update(db).await?;
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::Migrator;
    use sea_orm::{ConnectionTrait as _, Database, DatabaseConnection};
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
        let claimed = claim_job(&db, 1, &["x86_64".to_string()], &["aarch64".to_string()], 60)
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
        let claimed = claim_job(&db, 1, &["x86_64".to_string()], &["aarch64".to_string()], 60)
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
        for expected_requeue in [true, true, true, false] {
            // ensure it's active + owned before requeue
            claim_job(&db, 9, &["x86_64".to_string()], &[], 60).await.unwrap();
            let requeued = requeue_or_fail(&db, 40, 3).await.unwrap();
            assert_eq!(requeued, expected_requeue);
        }
        let b = Builds::find_by_id(40).one(&db).await.unwrap().unwrap();
        assert_eq!(b.status, Some(STATUS_FAILED));
    }
}
