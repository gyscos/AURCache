use crate::models::authenticated::Authenticated;
use crate::utils::error::{ApiError, err};
use aurcache_activitylog::activity_utils::{Activity, ActivityLog};
use rocket::http::Status;
use rocket::serde::json::Json;
use rocket::{State, get};
use utoipa::OpenApi;

#[derive(OpenApi)]
#[openapi(paths(activity))]
pub struct ActivityApi;

#[utoipa::path(
    responses(
            (status = 200, description = "Get last n Activity entries", body = [Vec<Activity>]),
    )
)]
#[get("/activity?<limit>")]
pub async fn activity(
    _a: Authenticated,
    al: &State<ActivityLog>,
    limit: Option<u64>,
) -> Result<Json<Vec<Activity>>, ApiError> {
    let activities = al
        .list(limit)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;
    Ok(Json(activities))
}
