//! Database helpers for remote build workers: enrollment, approval, revocation,
//! and fingerprint lookup used by the mTLS auth guard.

use crate::helpers::time::now_secs;
use crate::prelude::Workers;
use crate::workers;
use aurcache_common::api::worker::ApprovalStatus;
use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::{OnConflict, Query};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DbErr, EntityTrait, QueryFilter, QueryOrder,
};

/// Everything a worker reports about itself at registration.
///
/// Grouped into a struct because *all* of it is refreshed on every
/// re-registration: the worker's environment is the source of truth, and any of
/// it can have changed since the machine last booted.
pub struct WorkerRegistration<'a> {
    pub name: &'a str,
    pub fingerprint: &'a str,
    pub native_arches: &'a str,
    pub emulated_arches: &'a str,
    pub version: &'a str,
    /// Build strategy the worker runs (`chroot`, `docker`); empty from a worker
    /// predating the field, which is stored as `None`.
    pub kind: &'a str,
    /// Comma-separated exact pkgbase names this worker is provisioned for.
    pub package_affinity: &'a str,
    /// Scheduling preference; higher wins.
    pub priority: i32,
    /// Maximum concurrent builds the worker will run.
    pub concurrency: i32,
    /// JSON array of the settings the worker declares it accepts, exactly as it
    /// sent them. `None` from a worker version that declares none, which
    /// replaces any declaration stored for it: a worker that stopped declaring
    /// a setting no longer accepts it.
    pub settings_declaration: Option<&'a str>,
}

/// Register a worker on first contact, or refresh the existing row when a worker
/// with the same certificate fingerprint re-enrolls (idempotent).
///
/// A freshly registered worker starts in the `pending` state and must be
/// approved before it can claim jobs. Re-registration refreshes every reported
/// field — including `name`, which the operator may have changed — but never
/// touches the approval state: a revoked worker that comes back stays revoked.
pub async fn register_worker<C: ConnectionTrait>(
    db: &C,
    reg: &WorkerRegistration<'_>,
) -> Result<workers::Model, DbErr> {
    let now = now_secs();
    // Empty means the worker predates the field, which is "unknown" rather than
    // any particular strategy -- so it is stored as NULL and shown as unknown,
    // not guessed at.
    let kind: Option<&str> = Some(reg.kind).filter(|k| !k.is_empty());
    // Insert-if-absent keyed on the unique cert_fingerprint. On conflict we keep
    // the existing row (status/approval preserved) and refresh what the worker
    // reported.
    let insert = Query::insert()
        .into_table(workers::Entity)
        .columns([
            workers::Column::Name,
            workers::Column::Status,
            workers::Column::CertFingerprint,
            workers::Column::NativeArches,
            workers::Column::EmulatedArches,
            workers::Column::LastSeen,
            workers::Column::Version,
            workers::Column::Kind,
            workers::Column::PackageAffinity,
            workers::Column::Priority,
            workers::Column::Concurrency,
            workers::Column::SettingsDeclaration,
        ])
        .values([
            reg.name.into(),
            ApprovalStatus::Pending.into(),
            reg.fingerprint.into(),
            reg.native_arches.into(),
            reg.emulated_arches.into(),
            now.into(),
            reg.version.into(),
            kind.into(),
            reg.package_affinity.into(),
            reg.priority.into(),
            reg.concurrency.into(),
            reg.settings_declaration.into(),
        ])
        .map_err(|e| DbErr::Custom(e.to_string()))?
        .on_conflict(
            OnConflict::column(workers::Column::CertFingerprint)
                .update_columns([
                    workers::Column::Name,
                    workers::Column::NativeArches,
                    workers::Column::EmulatedArches,
                    workers::Column::LastSeen,
                    workers::Column::Version,
                    workers::Column::Kind,
                    workers::Column::PackageAffinity,
                    workers::Column::Priority,
                    workers::Column::Concurrency,
                    workers::Column::SettingsDeclaration,
                ])
                .to_owned(),
        )
        .to_owned();

    db.execute(&insert).await?;

    find_worker_by_fingerprint(db, reg.fingerprint)
        .await?
        .ok_or_else(|| DbErr::Custom("worker vanished after registration".to_string()))
}

/// Look up a worker by id.
pub async fn find_worker<C: ConnectionTrait>(
    db: &C,
    id: i32,
) -> Result<Option<workers::Model>, DbErr> {
    Workers::find_by_id(id).one(db).await
}

/// Look up a worker by its public-key fingerprint (used by the mTLS guard).
pub async fn find_worker_by_fingerprint<C: ConnectionTrait>(
    db: &C,
    fingerprint: &str,
) -> Result<Option<workers::Model>, DbErr> {
    Workers::find()
        .filter(workers::Column::CertFingerprint.eq(fingerprint))
        .one(db)
        .await
}

/// Load a worker as an `ActiveModel` ready to be mutated and updated.
async fn load_for_update<C: ConnectionTrait>(
    db: &C,
    id: i32,
) -> Result<workers::ActiveModel, DbErr> {
    Workers::find_by_id(id)
        .one(db)
        .await?
        .map(Into::into)
        .ok_or_else(|| DbErr::Custom(format!("worker {id} not found")))
}

async fn set_status<C: ConnectionTrait>(
    db: &C,
    id: i32,
    status: ApprovalStatus,
) -> Result<workers::Model, DbErr> {
    let mut active = load_for_update(db, id).await?;
    active.status = Set(status);
    active.update(db).await
}

/// Store the CA-signed leaf certificate for a worker (done at registration time,
/// before approval). The certificate chains to the CA but the worker is still
/// refused at the auth guard until its status becomes `approved`.
pub async fn store_signed_cert<C: ConnectionTrait>(
    db: &C,
    id: i32,
    signed_cert: &str,
    not_after: i64,
) -> Result<workers::Model, DbErr> {
    let mut active = load_for_update(db, id).await?;
    active.signed_cert = Set(Some(signed_cert.to_string()));
    active.not_after = Set(Some(not_after));
    active.update(db).await
}

/// Approve a worker so it may claim jobs. The signed certificate is issued at
/// registration time; approval only flips the gating status.
pub async fn approve_worker<C: ConnectionTrait>(db: &C, id: i32) -> Result<workers::Model, DbErr> {
    set_status(db, id, ApprovalStatus::Approved).await
}

/// Revoke a worker: it is immediately refused at the auth guard, its package
/// affinity reservations are released (they only count *approved* workers), and
/// any build it still holds is requeued for someone else.
///
/// Revoking is also how a machine is retired — worker rows are never deleted, so
/// build history keeps resolving to the machine that produced it. Re-approving
/// is the way back.
pub async fn revoke_worker<C: ConnectionTrait>(
    db: &C,
    id: i32,
    max_attempts: i32,
) -> Result<workers::Model, DbErr> {
    let worker = set_status(db, id, ApprovalStatus::Revoked).await?;
    crate::helpers::worker_jobs::requeue_worker_builds(db, id, max_attempts).await?;
    Ok(worker)
}

/// List all workers in name order.
///
/// Not by `last_seen`: that is rewritten on every heartbeat, so a "most
/// recently seen" order shuffles the list every few seconds. Name order keeps
/// the fleet in the same place between polls; liveness is a column, not the
/// sort.
pub async fn list_workers<C: ConnectionTrait>(db: &C) -> Result<Vec<workers::Model>, DbErr> {
    Workers::find()
        .order_by_asc(workers::Column::Name)
        .order_by_asc(workers::Column::Id)
        .all(db)
        .await
}

/// Store what a worker reported its settings resolved to.
///
/// Kept whole, as the JSON the worker sent: the server renders this and never
/// reasons about it, so parsing it here would only add a way for a newer
/// worker's report to be lost on the way to the page that shows it.
pub async fn store_effective_config<C: ConnectionTrait>(
    db: &C,
    id: i32,
    effective: &str,
) -> Result<(), DbErr> {
    let Some(worker) = Workers::find_by_id(id).one(db).await? else {
        return Ok(());
    };
    let mut active: workers::ActiveModel = worker.into();
    active.effective_config = Set(Some(effective.to_string()));
    active.update(db).await?;
    Ok(())
}

/// Update a worker's `last_seen` (and optionally its reported version).
pub async fn touch_last_seen<C: ConnectionTrait>(
    db: &C,
    id: i32,
    version: Option<&str>,
) -> Result<(), DbErr> {
    let Some(worker) = Workers::find_by_id(id).one(db).await? else {
        return Ok(());
    };
    let mut active: workers::ActiveModel = worker.into();
    active.last_seen = Set(Some(now_secs()));
    if let Some(v) = version {
        active.version = Set(Some(v.to_string()));
    }
    active.update(db).await?;
    Ok(())
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

    /// A minimal registration; tests override the fields they care about.
    fn reg<'a>(name: &'a str, fingerprint: &'a str) -> WorkerRegistration<'a> {
        WorkerRegistration {
            name,
            fingerprint,
            native_arches: "x86_64",
            emulated_arches: "",
            version: "0.1.0",
            kind: "chroot",
            package_affinity: "",
            priority: 0,
            concurrency: 1,
            settings_declaration: None,
        }
    }

    /// A worker's declaration is stored verbatim and replaced wholesale when it
    /// registers again: the worker's own code is the source of truth for what
    /// it accepts, and an upgrade that drops a setting must not leave the
    /// server offering it.
    #[tokio::test]
    async fn registration_replaces_the_declaration() {
        let db = setup().await;
        let first = r#"[{"key":"concurrency"}]"#;
        let w = register_worker(
            &db,
            &WorkerRegistration {
                settings_declaration: Some(first),
                ..reg("w1", "fp-1")
            },
        )
        .await
        .unwrap();
        assert_eq!(w.settings_declaration.as_deref(), Some(first));
        assert!(w.effective_config.is_none());

        let second = r#"[{"key":"build_timeout"}]"#;
        let w = register_worker(
            &db,
            &WorkerRegistration {
                settings_declaration: Some(second),
                ..reg("w1", "fp-1")
            },
        )
        .await
        .unwrap();
        assert_eq!(w.settings_declaration.as_deref(), Some(second));
    }

    /// The effective configuration arrives on the heartbeat, so it is stored
    /// apart from the declaration and survives the next registration.
    #[tokio::test]
    async fn effective_config_is_stored_and_outlives_registration() {
        let db = setup().await;
        let w = register_worker(
            &db,
            &WorkerRegistration {
                settings_declaration: Some("[]"),
                ..reg("w1", "fp-1")
            },
        )
        .await
        .unwrap();

        let reported = r#"{"settings":{"concurrency":{"value":"2"}}}"#;
        store_effective_config(&db, w.id, reported).await.unwrap();

        let w = register_worker(
            &db,
            &WorkerRegistration {
                settings_declaration: Some("[]"),
                ..reg("w1", "fp-1")
            },
        )
        .await
        .unwrap();
        assert_eq!(w.effective_config.as_deref(), Some(reported));
    }

    /// The kind is stored as reported, and re-registration updates it: a
    /// machine switched from the container builder to the chroot one is the
    /// same worker, and the page must not keep showing the old strategy.
    #[tokio::test]
    async fn registration_records_and_refreshes_the_kind() {
        let db = setup().await;
        let w = register_worker(&db, &reg("w1", "fp-1")).await.unwrap();
        assert_eq!(w.kind.as_deref(), Some("chroot"));

        let w = register_worker(
            &db,
            &WorkerRegistration {
                kind: "docker",
                ..reg("w1", "fp-1")
            },
        )
        .await
        .unwrap();
        assert_eq!(w.kind.as_deref(), Some("docker"));
    }

    /// A worker predating the field reports nothing, which is "unknown" rather
    /// than any particular strategy -- stored as NULL so the page can say so
    /// instead of guessing at the default.
    #[tokio::test]
    async fn an_unreported_kind_is_stored_as_unknown() {
        let db = setup().await;
        let w = register_worker(
            &db,
            &WorkerRegistration {
                kind: "",
                ..reg("w1", "fp-1")
            },
        )
        .await
        .unwrap();
        assert!(w.kind.is_none());
    }

    #[tokio::test]
    async fn register_is_idempotent_by_fingerprint() {
        let db = setup().await;
        let a = register_worker(&db, &reg("w1", "fp-1")).await.unwrap();
        let b = register_worker(
            &db,
            &WorkerRegistration {
                emulated_arches: "aarch64",
                version: "0.2.0",
                ..reg("w1", "fp-1")
            },
        )
        .await
        .unwrap();
        assert_eq!(a.id, b.id);
        assert_eq!(b.status, ApprovalStatus::Pending);
        assert_eq!(b.emulated_arches, "aarch64");
        assert_eq!(b.version.as_deref(), Some("0.2.0"));
        assert_eq!(list_workers(&db).await.unwrap().len(), 1);
    }

    /// The worker's environment is the source of truth for its configuration,
    /// so a restart with edited settings must be reflected server-side —
    /// including the name, which is easy to forget in the conflict clause.
    #[tokio::test]
    async fn reregistration_refreshes_all_reported_config() {
        let db = setup().await;
        let w = register_worker(&db, &reg("old-name", "fp-1"))
            .await
            .unwrap();
        approve_worker(&db, w.id).await.unwrap();

        let updated = register_worker(
            &db,
            &WorkerRegistration {
                native_arches: "aarch64",
                emulated_arches: "armv7h",
                version: "9.9.9",
                package_affinity: "unreal-engine",
                priority: 10,
                concurrency: 8,
                ..reg("new-name", "fp-1")
            },
        )
        .await
        .unwrap();

        assert_eq!(updated.id, w.id);
        assert_eq!(updated.name, "new-name");
        assert_eq!(updated.native_arches, "aarch64");
        assert_eq!(updated.emulated_arches, "armv7h");
        assert_eq!(updated.package_affinity, "unreal-engine");
        assert_eq!(updated.priority, 10);
        assert_eq!(updated.concurrency, 8);
        // Refreshing configuration must not disturb the approval decision.
        assert_eq!(updated.status, ApprovalStatus::Approved);
    }

    /// Re-registering is how a returning machine announces itself, so it must
    /// never launder away a revoke.
    #[tokio::test]
    async fn reregistration_does_not_resurrect_a_revoked_worker() {
        let db = setup().await;
        let w = register_worker(&db, &reg("w1", "fp-1")).await.unwrap();
        approve_worker(&db, w.id).await.unwrap();
        revoke_worker(&db, w.id, 3).await.unwrap();

        let back = register_worker(&db, &reg("w1", "fp-1")).await.unwrap();
        assert_eq!(back.status, ApprovalStatus::Revoked);
        // ...but its liveness is refreshed, so the UI can show it checked in.
        assert!(back.last_seen.is_some());
    }

    #[tokio::test]
    async fn approve_then_revoke_transitions() {
        let db = setup().await;
        let w = register_worker(&db, &reg("w1", "fp-1")).await.unwrap();
        store_signed_cert(&db, w.id, "CERTPEM", 9999).await.unwrap();
        let approved = approve_worker(&db, w.id).await.unwrap();
        assert_eq!(approved.status, ApprovalStatus::Approved);
        assert_eq!(approved.signed_cert.as_deref(), Some("CERTPEM"));
        assert_eq!(approved.not_after, Some(9999));

        // Re-registration keeps the approval.
        let re = register_worker(
            &db,
            &WorkerRegistration {
                version: "0.3.0",
                ..reg("w1", "fp-1")
            },
        )
        .await
        .unwrap();
        assert_eq!(re.status, ApprovalStatus::Approved);

        let revoked = revoke_worker(&db, w.id, 3).await.unwrap();
        assert_eq!(revoked.status, ApprovalStatus::Revoked);
    }

    /// Revoking must release the worker's grip on work in flight, not leave it
    /// stranded until the lease reaper notices — the worker is refused from this
    /// moment and can never report those builds complete.
    #[tokio::test]
    async fn revoke_requeues_builds_the_worker_still_holds() {
        use crate::helpers::worker_jobs::{STATUS_ACTIVE, STATUS_ENQUEUED};

        let db = setup().await;
        let w = register_worker(&db, &reg("w1", "fp-1")).await.unwrap();
        approve_worker(&db, w.id).await.unwrap();

        db.execute_unprepared("INSERT INTO packages (id, name) VALUES (1, 'p1')")
            .await
            .unwrap();
        db.execute_unprepared(&format!(
            "INSERT INTO builds (id, pkg_id, status, platform, version, attempt_count, worker_id) \
             VALUES (1, 1, {STATUS_ACTIVE}, 'x86_64', '1.0', 0, {})",
            w.id
        ))
        .await
        .unwrap();

        revoke_worker(&db, w.id, 3).await.unwrap();

        let row = db
            .query_one_raw(sea_orm::Statement::from_string(
                db.get_database_backend(),
                "SELECT status, worker_id FROM builds WHERE id = 1".to_string(),
            ))
            .await
            .unwrap()
            .expect("build exists");
        let status: i32 = row.try_get("", "status").unwrap();
        let worker_id: Option<i32> = row.try_get("", "worker_id").unwrap();
        assert_eq!(status, STATUS_ENQUEUED);
        assert_eq!(worker_id, None);
    }

    #[tokio::test]
    async fn lookup_by_fingerprint() {
        let db = setup().await;
        assert!(
            find_worker_by_fingerprint(&db, "missing")
                .await
                .unwrap()
                .is_none()
        );
        register_worker(&db, &reg("w1", "fp-x")).await.unwrap();
        assert!(
            find_worker_by_fingerprint(&db, "fp-x")
                .await
                .unwrap()
                .is_some()
        );
    }
}
