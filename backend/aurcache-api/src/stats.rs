use crate::auth::has_api_token;
use anyhow::bail;
use bigdecimal::ToPrimitive;

use rocket::serde::json::Json;

use crate::models::authenticated::Authenticated;
use crate::models::stats::{GraphDataPoint, ListStats, UserInfo};
use crate::utils::error::{ApiError, err};
use aurcache_db::builds;
use aurcache_db::helpers::dbtype::database_type;
use aurcache_db::prelude::{Builds, Packages};
use aurcache_types::api::stats::RECENT_DAYS;
use aurcache_types::builder::BuildStates;
use aurcache_utils::utils::dir_size::dir_size;
use rocket::http::Status;
use rocket::{State, get};
use sea_orm::prelude::BigDecimal;
use sea_orm::{ColumnTrait, QueryFilter};
use sea_orm::{DatabaseConnection, EntityTrait};
use sea_orm::{DbBackend, FromQueryResult, PaginatorTrait, Statement};
use std::time::{SystemTime, UNIX_EPOCH};
use utoipa::OpenApi;

#[derive(OpenApi)]
#[openapi(paths(stats, dashboard_graph_data, user_info))]
pub struct StatsApi;

#[utoipa::path(
    responses(
            (status = 200, description = "Get general build-server stats", body = [ListStats]),
    )
)]
#[get("/stats")]
pub async fn stats(
    db: &State<DatabaseConnection>,
    _a: Authenticated,
) -> Result<Json<ListStats>, ApiError> {
    let db = db.inner();

    get_stats(db)
        .await
        .map_err(|e| err(Status::InternalServerError, e))
        .map(Json)
}

#[utoipa::path(
    responses(
            (status = 200, description = "Get infos about the signed in user", body = [UserInfo]),
    )
)]
#[get("/userinfo")]
pub async fn user_info(
    db: &State<DatabaseConnection>,
    a: Authenticated,
) -> Result<Json<UserInfo>, ApiError> {
    let username = a.username;
    let has_api_token = match &username {
        Some(username) => has_api_token(db, username)
            .await
            .map_err(|e| err(Status::InternalServerError, e))?,
        None => false,
    };
    Ok(Json(UserInfo {
        username,
        has_api_token,
    }))
}

#[utoipa::path(
    responses(
        (status = 200, description = "Get graph data for dashboard", body = [Vec<GraphDataPoint>]),
    )
)]
#[get("/graph")]
pub async fn dashboard_graph_data(
    db: &State<DatabaseConnection>,
    _a: Authenticated,
) -> Result<Json<Vec<GraphDataPoint>>, ApiError> {
    let db = db.inner();

    get_graph_datapoints(db)
        .await
        .map_err(|e| err(Status::InternalServerError, e))
        .map(Json)
}

async fn get_graph_datapoints(db: &DatabaseConnection) -> anyhow::Result<Vec<GraphDataPoint>> {
    // The success predicate is built from the enum rather than written as a
    // literal, so a renumbered state cannot silently turn this into a count of
    // something else.
    let succeeded = format!("status = {}", BuildStates::SUCCESSFUL_BUILD);
    let query = match database_type() {
        DbBackend::Sqlite => {
            format!(
                "SELECT
    CAST(strftime('%Y', datetime(start_time, 'unixepoch')) AS INTEGER) AS year,
    CAST(strftime('%m', datetime(start_time, 'unixepoch')) AS INTEGER) AS month,
    COUNT(*) AS count,
    CAST(SUM(CASE WHEN {succeeded} THEN 1 ELSE 0 END) AS INTEGER) AS successful
FROM
    builds
WHERE
    start_time >= strftime('%s', 'now', 'start of month', '-11 months')
GROUP BY
    year, month
ORDER BY
    year DESC, month DESC;"
            )
        }
        DbBackend::Postgres => {
            format!(
                "SELECT
    EXTRACT(YEAR FROM to_timestamp(start_time))::INTEGER AS year,
    EXTRACT(MONTH FROM to_timestamp(start_time))::INTEGER AS month,
    COUNT(*)::INTEGER AS count,
    SUM(CASE WHEN {succeeded} THEN 1 ELSE 0 END)::INTEGER AS successful
FROM
    builds
WHERE
    start_time >= EXTRACT(EPOCH FROM date_trunc('month', now()) - interval '11 months')
GROUP BY
    year, month
ORDER BY
    year DESC, month DESC;"
            )
        }
        _ => bail!("Unsupported database type"),
    };

    let result = GraphDataPoint::find_by_statement(Statement::from_sql_and_values(
        database_type(),
        &query,
        vec![],
    ))
    .all(db)
    .await?;

    Ok(result)
}

/// Packages someone asked for (`requested`), or ones present only as
/// dependencies of those.
async fn count_packages(db: &DatabaseConnection, requested: bool) -> anyhow::Result<u32> {
    Packages::find()
        .filter(aurcache_db::packages::Column::DirectlyRequested.eq(requested))
        .count(db)
        .await?
        .try_into()
        .map_err(Into::into)
}

/// Average duration of a successful build, in seconds.
///
/// The query is dialect-neutral, so it is issued with the SQLite backend on
/// every database. Missing or unrepresentable averages read as `0`.
async fn avg_build_time(db: &DatabaseConnection) -> anyhow::Result<u32> {
    #[derive(Debug, FromQueryResult)]
    struct BuildTimeStruct {
        avg_build_time: Option<BigDecimal>,
    }

    let unique: BuildTimeStruct =
        BuildTimeStruct::find_by_statement(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            r"SELECT AVG((builds.end_time - builds.start_time)) AS avg_build_time
        FROM builds
        WHERE builds.end_time IS NOT NULL AND builds.status = 1;",
            [],
        ))
        .one(db)
        .await?
        .ok_or_else(|| anyhow::anyhow!("No Average build time"))?;

    Ok(unique
        .avg_build_time
        .and_then(|avg| avg.to_u32())
        .unwrap_or(0))
}

/// Change over the last 30 days relative to the 30 days before it, as a
/// fraction (`0.5` = up 50%). Each is `0.0` when the earlier window is empty.
struct BuildTrends {
    count: f32,
    duration: f32,
}

/// The 60-day windowed aggregate behind [`BuildTrends`], per SQL dialect.
fn build_trends_query() -> anyhow::Result<&'static str> {
    Ok(match database_type() {
        DbBackend::Sqlite => "
WITH build_stats AS (
    SELECT
        CASE
            WHEN start_time >= strftime('%s', 'now', '-30 days') THEN 'last_30_days'
            WHEN start_time >= strftime('%s', 'now', '-60 days') THEN 'prev_30_days'
            END AS period,
        COUNT(*) AS build_count,
        AVG(end_time - start_time) AS avg_build_duration
    FROM builds
    WHERE start_time >= strftime('%s', 'now', '-60 days') -- Only consider last 60 days
    GROUP BY period
)
SELECT
    COALESCE((SELECT build_count FROM build_stats WHERE period = 'last_30_days'), 0) AS last_30_days_builds,
    COALESCE((SELECT avg_build_duration FROM build_stats WHERE period = 'last_30_days'), 0.0) AS last_30_days_avg_duration,
    COALESCE((SELECT build_count FROM build_stats WHERE period = 'prev_30_days'), 0) AS prev_30_days_builds,
    COALESCE((SELECT avg_build_duration FROM build_stats WHERE period = 'prev_30_days'), 0.0) AS prev_30_days_avg_duration;
    ",
        DbBackend::Postgres => "
WITH build_stats AS (
    SELECT
        CASE
            WHEN start_time >= EXTRACT(EPOCH FROM NOW() - INTERVAL '30 days') THEN 'last_30_days'
            WHEN start_time >= EXTRACT(EPOCH FROM NOW() - INTERVAL '60 days') THEN 'prev_30_days'
        END AS period,
        COUNT(*) AS build_count,
        AVG(end_time - start_time)::FLOAT4 AS avg_build_duration
    FROM builds
    WHERE start_time >= EXTRACT(EPOCH FROM NOW() - INTERVAL '60 days')
    GROUP BY period
)
SELECT
    COALESCE((SELECT build_count FROM build_stats WHERE period = 'last_30_days'), 0) AS last_30_days_builds,
    COALESCE((SELECT avg_build_duration FROM build_stats WHERE period = 'last_30_days'), 0.0) AS last_30_days_avg_duration,
    COALESCE((SELECT build_count FROM build_stats WHERE period = 'prev_30_days'), 0) AS prev_30_days_builds,
    COALESCE((SELECT avg_build_duration FROM build_stats WHERE period = 'prev_30_days'), 0.0) AS prev_30_days_avg_duration;
",
        _ => bail!("Unsupported database type"),
    })
}

async fn build_trends(db: &DatabaseConnection) -> anyhow::Result<BuildTrends> {
    #[derive(Debug, FromQueryResult)]
    struct LastBuildsStruct {
        last_30_days_builds: i64,
        prev_30_days_builds: i64,
        last_30_days_avg_duration: f32,
        prev_30_days_avg_duration: f32,
    }

    let last_build_cnt: LastBuildsStruct = LastBuildsStruct::find_by_statement(
        Statement::from_sql_and_values(database_type(), build_trends_query()?, []),
    )
    .one(db)
    .await?
    .ok_or_else(|| anyhow::anyhow!("No last build cnts"))?;

    let count = if last_build_cnt.prev_30_days_builds == 0 {
        0.0
    } else {
        (last_build_cnt.last_30_days_builds as f32 / last_build_cnt.prev_30_days_builds as f32)
            - 1.0
    };

    let duration = if last_build_cnt.prev_30_days_avg_duration == 0.0 {
        0.0
    } else {
        (last_build_cnt.last_30_days_avg_duration / last_build_cnt.prev_30_days_avg_duration) - 1.0
    };

    Ok(BuildTrends { count, duration })
}

/// Count builds, optionally in one state and optionally only recent ones.
///
/// `since` is a Unix second; builds with no start time are excluded from a
/// windowed count, since an unstarted build has not happened yet.
async fn count_builds(
    db: &DatabaseConnection,
    status: Option<i32>,
    since: Option<i64>,
) -> anyhow::Result<u32> {
    let mut query = Builds::find();
    if let Some(status) = status {
        query = query.filter(builds::Column::Status.eq(status));
    }
    if let Some(since) = since {
        query = query.filter(builds::Column::StartTime.gte(since));
    }
    Ok(query.count(db).await?.try_into()?)
}

async fn get_stats(db: &DatabaseConnection) -> anyhow::Result<ListStats> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
    let cutoff = now - RECENT_DAYS * 24 * 60 * 60;

    let trends = build_trends(db).await?;

    Ok(ListStats {
        total_builds: count_builds(db, None, None).await?,
        successful_builds: count_builds(db, Some(BuildStates::SUCCESSFUL_BUILD), None).await?,
        failed_builds: count_builds(db, Some(BuildStates::FAILED_BUILD), None).await?,

        recent_builds: count_builds(db, None, Some(cutoff)).await?,
        recent_successful: count_builds(db, Some(BuildStates::SUCCESSFUL_BUILD), Some(cutoff))
            .await?,
        recent_failed: count_builds(db, Some(BuildStates::FAILED_BUILD), Some(cutoff)).await?,

        avg_build_time: avg_build_time(db).await?,
        repo_size: dir_size("repo/").unwrap_or(0),
        requested_packages: count_packages(db, true).await?,
        dependency_packages: count_packages(db, false).await?,
        total_build_trend: trends.count,
        avg_build_time_trend: trends.duration,
    })
}

#[cfg(test)]
mod tests {
    use super::count_packages;
    use aurcache_db::migration::Migrator;
    use aurcache_db::packages::{self, SourceData};
    use sea_orm::{ActiveModelTrait, Database, Set};
    use sea_orm_migration::MigratorTrait;

    #[tokio::test]
    async fn stats_only_count_directly_requested_packages() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

        packages::ActiveModel {
            name: Set("top-level".to_string()),
            status: Set(1),
            out_of_date: Set(0),
            upstream_version: Set(Some("1.0.0".to_string())),
            latest_build: Set(None),
            build_flags: Set("--noconfirm".to_string()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(packages::SourceType::Aur),
            source_data: Set(SourceData::Aur {
                name: "top-level".into(),
            }),
            directly_requested: Set(true),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        packages::ActiveModel {
            name: Set("transitive-dependency".to_string()),
            status: Set(1),
            out_of_date: Set(0),
            upstream_version: Set(Some("1.0.0".to_string())),
            latest_build: Set(None),
            build_flags: Set("--noconfirm".to_string()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(packages::SourceType::Aur),
            source_data: Set(SourceData::Aur {
                name: "transitive-dependency".into(),
            }),
            directly_requested: Set(false),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        // One of each was inserted above, so the two counts partition the
        // table — a filter that ignored the flag would give 2 for both.
        assert_eq!(count_packages(&db, true).await.unwrap(), 1);
        assert_eq!(count_packages(&db, false).await.unwrap(), 1);
    }
}
