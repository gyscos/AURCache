use itertools::Itertools;
use rocket::http::Status;
use rocket::serde::json::Json;
use rocket::{State, delete, get, post};

use crate::models::authenticated::Authenticated;
use crate::models::builds::ListBuildsModel;
use crate::utils::error::{ApiError, err};
use crate::worker::liveness_timeout_secs;
use aurcache_db::helpers::worker_jobs;
use aurcache_db::prelude::Builds;
use aurcache_db::{builds, packages};
use aurcache_types::builder::Action;
use aurcache_utils::package::update::package_update;
use aurcache_utils::snapshot::SnapshotStore;
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
            (status = 404, description = "No build with that id"),
    ),
    params(
            ("buildid", description = "Id of build"),
            ("startline", description = "Number of leading lines to skip (i.e. how many the caller already has)")
    )
)]
#[get("/build/<buildid>/output?<startline>")]
pub async fn build_output(
    db: &State<DatabaseConnection>,
    buildid: i32,
    startline: Option<i32>,
    _a: Authenticated,
) -> Result<String, ApiError> {
    let db = db.inner();

    let build = Builds::find_by_id(buildid)
        .one(db)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?
        .ok_or_else(|| err(Status::NotFound, "no build with that id"))?;

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
) -> Result<Json<Vec<ListBuildsModel>>, ApiError> {
    list_builds_impl(db.inner(), None, limit, page).await
}

/// Builds for one package.
///
/// A sub-resource of the package rather than a `?pkgbase=` filter: a pkgbase
/// may contain `+`, which decodes to a space in a query value but is literal in
/// a path segment.
#[utoipa::path(
    responses((status = 200, description = "List builds for a package", body = Vec<ListBuildsModel>)),
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
) -> Result<Json<Vec<ListBuildsModel>>, ApiError> {
    let pkg = crate::package::package_id_for(db.inner(), Some(pkgbase)).await?;
    list_builds_impl(db.inner(), pkg, limit, page).await
}

async fn list_builds_impl(
    db: &DatabaseConnection,
    pkg_id: Option<i32>,
    limit: Option<u64>,
    page: Option<u64>,
) -> Result<Json<Vec<ListBuildsModel>>, ApiError> {
    let basequery = Builds::find()
        .join_rev(JoinType::InnerJoin, packages::Relation::Builds.def())
        .select_only()
        .column_as(builds::Column::Id, "id")
        .column(builds::Column::Status)
        .column_as(packages::Column::Name, "pkg_name")
        .column_as(packages::Column::Id, "pkg_id")
        .column(builds::Column::Version)
        .column(builds::Column::EndTime)
        .column(builds::Column::StartTime)
        .column(builds::Column::Platform)
        .order_by(builds::Column::StartTime, Order::Desc)
        .limit(limit)
        .offset(page.zip(limit).map(|(page, limit)| page * limit));

    let mut build = match pkg_id {
        None => basequery.into_model::<ListBuildsModel>().all(db),
        Some(pkg_id) => basequery
            .filter(builds::Column::PkgId.eq(pkg_id))
            .into_model::<ListBuildsModel>()
            .all(db),
    }
    .await
    .map_err(|e| err(Status::InternalServerError, e))?;

    annotate_waiting(db, &mut build).await?;
    Ok(Json(build))
}

/// Attach [`WaitingReason`]s to any listed build that no approved worker can
/// currently claim, so a stalled build is distinguishable from a queued one.
///
/// Best-effort by design: this is diagnostic decoration, and failing to compute
/// it must never turn a working build list into an error page.
async fn annotate_waiting(
    db: &DatabaseConnection,
    builds: &mut [ListBuildsModel],
) -> Result<(), ApiError> {
    if builds.is_empty() {
        return Ok(());
    }
    let reasons = match worker_jobs::waiting_reasons(db, liveness_timeout_secs()).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("could not compute build waiting reasons: {e}");
            return Ok(());
        }
    };
    for build in builds {
        build.waiting_reason = reasons.get(&build.id).cloned();
    }
    Ok(())
}

#[utoipa::path(
    responses(
            (status = 200, description = "Get build details"),
    ),
    params(
            ("buildid", description = "Id of build")
    )
)]
#[get("/build/<buildid>")]
pub async fn get_build(
    db: &State<DatabaseConnection>,
    buildid: i32,
    _a: Authenticated,
) -> Result<Json<ListBuildsModel>, ApiError> {
    let db = db.inner();

    let result = Builds::find()
        .join_rev(JoinType::InnerJoin, packages::Relation::Builds.def())
        .filter(builds::Column::Id.eq(buildid))
        .select_only()
        .column_as(builds::Column::Id, "id")
        .column(builds::Column::Status)
        .column_as(packages::Column::Name, "pkg_name")
        .column_as(builds::Column::PkgId, "pkg_id")
        .column(builds::Column::Version)
        .column(builds::Column::EndTime)
        .column(builds::Column::StartTime)
        .column(builds::Column::Platform)
        .into_model::<ListBuildsModel>()
        .one(db)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?
        .ok_or_else(|| err(Status::NotFound, "no build with that id"))?;

    let mut result = [result];
    annotate_waiting(db, &mut result).await?;
    let [result] = result;
    Ok(Json(result))
}

#[utoipa::path(
    responses(
            (status = 200, description = "Delete build"),
    ),
    params(
            ("buildid", description = "Id of build")
    )
)]
#[delete("/build/<buildid>")]
pub async fn delete_build(
    db: &State<DatabaseConnection>,
    buildid: i32,
    _a: Authenticated,
) -> Result<(), ApiError> {
    let db = db.inner();

    let build = Builds::find_by_id(buildid)
        .one(db)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?
        .ok_or_else(|| err(Status::NotFound, "no build with that id"))?;

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
            ("buildid", description = "Id of build")
    )
)]
#[post("/build/<buildid>/cancel")]
pub async fn cancel_build(
    tx: &State<Sender<Action>>,
    buildid: i32,
    _a: Authenticated,
) -> Result<(), ApiError> {
    tx.send(Action::Cancel(buildid))
        .map_err(|e| err(Status::InternalServerError, e))?;

    Ok(())
}

#[utoipa::path(
    responses(
            (status = 200, description = "Retry build"),
    ),
    params(
            ("buildid", description = "Id of build"),
    )
)]
#[post("/build/<buildid>/retry")]
pub async fn retry_build(
    db: &State<DatabaseConnection>,
    tx: &State<Sender<Action>>,
    store: &State<Arc<SnapshotStore>>,
    buildid: i32,
    _a: Authenticated,
) -> Result<Json<i32>, ApiError> {
    let db = db.inner();

    // Fetch the build details
    let old_build = Builds::find_by_id(buildid)
        .one(db)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?
        .ok_or_else(|| err(Status::NotFound, "no build with that id"))?;

    // Extract the platform and package ID
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
    let build_id = platform_results
        .into_iter()
        .find(|r| r.platform == platform)
        .map(|r| r.build_id)
        .ok_or_else(|| {
            err(
                Status::InternalServerError,
                "no build was enqueued for retry",
            )
        })?;

    Ok(Json(build_id))
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
