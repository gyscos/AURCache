use crate::api::waiting::WaitingReason;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Deserialize, ToSchema, Serialize)]
#[cfg_attr(feature = "db", derive(sea_orm::FromQueryResult))]
pub struct BuildSummary {
    pub id: i32,
    pub pkg_id: i32,
    pub pkg_name: String,
    pub version: String,
    pub status: i32,
    pub start_time: Option<i64>,
    pub end_time: Option<i64>,
    pub platform: String,
    /// Why this build is stuck, when it is `ENQUEUED` and *no* approved worker
    /// can currently take it. `None` for everything else, including a build
    /// merely waiting behind a busy worker — see
    /// `aurcache_db::helpers::worker_jobs::waiting_reasons`.
    #[cfg_attr(feature = "db", sea_orm(skip))]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting_reason: Option<WaitingReason>,
}
