use itertools::Itertools;
use rocket::http::Status;
use rocket::serde::json::Json;
use rocket::{State, delete, get, post};

use crate::models::authenticated::Authenticated;
use crate::models::builds::BuildSummary;
use crate::utils::error::{ApiError, err};
use crate::worker::liveness_timeout_secs;
use aurcache_db::action::Action;
use aurcache_db::helpers::worker_jobs;
use aurcache_db::prelude::Builds;
use aurcache_db::{builds, packages};
use aurcache_types::api::waiting::WaitingReason;
use aurcache_utils::package::update::package_update;
use aurcache_utils::snapshot::SnapshotStore;
use sea_orm::FromQueryResult;
use sea_orm::{
    ColumnTrait, DatabaseConnection, EntityTrait, JoinType, ModelTrait, Order, QueryFilter,
    QueryOrder, QuerySelect, RelationTrait,
};
use std::sync::Arc;
use tokio::sync::broadcast::Sender;
use utoipa::OpenApi;

#[derive(OpenApi)]
#[openapi(paths(
    build_output,
    list_builds,
    list_package_builds,
    get_build,
    delete_build,
    cancel_build,
    retry_build
))]
pub struct BuildApi;

/// Slice a build's stored output for the caller.
///
/// `startline` is how many lines the caller already holds, so the response
/// carries only what is new; an out-of-range or negative offset simply skips
/// nothing or everything rather than erroring.
///
/// A build that exists but has not written anything yet yields an empty string.
/// "No output yet" is the normal state of a freshly started build, and the log
/// view polls this endpoint from the moment it opens — reporting that as an
/// error would make every new build's first poll fail.
fn slice_output(output: Option<String>, startline: Option<i32>) -> String {
    let output = output.unwrap_or_default();
    let Some(startline) = startline else {
        return output;
    };
    let skip = usize::try_from(startline).unwrap_or(0);
    output.lines().skip(skip).join("\n")
}

#[utoipa::path(
    responses(
            (status = 200, description = "Build output from `startline` onwards; empty if the build has not logged anything yet"),
            (status = 404, description = "No such build"),
    ),
    params(
            ("pkgbase", description = "pkgbase of the package"),
            ("number", description = "Build number within that package"),
            ("startline", description = "Number of leading lines to skip (i.e. how many the caller already has)")
    )
)]
#[get("/package/<pkgbase>/build/<number>/output?<startline>")]
pub async fn build_output(
    db: &State<DatabaseConnection>,
    pkgbase: &str,
    number: i32,
    startline: Option<i32>,
    _a: Authenticated,
) -> Result<String, ApiError> {
    let db = db.inner();

    let build = build_by_number(db, pkgbase, number).await?;

    Ok(slice_output(build.output, startline))
}

#[utoipa::path(
    responses(
            (status = 200, description = "List of all builds"),
    ),
    params(
            ("limit", description = "Limit of items to fetch"),
            ("page", description = "Page to fetch")
    )
)]
#[get("/builds?<limit>&<page>")]
pub async fn list_builds(
    db: &State<DatabaseConnection>,
    limit: Option<u64>,
    page: Option<u64>,
    _a: Authenticated,
) -> Result<Json<Vec<BuildSummary>>, ApiError> {
    list_builds_impl(db.inner(), None, limit, page).await
}

/// Builds for one package.
///
/// A sub-resource of the package rather than a `?pkgbase=` filter: a pkgbase
/// may contain `+`, which decodes to a space in a query value but is literal in
/// a path segment.
#[utoipa::path(
    responses((status = 200, description = "List builds for a package", body = Vec<BuildSummary>)),
    params(
        ("pkgbase" = String, Path, description = "pkgbase of the package"),
        ("limit", description = "Limit of items to fetch"),
        ("page", description = "Page to fetch"),
    )
)]
#[get("/package/<pkgbase>/builds?<limit>&<page>")]
pub async fn list_package_builds(
    db: &State<DatabaseConnection>,
    pkgbase: &str,
    limit: Option<u64>,
    page: Option<u64>,
    _a: Authenticated,
) -> Result<Json<Vec<BuildSummary>>, ApiError> {
    let pkg = crate::package::package_id_for(db.inner(), Some(pkgbase)).await?;
    list_builds_impl(db.inner(), pkg, limit, page).await
}

async fn list_builds_impl(
    db: &DatabaseConnection,
    pkg_id: Option<i32>,
    limit: Option<u64>,
    page: Option<u64>,
) -> Result<Json<Vec<BuildSummary>>, ApiError> {
    let basequery = Builds::find()
        .join_rev(JoinType::InnerJoin, packages::Relation::Builds.def())
        .select_only()
        .column_as(builds::Column::Id, "id")
        .column_as(builds::Column::Number, "number")
        .column(builds::Column::Status)
        .column_as(packages::Column::Name, "pkg_name")
        .column(builds::Column::Version)
        .column(builds::Column::EndTime)
        .column(builds::Column::StartTime)
        .column(builds::Column::Platform)
        .order_by(builds::Column::StartTime, Order::Desc)
        .limit(limit)
        .offset(page.zip(limit).map(|(page, limit)| page * limit));

    let rows = match pkg_id {
        None => basequery.into_model::<BuildRow>().all(db),
        Some(pkg_id) => basequery
            .filter(builds::Column::PkgId.eq(pkg_id))
            .into_model::<BuildRow>()
            .all(db),
    }
    .await
    .map_err(|e| err(Status::InternalServerError, e))?;

    Ok(Json(annotate_waiting(db, rows).await))
}

/// A listed build as queried, including the row id the response omits.
///
/// Waiting reasons are keyed by row id internally, so the id has to survive as
/// far as the annotation — but no further.
#[derive(FromQueryResult)]
struct BuildRow {
    id: i32,
    number: i32,
    pkg_name: String,
    version: String,
    status: i32,
    start_time: Option<i64>,
    end_time: Option<i64>,
    platform: String,
}

impl BuildRow {
    fn into_summary(self, waiting_reason: Option<WaitingReason>) -> BuildSummary {
        BuildSummary {
            number: self.number,
            pkg_name: self.pkg_name,
            version: self.version,
            status: self.status,
            start_time: self.start_time,
            end_time: self.end_time,
            platform: self.platform,
            waiting_reason,
        }
    }
}

/// Attach [`WaitingReason`]s to any listed build that no approved worker can
/// currently claim, so a stalled build is distinguishable from a queued one.
///
/// Best-effort by design: this is diagnostic decoration, and failing to compute
/// it must never turn a working build list into an error page.
async fn annotate_waiting(db: &DatabaseConnection, rows: Vec<BuildRow>) -> Vec<BuildSummary> {
    if rows.is_empty() {
        return Vec::new();
    }
    let reasons = match worker_jobs::waiting_reasons(db, liveness_timeout_secs()).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("could not compute build waiting reasons: {e}");
            Default::default()
        }
    };
    rows.into_iter()
        .map(|row| {
            let reason = reasons.get(&row.id).cloned();
            row.into_summary(reason)
        })
        .collect()
}

/// Resolve a public build identity — `<pkgbase>/<number>` — to its row.
///
/// The row id never leaves the server: it is a global sequence that says
/// nothing about which package a build belongs to. Everything public keys on
/// the package and the build's number within it.
async fn build_by_number(
    db: &DatabaseConnection,
    pkgbase: &str,
    number: i32,
) -> Result<builds::Model, ApiError> {
    Builds::find()
        .join_rev(JoinType::InnerJoin, packages::Relation::Builds.def())
        .filter(packages::Column::Name.eq(pkgbase))
        .filter(builds::Column::Number.eq(number))
        .one(db)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?
        .ok_or_else(|| err(Status::NotFound, format!("no build {pkgbase}/{number}")))
}

#[utoipa::path(
    responses(
            (status = 200, description = "Get build details"),
    ),
    params(
            ("pkgbase", description = "pkgbase of the package"),
            ("number", description = "Build number within that package")
    )
)]
#[get("/package/<pkgbase>/build/<number>")]
pub async fn get_build(
    db: &State<DatabaseConnection>,
    pkgbase: &str,
    number: i32,
    _a: Authenticated,
) -> Result<Json<BuildSummary>, ApiError> {
    let db = db.inner();

    let row = Builds::find()
        .join_rev(JoinType::InnerJoin, packages::Relation::Builds.def())
        .filter(packages::Column::Name.eq(pkgbase))
        .filter(builds::Column::Number.eq(number))
        .select_only()
        .column_as(builds::Column::Id, "id")
        .column_as(builds::Column::Number, "number")
        .column(builds::Column::Status)
        .column_as(packages::Column::Name, "pkg_name")
        .column(builds::Column::Version)
        .column(builds::Column::EndTime)
        .column(builds::Column::StartTime)
        .column(builds::Column::Platform)
        .into_model::<BuildRow>()
        .one(db)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?
        .ok_or_else(|| err(Status::NotFound, format!("no build {pkgbase}/{number}")))?;

    let mut annotated = annotate_waiting(db, vec![row]).await;
    annotated.pop().map(Json).ok_or_else(|| {
        err(
            Status::InternalServerError,
            "build vanished while annotating",
        )
    })
}

#[utoipa::path(
    responses(
            (status = 200, description = "Delete build"),
    ),
    params(
            ("pkgbase", description = "pkgbase of the package"),
            ("number", description = "Build number within that package")
    )
)]
#[delete("/package/<pkgbase>/build/<number>")]
pub async fn delete_build(
    db: &State<DatabaseConnection>,
    pkgbase: &str,
    number: i32,
    _a: Authenticated,
) -> Result<(), ApiError> {
    let db = db.inner();

    let build = build_by_number(db, pkgbase, number).await?;

    build
        .delete(db)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;

    Ok(())
}

#[utoipa::path(
    responses(
            (status = 200, description = "Cancel build job"),
    ),
    params(
            ("pkgbase", description = "pkgbase of the package"),
            ("number", description = "Build number within that package")
    )
)]
#[post("/package/<pkgbase>/build/<number>/cancel")]
pub async fn cancel_build(
    db: &State<DatabaseConnection>,
    tx: &State<Sender<Action>>,
    pkgbase: &str,
    number: i32,
    _a: Authenticated,
) -> Result<(), ApiError> {
    // Cancellation still travels by row id on the internal queue; only the way
    // the caller names the build has changed.
    let build_id = build_by_number(db.inner(), pkgbase, number).await?.id;
    tx.send(Action::Cancel(build_id))
        .map_err(|e| err(Status::InternalServerError, e))?;

    Ok(())
}

#[utoipa::path(
    responses(
            (status = 200, description = "Retry build"),
    ),
    params(
            ("pkgbase", description = "pkgbase of the package"),
            ("number", description = "Build number within that package"),
    )
)]
#[post("/package/<pkgbase>/build/<number>/retry")]
pub async fn retry_build(
    db: &State<DatabaseConnection>,
    tx: &State<Sender<Action>>,
    store: &State<Arc<SnapshotStore>>,
    pkgbase: &str,
    number: i32,
    _a: Authenticated,
) -> Result<Json<i32>, ApiError> {
    let db = db.inner();

    // The build being retried tells us which platform and package to rebuild.
    let old_build = build_by_number(db, pkgbase, number).await?;
    let platform = old_build.platform;
    let pkg_id = old_build.pkg_id;

    // Fetch the package details
    let package = packages::Entity::find_by_id(pkg_id)
        .one(db)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?
        .ok_or_else(|| err(Status::NotFound, "no package with that id"))?;

    // Route retries through the same path as "Force Rebuild": this re-fetches
    // the .SRCINFO, resolves AUR dependencies again, and syncs the dependency
    // graph before enqueuing builds, instead of blindly re-enqueuing the old
    // build's stored version with a stale dependency graph.
    let platform_results = package_update(store, db, package, true, tx)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;

    // Pick out the build explicitly reported for the platform that was
    // retried; it may have been enqueued/promoted or left waiting on a
    // dependency rebuild, either way `package_update` already tells us its id.
    let build_number = platform_results
        .into_iter()
        .find(|r| r.platform == platform)
        .map(|r| r.build_number)
        .ok_or_else(|| {
            err(
                Status::InternalServerError,
                "no build was enqueued for retry",
            )
        })?;

    Ok(Json(build_number))
}

#[cfg(test)]
mod tests {
    use super::slice_output;

    /// A build that exists but has not logged anything is not an error: the log
    /// view opens (and starts polling) before the first line is ever written.
    #[test]
    fn missing_output_reads_as_empty() {
        assert_eq!(slice_output(None, None), "");
        assert_eq!(slice_output(None, Some(0)), "");
        assert_eq!(slice_output(None, Some(5)), "");
    }

    #[test]
    fn without_a_startline_the_whole_output_is_returned() {
        assert_eq!(
            slice_output(Some("a\nb\nc\n".to_string()), None),
            "a\nb\nc\n"
        );
    }

    #[test]
    fn startline_skips_the_lines_the_caller_already_has() {
        let output = Some("a\nb\nc\n".to_string());
        assert_eq!(slice_output(output.clone(), Some(0)), "a\nb\nc");
        assert_eq!(slice_output(output.clone(), Some(2)), "c");
        // Caller is already up to date.
        assert_eq!(slice_output(output.clone(), Some(3)), "");
        assert_eq!(slice_output(output, Some(99)), "");
    }

    /// A nonsensical offset skips nothing rather than wrapping around to a huge
    /// one, which `as usize` would have done.
    #[test]
    fn negative_startline_skips_nothing() {
        assert_eq!(slice_output(Some("a\nb\n".to_string()), Some(-1)), "a\nb");
    }
}
