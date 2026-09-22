use crate::auth::has_api_token;
use anyhow::bail;
use bigdecimal::ToPrimitive;

use rocket::serde::json::Json;

use crate::build::{BuildRow, annotate_waiting, build_row_select};
use crate::models::authenticated::Authenticated;
use crate::models::stats::{
    DashboardView, GraphDataPoint, ListStats, LongBuild, OutOfDateSlice, QueueSlice, UserInfo,
};
use crate::package::package_row_select;
use crate::utils::error::{ApiError, err};
use aurcache_activitylog::log_store::{LogFilter, LogStore};
use aurcache_common::api::activity::Severity;
use aurcache_common::api::builds::BuildSummary;
use aurcache_common::api::log::LogEntry;
use aurcache_common::api::package::SimplePackage;
use aurcache_common::api::stats::{LONGEST_WINDOW_DAYS, RECENT_DAYS};
use aurcache_common::builder::BuildStates;
use aurcache_common::fs::dir_size;
use aurcache_common::settings::{ApplicationSettings, Setting};
use aurcache_db::builds;
use aurcache_db::helpers::dbtype::database_type;
use aurcache_db::helpers::files::total_artifact_size_expr;
use aurcache_db::prelude::{Builds, Packages};
use aurcache_db::{packages, workers};
use aurcache_utils::settings::general::SettingsTraits;
use rocket::http::Status;
use rocket::{State, get};
use sea_orm::prelude::BigDecimal;
use sea_orm::sea_query::{Expr, ExprTrait, Func};
use sea_orm::{ColumnTrait, QueryFilter, QuerySelect};
use sea_orm::{DatabaseConnection, EntityTrait};
use sea_orm::{
    DbBackend, FromQueryResult, JoinType, Order, PaginatorTrait, QueryOrder, RelationTrait,
    Statement,
};
use std::collections::HashSet;
use utoipa::OpenApi;

#[derive(OpenApi)]
#[openapi(paths(stats, dashboard, dashboard_graph_data, user_info))]
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
    let backend = database_type();
    let query = match backend {
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

    let result =
        GraphDataPoint::find_by_statement(Statement::from_sql_and_values(backend, &query, vec![]))
            .all(db)
            .await?;

    Ok(result)
}

/// Packages someone asked for (`requested`), or ones present only as
/// dependencies of those.
async fn count_packages(db: &DatabaseConnection, requested: bool) -> anyhow::Result<u32> {
    let count = Packages::find()
        .filter(aurcache_db::packages::Column::DirectlyRequested.eq(requested))
        .count(db)
        .await?
        .try_into()?;
    Ok(count)
}

/// Average duration of a successful build, in seconds.
///
/// Missing or unrepresentable averages read as `0`.
async fn avg_build_time(db: &DatabaseConnection) -> anyhow::Result<u32> {
    #[derive(Debug, FromQueryResult)]
    struct BuildTimeStruct {
        avg_build_time: Option<BigDecimal>,
    }

    let unique: Option<BuildTimeStruct> = Builds::find()
        .select_only()
        .column_as(
            Expr::from(Func::avg(
                Expr::col(builds::Column::EndTime).sub(Expr::col(builds::Column::StartTime)),
            )),
            "avg_build_time",
        )
        .filter(builds::Column::EndTime.is_not_null())
        .filter(builds::Column::Status.eq(BuildStates::SUCCESSFUL_BUILD))
        .into_model::<BuildTimeStruct>()
        .one(db)
        .await?;
    // An aggregate without `GROUP BY` always returns a row, so "no rows" can
    // only mean an empty table — which deserves a 0 average, not a failed
    // `/stats` page. (The old `ok_or_else` arm was dead code that errored the
    // whole endpoint exactly when there was nothing to average.)
    Ok(unique
        .and_then(|row| row.avg_build_time)
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
    let now = aurcache_db::helpers::time::now_secs();
    let cutoff = now - RECENT_DAYS * 24 * 60 * 60;

    // Independent reads over one pooled connection: serial awaits paid each
    // round trip in turn for queries that share nothing.
    let (
        total_builds,
        successful_builds,
        failed_builds,
        recent_builds,
        recent_successful,
        recent_failed,
        trends,
        avg_build_time,
        requested_packages,
        dependency_packages,
    ) = tokio::join!(
        count_builds(db, None, None),
        count_builds(db, Some(BuildStates::SUCCESSFUL_BUILD), None),
        count_builds(db, Some(BuildStates::FAILED_BUILD), None),
        count_builds(db, None, Some(cutoff)),
        count_builds(db, Some(BuildStates::SUCCESSFUL_BUILD), Some(cutoff)),
        count_builds(db, Some(BuildStates::FAILED_BUILD), Some(cutoff)),
        build_trends(db),
        avg_build_time(db),
        count_packages(db, true),
        count_packages(db, false),
    );

    let trends = trends?;
    Ok(ListStats {
        total_builds: total_builds?,
        successful_builds: successful_builds?,
        failed_builds: failed_builds?,

        recent_builds: recent_builds?,
        recent_successful: recent_successful?,
        recent_failed: recent_failed?,

        avg_build_time: avg_build_time?,
        repo_size: dir_size("repo/"),
        requested_packages: requested_packages?,
        dependency_packages: dependency_packages?,
        total_build_trend: trends.count,
        avg_build_time_trend: trends.duration,
    })
}

/// How many rows each dashboard card shows.
const DASHBOARD_LIMIT: u64 = 5;

#[utoipa::path(
    responses(
            (status = 200, description = "Dashboard slices for the landing page", body = [DashboardView]),
    )
)]
#[get("/stats/dashboard")]
pub async fn dashboard(
    db: &State<DatabaseConnection>,
    _a: Authenticated,
) -> Result<Json<DashboardView>, ApiError> {
    let db = db.inner();

    fn opt<T>(result: anyhow::Result<T>, section: &str) -> Option<T> {
        match result {
            Ok(value) => Some(value),
            Err(e) => {
                tracing::warn!("dashboard section {section} failed: {e:#}");
                None
            }
        }
    }

    let (recent_packages, recent_builds, failed, out_of_date, queue, problems, largest, longest) = tokio::join!(
        recent_packages(db),
        recent_builds(db),
        failed_packages(db),
        out_of_date_slice(db),
        queue_slice(db),
        problem_entries(db),
        largest_packages(db),
        longest_builds(db),
    );

    Ok(Json(DashboardView {
        recent_packages: opt(recent_packages, "recent_packages"),
        recent_builds: opt(recent_builds, "recent_builds"),
        failed: opt(failed, "failed"),
        out_of_date: opt(out_of_date, "out_of_date"),
        queue: opt(queue, "queue"),
        problems: opt(problems, "problems"),
        largest: opt(largest, "largest"),
        longest: opt(longest, "longest"),
    }))
}

async fn recent_packages(db: &DatabaseConnection) -> anyhow::Result<Vec<SimplePackage>> {
    Ok(package_row_select()
        .filter(packages::Column::DirectlyRequested.eq(true))
        .order_by(packages::Column::Id, Order::Desc)
        .limit(DASHBOARD_LIMIT)
        .into_model::<SimplePackage>()
        .all(db)
        .await?)
}

async fn recent_builds(db: &DatabaseConnection) -> anyhow::Result<Vec<BuildSummary>> {
    let rows = build_row_select()
        .order_by(builds::Column::StartTime, Order::Desc)
        .limit(DASHBOARD_LIMIT)
        .into_model::<BuildRow>()
        .all(db)
        .await?;
    Ok(annotate_waiting(db, rows).await)
}

async fn failed_packages(db: &DatabaseConnection) -> anyhow::Result<Vec<SimplePackage>> {
    Ok(package_row_select()
        .filter(packages::Column::Status.eq(BuildStates::FAILED_BUILD))
        .order_by(packages::Column::Id, Order::Desc)
        .limit(DASHBOARD_LIMIT)
        .into_model::<SimplePackage>()
        .all(db)
        .await?)
}

async fn out_of_date_slice(db: &DatabaseConnection) -> anyhow::Result<OutOfDateSlice> {
    let outdated: Vec<SimplePackage> = package_row_select()
        .filter(packages::Column::OutOfDate.ne(0))
        .order_by(packages::Column::Id, Order::Desc)
        .into_model::<SimplePackage>()
        .all(db)
        .await?;
    if outdated.is_empty() {
        return Ok(OutOfDateSlice {
            needs_hand: Vec::new(),
            handled: 0,
        });
    }
    let pkg_ids: Vec<i32> = outdated.iter().map(|pkg| pkg.id).collect();

    let auto: Vec<bool> =
        ApplicationSettings::get_many::<bool>(Setting::BuildOnNewVersion, &pkg_ids, db)
            .await?
            .into_iter()
            .map(|entry| entry.value)
            .collect();

    let active: HashSet<i32> = if pkg_ids.is_empty() {
        HashSet::new()
    } else {
        Builds::find()
            .select_only()
            .column(builds::Column::PkgId)
            .filter(builds::Column::PkgId.is_in(pkg_ids.clone()))
            .filter(builds::Column::Status.is_in([
                BuildStates::ACTIVE_BUILD,
                BuildStates::ENQUEUED_BUILD,
                BuildStates::WAITING_FOR_DEPS,
                BuildStates::PUBLISHING,
            ]))
            .into_tuple::<(i32,)>()
            .all(db)
            .await?
            .into_iter()
            .map(|(pkg_id,)| pkg_id)
            .collect()
    };

    let mut needs_hand = Vec::new();
    let mut handled = 0u64;
    for (pkg, auto_on) in outdated.into_iter().zip(auto) {
        if !auto_on && !active.contains(&pkg.id) {
            if needs_hand.len() < DASHBOARD_LIMIT as usize {
                needs_hand.push(pkg);
            }
        } else {
            handled += 1;
        }
    }
    Ok(OutOfDateSlice {
        needs_hand,
        handled,
    })
}

async fn queue_slice(db: &DatabaseConnection) -> anyhow::Result<QueueSlice> {
    let queued = [BuildStates::ENQUEUED_BUILD, BuildStates::WAITING_FOR_DEPS];
    let depth: u64 = Builds::find()
        .filter(builds::Column::Status.is_in(queued))
        .count(db)
        .await?;
    let rows = build_row_select()
        .filter(builds::Column::Status.is_in(queued))
        .order_by(builds::Column::StartTime, Order::Asc)
        .limit(DASHBOARD_LIMIT)
        .into_model::<BuildRow>()
        .all(db)
        .await?;
    Ok(QueueSlice {
        depth,
        oldest: annotate_waiting(db, rows).await,
    })
}

async fn problem_entries(db: &DatabaseConnection) -> anyhow::Result<Vec<LogEntry>> {
    let page = LogStore::new(db.clone())
        .page(
            DASHBOARD_LIMIT,
            0,
            &LogFilter {
                severity: Some(Severity::Warning),
                ..Default::default()
            },
        )
        .await?;
    Ok(page.entries)
}

async fn largest_packages(db: &DatabaseConnection) -> anyhow::Result<Vec<SimplePackage>> {
    Ok(package_row_select()
        .order_by(total_artifact_size_expr(), Order::Desc)
        .limit(DASHBOARD_LIMIT)
        .into_model::<SimplePackage>()
        .all(db)
        .await?)
}

/// One long-build candidate with the package id for the previous-run lookup.
#[derive(FromQueryResult)]
struct LongestRow {
    pkg_id: i32,
    number: i32,
    pkg_name: String,
    version: String,
    status: i32,
    start_time: Option<i64>,
    end_time: Option<i64>,
    platform: String,
    size: Option<i64>,
    peak_memory: Option<i64>,
    worker_name: Option<String>,
}

async fn longest_builds(db: &DatabaseConnection) -> anyhow::Result<Vec<LongBuild>> {
    let since = aurcache_db::helpers::time::now_secs() - LONGEST_WINDOW_DAYS * 24 * 60 * 60;
    let duration = Expr::col(builds::Column::EndTime).sub(Expr::col(builds::Column::StartTime));
    let rows: Vec<LongestRow> = Builds::find()
        .join_rev(JoinType::InnerJoin, packages::Relation::Builds.def())
        .select_only()
        .column_as(builds::Column::PkgId, "pkg_id")
        .column_as(builds::Column::Number, "number")
        .column(builds::Column::Status)
        .column_as(packages::Column::Name, "pkg_name")
        .column(builds::Column::Version)
        .column(builds::Column::EndTime)
        .column(builds::Column::StartTime)
        .column(builds::Column::Platform)
        .column(builds::Column::Size)
        .column(builds::Column::PeakMemory)
        .join(JoinType::LeftJoin, builds::Relation::Workers.def())
        .column_as(workers::Column::Name, "worker_name")
        .filter(builds::Column::Status.eq(BuildStates::SUCCESSFUL_BUILD))
        .filter(builds::Column::StartTime.gte(since))
        .filter(builds::Column::EndTime.is_not_null())
        .order_by(duration, Order::Desc)
        .limit(DASHBOARD_LIMIT)
        .into_model::<LongestRow>()
        .all(db)
        .await?;

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let previous_secs = match (row.start_time, row.pkg_id, row.platform.clone()) {
            (Some(started), pkg_id, platform) => {
                let prev: Option<(Option<i64>, Option<i64>)> = Builds::find()
                    .select_only()
                    .column(builds::Column::StartTime)
                    .column(builds::Column::EndTime)
                    .filter(builds::Column::PkgId.eq(pkg_id))
                    .filter(builds::Column::Platform.eq(platform.as_str()))
                    .filter(builds::Column::Status.eq(BuildStates::SUCCESSFUL_BUILD))
                    .filter(builds::Column::StartTime.lt(started))
                    .order_by(builds::Column::StartTime, Order::Desc)
                    .limit(1)
                    .into_tuple::<(Option<i64>, Option<i64>)>()
                    .one(db)
                    .await?;
                match prev {
                    Some((Some(prev_start), Some(prev_end))) => {
                        Some(std::cmp::max(prev_end.saturating_sub(prev_start), 0))
                    }
                    _ => None,
                }
            }
            _ => None,
        };
        out.push(LongBuild {
            build: BuildSummary {
                number: row.number,
                pkg_name: row.pkg_name,
                version: row.version,
                status: row.status,
                start_time: row.start_time,
                end_time: row.end_time,
                platform: row.platform,
                size: row.size,
                peak_memory: row.peak_memory,
                worker_name: row.worker_name,
                log_size: None,
                waiting_reason: None,
            },
            previous_secs,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{
        count_packages, failed_packages, largest_packages, longest_builds, out_of_date_slice,
        problem_entries, queue_slice, recent_builds, recent_packages,
    };
    use aurcache_common::api::activity::Severity;
    use aurcache_common::builder::BuildStates;
    use aurcache_common::settings::{ApplicationSettings, Setting};
    use aurcache_db::helpers::time::now_secs;
    use aurcache_db::migration::Migrator;
    use aurcache_db::packages::{self, SourceData};
    use aurcache_db::{builds, logs};
    use aurcache_utils::settings::general::SettingsTraits;
    use pacman_mirrors::platforms::Platform;
    use sea_orm::{ActiveModelTrait, Database, EntityTrait, Set};
    use sea_orm_migration::MigratorTrait;

    async fn insert_package(
        db: &sea_orm::DatabaseConnection,
        name: &str,
        out_of_date: i32,
        directly_requested: bool,
    ) -> i32 {
        packages::ActiveModel {
            name: Set(name.to_string()),
            status: Set(BuildStates::SUCCESSFUL_BUILD),
            out_of_date: Set(out_of_date),
            upstream_version: Set(Some("1.0-1".to_string())),
            latest_build: Set(None),
            build_flags: Set(String::new()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(packages::SourceType::Aur),
            source_data: Set(SourceData::Aur {
                name: name.to_string(),
            }),
            directly_requested: Set(directly_requested),
            split_packages: Set(None),
            ..Default::default()
        }
        .insert(db)
        .await
        .expect("insert package")
        .id
    }

    async fn insert_build(
        db: &sea_orm::DatabaseConnection,
        pkg_id: i32,
        number: i32,
        status: i32,
        start: Option<i64>,
        end: Option<i64>,
    ) {
        aurcache_db::prelude::Builds::insert(builds::ActiveModel {
            pkg_id: Set(pkg_id),
            number: Set(number),
            status: Set(Some(status)),
            platform: Set(Platform::X86_64),
            version: Set("1.0-1".to_string()),
            start_time: Set(start),
            end_time: Set(end),
            ..Default::default()
        })
        .exec(db)
        .await
        .expect("insert build");
    }

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

    #[tokio::test]
    async fn recent_packages_are_newest_requested_regardless_of_out_of_date() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

        insert_package(&db, "pkg-a", 0, true).await;
        insert_package(&db, "pkg-b", 1, true).await;
        insert_package(&db, "pkg-dep", 0, false).await;

        let recent = recent_packages(&db).await.unwrap();
        assert_eq!(recent.len(), 2);
        // Id DESC: pkg-b was inserted after pkg-a, out-of-date or not.
        assert_eq!(recent[0].name, "pkg-b");
        assert_eq!(recent[1].name, "pkg-a");
    }

    #[tokio::test]
    async fn queue_is_oldest_first_depth_counts_past_cap_and_reason_survives() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

        // One queued build per package: the pending (pkg_id, platform)
        // unique index forbids two queued builds for the same package on
        // the same platform.
        for number in 1..=7 {
            let pkg_id = insert_package(&db, &format!("queued-pkg-{number}"), 0, true).await;
            insert_build(
                &db,
                pkg_id,
                1,
                BuildStates::ENQUEUED_BUILD,
                Some(100 + number as i64),
                None,
            )
            .await;
        }
        let done_id = insert_package(&db, "done-pkg", 0, true).await;
        insert_build(
            &db,
            done_id,
            1,
            BuildStates::SUCCESSFUL_BUILD,
            Some(200),
            Some(260),
        )
        .await;

        let queue = queue_slice(&db).await.unwrap();
        assert_eq!(queue.depth, 7);
        assert_eq!(queue.oldest.len(), 5);
        // Oldest first: queued since 101 comes before 102.
        assert_eq!(queue.oldest[0].start_time, Some(101));
        assert_eq!(queue.oldest[4].start_time, Some(105));
        // No workers are registered, so no approved worker can take an
        // x86_64 build — the server names the missing arch.
        assert!(queue.oldest[0].waiting_reason.is_some());
    }

    #[tokio::test]
    async fn out_of_date_splits_on_setting_and_queued_build() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

        let pkg_a = insert_package(&db, "needs-hand", 1, true).await;
        insert_package(&db, "auto-rebuild", 1, true).await;
        let pkg_c = insert_package(&db, "queued-anyway", 1, true).await;

        // Global on, with per-package overrides off for A and C: the override
        // wins over the global, so A and C resolve off and B resolves on.
        ApplicationSettings::patch(
            &db,
            [(Setting::BuildOnNewVersion, None, Some("true".to_string()))],
        )
        .await
        .unwrap();
        ApplicationSettings::patch(
            &db,
            [
                (
                    Setting::BuildOnNewVersion,
                    Some(pkg_a),
                    Some("false".to_string()),
                ),
                (
                    Setting::BuildOnNewVersion,
                    Some(pkg_c),
                    Some("false".to_string()),
                ),
            ],
        )
        .await
        .unwrap();
        insert_build(
            &db,
            pkg_c,
            1,
            BuildStates::ENQUEUED_BUILD,
            Some(now_secs()),
            None,
        )
        .await;

        let slice = out_of_date_slice(&db).await.unwrap();
        assert_eq!(slice.needs_hand.len(), 1);
        assert_eq!(slice.needs_hand[0].name, "needs-hand");
        // B rebuilds on its own via the global setting, C via its queued
        // build despite resolving off.
        assert_eq!(slice.handled, 2);
    }

    #[tokio::test]
    async fn longest_respects_window_and_pairs_previous_on_same_platform() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

        let now = now_secs();
        let pkg_id = insert_package(&db, "long-pkg", 0, true).await;
        // Outside the 30-day window: long enough to win if the window were
        // not applied, so its absence proves the filter.
        insert_build(
            &db,
            pkg_id,
            1,
            BuildStates::SUCCESSFUL_BUILD,
            Some(now - 40 * 24 * 60 * 60),
            Some(now - 40 * 24 * 60 * 60 + 1000),
        )
        .await;
        insert_build(
            &db,
            pkg_id,
            2,
            BuildStates::SUCCESSFUL_BUILD,
            Some(now - 10 * 24 * 60 * 60),
            Some(now - 10 * 24 * 60 * 60 + 60),
        )
        .await;
        insert_build(
            &db,
            pkg_id,
            3,
            BuildStates::SUCCESSFUL_BUILD,
            Some(now - 24 * 60 * 60),
            Some(now - 24 * 60 * 60 + 120),
        )
        .await;
        let single_id = insert_package(&db, "single-pkg", 0, true).await;
        insert_build(
            &db,
            single_id,
            1,
            BuildStates::SUCCESSFUL_BUILD,
            Some(now - 24 * 60 * 60),
            Some(now - 24 * 60 * 60 + 30),
        )
        .await;

        let longest = longest_builds(&db).await.unwrap();
        assert_eq!(longest.len(), 3);
        // Ordered by duration DESC: 120, 60, 30. The 1000-second build is
        // outside the window and must not appear.
        assert_eq!(longest[0].build.pkg_name, "long-pkg");
        assert_eq!(longest[0].build.number, 3);
        assert_eq!(longest[0].previous_secs, Some(60));
        assert_eq!(longest[2].build.pkg_name, "single-pkg");
        assert_eq!(longest[2].previous_secs, None);
    }

    #[tokio::test]
    async fn problems_exclude_info() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

        let now = now_secs();
        for (kind, severity) in [
            ("test.info", Severity::Info),
            ("test.warn", Severity::Warning),
        ] {
            logs::ActiveModel {
                kind: Set(kind.to_string()),
                severity: Set(severity),
                message: Set(format!("{kind} happened")),
                data: Set("{}".to_string()),
                scope: Set(None),
                timestamp: Set(now),
                user: Set(None),
                ..Default::default()
            }
            .insert(&db)
            .await
            .unwrap();
        }

        let problems = problem_entries(&db).await.unwrap();
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].kind, "test.warn");
    }

    #[tokio::test]
    async fn dashboard_slices_succeed_on_an_empty_db_and_hold_none() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

        // Every slice is independently fallible in the route (`None` on
        // error), so each must succeed on an empty instance rather than
        // failing the whole page.
        assert!(recent_packages(&db).await.is_ok());
        assert!(recent_builds(&db).await.is_ok());
        assert!(failed_packages(&db).await.is_ok());
        assert!(out_of_date_slice(&db).await.is_ok());
        assert!(queue_slice(&db).await.is_ok());
        assert!(problem_entries(&db).await.is_ok());
        assert!(largest_packages(&db).await.is_ok());
        assert!(longest_builds(&db).await.is_ok());

        // The shape itself holds a failed section while the rest render:
        // one `None` must survive a round trip.
        let view = super::DashboardView {
            recent_packages: Some(Vec::new()),
            recent_builds: Some(Vec::new()),
            failed: Some(Vec::new()),
            out_of_date: Some(super::OutOfDateSlice {
                needs_hand: Vec::new(),
                handled: 0,
            }),
            queue: Some(super::QueueSlice {
                depth: 0,
                oldest: Vec::new(),
            }),
            problems: None,
            largest: Some(Vec::new()),
            longest: Some(Vec::new()),
        };
        let json = serde_json::to_string(&view).unwrap();
        let back: super::DashboardView = serde_json::from_str(&json).unwrap();
        assert!(back.problems.is_none());
        assert!(back.recent_packages.is_some());
    }
}
