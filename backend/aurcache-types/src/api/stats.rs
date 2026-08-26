use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Deserialize, ToSchema, Serialize)]
#[cfg_attr(feature = "db", derive(sea_orm::FromQueryResult))]
pub struct ListStats {
    pub total_builds: u32,
    pub successful_builds: u32,
    pub failed_builds: u32,
    pub avg_build_time: u32,
    pub repo_size: u64,
    pub total_packages: u32,

    pub total_build_trend: f32,
    pub avg_build_time_trend: f32,
}

#[derive(Deserialize, ToSchema, Serialize)]
#[cfg_attr(feature = "db", derive(sea_orm::FromQueryResult))]
pub struct GraphDataPoint {
    pub month: i32,
    pub year: i32,
    pub count: i32,
}

#[derive(Deserialize, ToSchema, Serialize)]
#[cfg_attr(feature = "db", derive(sea_orm::FromQueryResult))]
pub struct UserInfo {
    pub username: Option<String>,
    pub has_api_token: bool,
}
