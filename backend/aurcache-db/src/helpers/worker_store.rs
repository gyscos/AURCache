//! Database helpers for remote build workers: enrollment, approval, revocation,
//! and fingerprint lookup used by the mTLS auth guard.

use crate::prelude::Workers;
use crate::workers;
use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::{OnConflict, Query};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DbErr, EntityTrait, QueryFilter, QueryOrder,
};
use std::time::{SystemTime, UNIX_EPOCH};

/// Worker status string constants (mirror of `aurcache_types::worker::WorkerStatus`,
/// duplicated here so the db crate stays free of a types dependency cycle).
pub const STATUS_PENDING: &str = "pending";
pub const STATUS_APPROVED: &str = "approved";
pub const STATUS_REVOKED: &str = "revoked";

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Register a worker on first contact, or return the existing row when a worker
/// with the same certificate fingerprint re-enrolls (idempotent).
///
/// A freshly registered worker starts in the `pending` state and must be
/// approved before it can claim jobs. Re-enrollment refreshes the advertised
/// arches / version / `last_seen` but never changes an existing approval state.
pub async fn register_worker<C: ConnectionTrait>(
    db: &C,
    name: &str,
    fingerprint: &str,
    native_arches: &str,
    emulated_arches: &str,
    version: &str,
) -> Result<workers::Model, DbErr> {
    let now = now_secs();
    // Insert-if-absent keyed on the unique cert_fingerprint. On conflict we keep
    // the existing row (status/approval preserved) and just refresh liveness fields.
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
        ])
        .values([
            name.into(),
            STATUS_PENDING.into(),
            fingerprint.into(),
            native_arches.into(),
            emulated_arches.into(),
            now.into(),
            version.into(),
        ])
        .map_err(|e| DbErr::Custom(e.to_string()))?
        .on_conflict(
            OnConflict::column(workers::Column::CertFingerprint)
                .update_columns([
                    workers::Column::NativeArches,
                    workers::Column::EmulatedArches,
                    workers::Column::LastSeen,
                    workers::Column::Version,
                ])
                .to_owned(),
        )
        .to_owned();

    db.execute(db.get_database_backend().build(&insert)).await?;

    find_worker_by_fingerprint(db, fingerprint)
        .await?
        .ok_or_else(|| DbErr::Custom("worker vanished after registration".to_string()))
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

/// Store the CA-signed leaf certificate for a worker (done at registration time,
/// before approval). The certificate chains to the CA but the worker is still
/// refused at the auth guard until its status becomes `approved`.
pub async fn store_signed_cert<C: ConnectionTrait>(
    db: &C,
    id: i32,
    signed_cert: &str,
    serial: &str,
    not_after: i64,
) -> Result<workers::Model, DbErr> {
    let worker = Workers::find_by_id(id)
        .one(db)
        .await?
        .ok_or_else(|| DbErr::Custom(format!("worker {id} not found")))?;
    let mut active: workers::ActiveModel = worker.into();
    active.signed_cert = Set(Some(signed_cert.to_string()));
    active.cert_serial = Set(Some(serial.to_string()));
    active.not_after = Set(Some(not_after));
    active.update(db).await
}

/// Approve a worker so it may claim jobs. The signed certificate is issued at
/// registration time; approval only flips the gating status.
pub async fn approve_worker<C: ConnectionTrait>(db: &C, id: i32) -> Result<workers::Model, DbErr> {
    let worker = Workers::find_by_id(id)
        .one(db)
        .await?
        .ok_or_else(|| DbErr::Custom(format!("worker {id} not found")))?;
    let mut active: workers::ActiveModel = worker.into();
    active.status = Set(STATUS_APPROVED.to_string());
    active.update(db).await
}

/// Revoke a worker: it is immediately refused at the auth guard.
pub async fn revoke_worker<C: ConnectionTrait>(db: &C, id: i32) -> Result<workers::Model, DbErr> {
    let worker = Workers::find_by_id(id)
        .one(db)
        .await?
        .ok_or_else(|| DbErr::Custom(format!("worker {id} not found")))?;
    let mut active: workers::ActiveModel = worker.into();
    active.status = Set(STATUS_REVOKED.to_string());
    active.update(db).await
}

/// List all workers, most recently seen first.
pub async fn list_workers<C: ConnectionTrait>(db: &C) -> Result<Vec<workers::Model>, DbErr> {
    Workers::find()
        .order_by_desc(workers::Column::LastSeen)
        .all(db)
        .await
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

    #[tokio::test]
    async fn register_is_idempotent_by_fingerprint() {
        let db = setup().await;
        let a = register_worker(&db, "w1", "fp-1", "x86_64", "", "0.1.0")
            .await
            .unwrap();
        let b = register_worker(&db, "w1", "fp-1", "x86_64", "aarch64", "0.2.0")
            .await
            .unwrap();
        assert_eq!(a.id, b.id);
        assert_eq!(b.status, STATUS_PENDING);
        assert_eq!(b.emulated_arches, "aarch64");
        assert_eq!(b.version.as_deref(), Some("0.2.0"));
        assert_eq!(list_workers(&db).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn approve_then_revoke_transitions() {
        let db = setup().await;
        let w = register_worker(&db, "w1", "fp-1", "x86_64", "", "0.1.0")
            .await
            .unwrap();
        store_signed_cert(&db, w.id, "CERTPEM", "AB12", 9999)
            .await
            .unwrap();
        let approved = approve_worker(&db, w.id).await.unwrap();
        assert_eq!(approved.status, STATUS_APPROVED);
        assert_eq!(approved.signed_cert.as_deref(), Some("CERTPEM"));
        assert_eq!(approved.not_after, Some(9999));

        // Re-registration keeps the approval.
        let re = register_worker(&db, "w1", "fp-1", "x86_64", "", "0.3.0")
            .await
            .unwrap();
        assert_eq!(re.status, STATUS_APPROVED);

        let revoked = revoke_worker(&db, w.id).await.unwrap();
        assert_eq!(revoked.status, STATUS_REVOKED);
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
        register_worker(&db, "w1", "fp-x", "x86_64", "", "0.1.0")
            .await
            .unwrap();
        assert!(
            find_worker_by_fingerprint(&db, "fp-x")
                .await
                .unwrap()
                .is_some()
        );
    }
}
