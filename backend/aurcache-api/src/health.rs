use rocket::http::Status;
use rocket::{State, get};
use sea_orm::DatabaseConnection;
use utoipa::OpenApi;

#[derive(OpenApi)]
#[openapi(paths(health))]
pub struct HealthApi;

#[utoipa::path(
    responses(
            (status = 200, description = "Internal Healthcheck"),
            (status = 500, description = "Database unreachable"),
    )
)]
#[get("/health")]
pub async fn health(db: &State<DatabaseConnection>) -> Result<(), Status> {
    // `{:#}` rather than `{}`: the outer message alone is usually just
    // "connection error", and the cause underneath it is the part worth
    // reading. It goes to the logs, not the wire: a health poller only needs
    // the status code, and the success case must stay an empty 200 (the
    // client distinguishes the API from the web UI by its empty body).
    if let Err(e) = check_health(db).await {
        tracing::error!("health check failed: {e:#}");
        return Err(Status::InternalServerError);
    }
    Ok(())
}

async fn check_health(db: &DatabaseConnection) -> anyhow::Result<()> {
    // Check database connection.
    db.ping().await?;

    Ok(())
}
