//! `SeaORM` Entity for remote build workers.

use sea_orm::entity::prelude::*;
use serde::Serialize;
use utoipa::ToSchema;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, ToSchema)]
#[sea_orm(table_name = "workers")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub name: String,
    /// One of `pending`, `approved`, `revoked`.
    pub status: String,
    /// SHA-256 fingerprint of the worker's self-generated certificate/CSR.
    /// Stable identity used for enrollment and mTLS mapping.
    pub cert_fingerprint: String,
    /// Serial of the CA-signed leaf certificate (set on approval).
    pub cert_serial: Option<String>,
    /// PEM of the CA-signed leaf certificate (set on approval).
    pub signed_cert: Option<String>,
    /// Epoch seconds when the signed certificate expires.
    pub not_after: Option<i64>,
    /// Comma-separated architectures the worker builds natively.
    pub native_arches: String,
    /// Comma-separated architectures the worker can build via emulation.
    pub emulated_arches: String,
    /// Epoch seconds of the last heartbeat/contact.
    pub last_seen: Option<i64>,
    /// Worker software version reported at enrollment/heartbeat.
    pub version: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
