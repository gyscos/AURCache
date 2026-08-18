//! Server-side handling of a worker's build completion: terminal status
//! transitions, version reconciliation, and promotion of dependent builds.
//!
//! This is the Docker-free counterpart of the logic that used to live in the
//! in-process builder (`aurcache-builder`'s `post_build`/`trigger_dependents`).
//! Workers poll for enqueued jobs, so promoting a dependent from
//! `WAITING_FOR_DEPS` to `ENQUEUED` is all that is required to dispatch it.

use aurcache_db::builds;
use aurcache_db::dependencies;
use aurcache_db::helpers::build_enqueue::promote_waiting_build;
use aurcache_db::prelude::{Builds, Dependencies, Packages};
use pacman_mirrors::platforms::Platform;
use sea_orm::ActiveValue::Set;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DbErr, EntityTrait, IntoActiveModel, Order,
    QueryFilter, QueryOrder, QuerySelect, TransactionTrait,
};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

const STATUS_ACTIVE: i32 = 0;
const STATUS_SUCCESS: i32 = 1;
const STATUS_FAILED: i32 = 2;

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

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
    if build.status != Some(STATUS_ACTIVE) {
        return Err(DbErr::Custom(format!("build {build_id} is not active")));
    }
    if build.worker_id != Some(worker_id) {
        return Err(DbErr::Custom(format!(
            "build {build_id} is not owned by worker {worker_id}"
        )));
    }
    Ok(build)
}

/// Record the authoritative built version (extracted from the uploaded package
/// files) on the build and its package.
pub async fn record_built_version<C: ConnectionTrait>(
    db: &C,
    build_id: i32,
    version: &str,
) -> Result<(), DbErr> {
    let build = Builds::find_by_id(build_id)
        .one(db)
        .await?
        .ok_or_else(|| DbErr::Custom(format!("build {build_id} not found")))?;
    let pkg_id = build.pkg_id;
    let mut active = build.into_active_model();
    active.version = Set(version.to_string());
    active.update(db).await?;

    if let Some(pkg) = Packages::find_by_id(pkg_id).one(db).await? {
        let mut pkg = pkg.into_active_model();
        pkg.upstream_version = Set(Some(version.to_string()));
        pkg.update(db).await?;
    }
    Ok(())
}

/// Mark a build (and its package) as successfully built, then promote any
/// dependents whose dependencies are now satisfied.
pub async fn complete_success<C: ConnectionTrait + TransactionTrait>(
    db: &C,
    build_id: i32,
) -> Result<(), DbErr> {
    let build = Builds::find_by_id(build_id)
        .one(db)
        .await?
        .ok_or_else(|| DbErr::Custom(format!("build {build_id} not found")))?;
    let pkg_id = build.pkg_id;
    let platform = build.platform;

    let txn = db.begin().await?;
    let mut b = build.into_active_model();
    b.status = Set(Some(STATUS_SUCCESS));
    b.worker_id = Set(None);
    b.lease_expires_at = Set(None);
    b.end_time = Set(Some(now_secs()));
    b.update(&txn).await?;

    if let Some(pkg) = Packages::find_by_id(pkg_id).one(&txn).await? {
        let mut pkg = pkg.into_active_model();
        pkg.status = Set(STATUS_SUCCESS);
        pkg.out_of_date = Set(i32::from(false));
        pkg.update(&txn).await?;
    }
    txn.commit().await?;

    if let Err(e) = trigger_dependents(db, pkg_id, platform).await {
        tracing::error!("Failed to trigger dependents of package {pkg_id}: {e}");
    }
    Ok(())
}

/// Mark a build (and its package) as failed. Deterministic failures reported by
/// the worker are terminal — no re-enqueue.
pub async fn complete_failure<C: ConnectionTrait + TransactionTrait>(
    db: &C,
    build_id: i32,
) -> Result<(), DbErr> {
    let build = Builds::find_by_id(build_id)
        .one(db)
        .await?
        .ok_or_else(|| DbErr::Custom(format!("build {build_id} not found")))?;
    let pkg_id = build.pkg_id;

    let txn = db.begin().await?;
    let mut b = build.into_active_model();
    b.status = Set(Some(STATUS_FAILED));
    b.worker_id = Set(None);
    b.lease_expires_at = Set(None);
    b.end_time = Set(Some(now_secs()));
    b.update(&txn).await?;

    if let Some(pkg) = Packages::find_by_id(pkg_id).one(&txn).await? {
        let mut pkg = pkg.into_active_model();
        pkg.status = Set(STATUS_FAILED);
        pkg.update(&txn).await?;
    }
    txn.commit().await?;
    Ok(())
}

/// After a successful build, promote any dependent whose dependencies are now
/// all satisfied from `WAITING_FOR_DEPS` to `ENQUEUED` so a worker can claim it.
pub async fn trigger_dependents<C: ConnectionTrait>(
    db: &C,
    pkg_id: i32,
    platform: Platform,
) -> Result<(), DbErr> {
    let deps_by_dependent = load_dependencies_for_dependents_of(db, pkg_id).await?;
    for (dependent_id, all_deps) in &deps_by_dependent {
        if dependencies_ready(db, all_deps, platform).await? {
            promote_dependent(db, *dependent_id, platform).await?;
        }
    }
    Ok(())
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
        if !dependency_satisfied(db, dep.dependee_id, platform, &dep.version_constraint).await? {
            return Ok(false);
        }
    }
    Ok(true)
}

async fn dependency_satisfied<C: ConnectionTrait>(
    db: &C,
    dependee_id: i32,
    platform: Platform,
    constraint: &str,
) -> Result<bool, DbErr> {
    let latest_success: Option<String> = Builds::find()
        .select_only()
        .column(builds::Column::Version)
        .filter(builds::Column::PkgId.eq(dependee_id))
        .filter(builds::Column::Platform.eq(platform.as_str()))
        .filter(builds::Column::Status.eq(Some(STATUS_SUCCESS)))
        .order_by(builds::Column::EndTime, Order::Desc)
        .limit(1)
        .into_tuple()
        .one(db)
        .await?;
    Ok(latest_success
        .map(|v| crate::pkg::satisfies_constraint(&v, constraint))
        .unwrap_or(false))
}

async fn promote_dependent<C: ConnectionTrait>(
    db: &C,
    dependent_id: i32,
    platform: Platform,
) -> Result<(), DbErr> {
    let Some(pkg) = Packages::find_by_id(dependent_id).one(db).await? else {
        return Ok(());
    };
    if !pkg.platforms.trim().is_empty()
        && !Platform::parse_many(&pkg.platforms).any(|r| r.is_ok_and(|p| p == platform))
    {
        return Ok(());
    }
    if let Some(promoted) = promote_waiting_build(db, pkg.id, platform).await? {
        tracing::info!(
            "Promoted build #{} for dependent '{}' on {} from waiting to enqueued",
            promoted.id,
            pkg.name,
            platform
        );
    }
    Ok(())
}

// (module functions above)

#[cfg(test)]
mod tests {
    use super::*;
    use aurcache_db::migration::Migrator;
    use sea_orm::{ConnectionTrait as _, Database, DatabaseConnection};
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

    #[tokio::test]
    async fn owned_active_guard() {
        let db = setup().await;
        pkg(&db, 1).await;
        build(&db, 10, 1, STATUS_ACTIVE, "5").await;
        assert!(assert_owned_active(&db, 5, 10).await.is_ok());
        assert!(assert_owned_active(&db, 6, 10).await.is_err());
    }

    #[tokio::test]
    async fn success_marks_terminal_and_clears_lease() {
        let db = setup().await;
        pkg(&db, 1).await;
        build(&db, 10, 1, STATUS_ACTIVE, "5").await;
        record_built_version(&db, 10, "2.0-1").await.unwrap();
        complete_success(&db, 10).await.unwrap();
        let b = Builds::find_by_id(10).one(&db).await.unwrap().unwrap();
        assert_eq!(b.status, Some(STATUS_SUCCESS));
        assert_eq!(b.worker_id, None);
        assert_eq!(b.version, "2.0-1");
        assert!(b.end_time.is_some());
    }

    #[tokio::test]
    async fn failure_is_terminal_not_requeued() {
        let db = setup().await;
        pkg(&db, 1).await;
        build(&db, 10, 1, STATUS_ACTIVE, "5").await;
        complete_failure(&db, 10).await.unwrap();
        let b = Builds::find_by_id(10).one(&db).await.unwrap().unwrap();
        assert_eq!(b.status, Some(STATUS_FAILED));
        assert_eq!(b.attempt_count, 0);
    }
}
