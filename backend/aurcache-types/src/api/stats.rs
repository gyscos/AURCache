use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Deserialize, ToSchema, Serialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "db", derive(sea_orm::FromQueryResult))]
pub struct ListStats {
    pub total_builds: u32,
    pub successful_builds: u32,
    pub failed_builds: u32,

    /// The same three over [`RECENT_DAYS`], by start time.
    ///
    /// A server that has been running for a year is described badly by its
    /// lifetime numbers: a run of failures this week disappears into thousands
    /// of old successes, which is exactly when you want to notice it.
    pub recent_builds: u32,
    pub recent_successful: u32,
    pub recent_failed: u32,

    pub avg_build_time: u32,
    pub repo_size: u64,

    /// Packages somebody asked for by name.
    ///
    /// Renamed from `total_packages`, which never counted the total: it has
    /// always excluded dependencies, and the name said otherwise.
    pub requested_packages: u32,
    /// Packages present only because something else needs them.
    pub dependency_packages: u32,

    pub total_build_trend: f32,
    pub avg_build_time_trend: f32,
}

/// The window the `recent_*` counts cover.
///
/// A week: long enough to survive a quiet weekend, short enough that a problem
/// starting on Monday is still visible on Friday.
pub const RECENT_DAYS: i64 = 7;

#[derive(Deserialize, ToSchema, Serialize, Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "db", derive(sea_orm::FromQueryResult))]
pub struct GraphDataPoint {
    pub month: i32,
    pub year: i32,
    /// Every build started that month, whatever became of it.
    pub count: i32,
    /// How many of them succeeded. Plotted against `count`, the gap between
    /// the two lines is the failures — which is the thing worth seeing, and
    /// reads better than a failure count on its own.
    pub successful: i32,
}

#[derive(Deserialize, ToSchema, Serialize)]
#[cfg_attr(feature = "db", derive(sea_orm::FromQueryResult))]
pub struct UserInfo {
    pub username: Option<String>,
    pub has_api_token: bool,
}
