use crate::models::authenticated::Authenticated;
use crate::utils::error::{ApiError, err};
use crate::utils::pagination::clamp_limit;
use aurcache_activitylog::activity_utils::{ActivityPage, ActivityStore, LogFilter, Severity};
use rocket::http::Status;
use rocket::serde::json::Json;
use rocket::{State, get};
use utoipa::OpenApi;

#[derive(OpenApi)]
#[openapi(paths(activity))]
pub struct ActivityApi;

#[utoipa::path(
    responses(
            (status = 200, description = "One page of the activity log, newest first", body = ActivityPage),
    )
)]
#[get("/activity?<limit>&<offset>&<severity>&<since_boot>")]
pub async fn activity(
    _a: Authenticated,
    db: &State<sea_orm::DatabaseConnection>,
    limit: Option<u64>,
    offset: Option<u64>,
    severity: Option<String>,
    since_boot: Option<bool>,
) -> Result<Json<ActivityPage>, ApiError> {
    let limit = clamp_limit(limit);
    // A severity nobody recognises narrows nothing, the way an unreadable
    // filter in a URL degrades to "any" everywhere else in this app.
    let filter = LogFilter {
        severity: severity.as_deref().and_then(Severity::from_slug),
        since_boot: since_boot.unwrap_or(false),
    };
    let page = ActivityStore::new(db.inner().clone())
        .page(limit, offset.unwrap_or(0), &filter)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;
    Ok(Json(page))
}
