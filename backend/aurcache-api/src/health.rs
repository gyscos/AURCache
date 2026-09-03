use rocket::{State, get};
use sea_orm::DatabaseConnection;
use utoipa::OpenApi;

#[derive(OpenApi)]
#[openapi(paths(health))]
pub struct HealthApi;

#[utoipa::path(
    responses(
            (status = 200, description = "Internal Healthcheck")
    )
)]
#[get("/health")]
pub async fn health(db: &State<DatabaseConnection>) -> Result<(), String> {
    // `{:#}` rather than `{}`: the outer message alone is usually just
    // "connection error", and the cause underneath it is the part worth
    // reading. `{:?}` would add a backtrace nobody asked for over HTTP.
    check_health(db).await.map_err(|e| format!("{e:#}"))?;
    Ok(())
}

async fn check_health(db: &DatabaseConnection) -> anyhow::Result<()> {
    // Check database connection.
    db.ping().await?;

    Ok(())
}
