use crate::models::authenticated::Authenticated;
use crate::models::operations::ActiveOperation;
use crate::models::package::{
    AddPackage, PackagePatch, SourceFileContent, SourceFileList, SourceFileUpdate,
    SourcePreviewFileRequest, SourcePreviewRequest, UpdatePackage,
};
use crate::models::package::{
    AddPackages, AurNotFoundPackage, AurPackage, BulkAddAccepted, BulkAddEntry, BulkAddOutcome,
    BulkAddProgress, ExtendedPackage, PackageDependency, PackageFile, PackageSource, SimplePackage,
};
use crate::models::package::{
    CandidateSource, DependencyCandidate, DependencyOptions, ReplaceDependency, ReplacementVerdict,
};
use crate::utils::error::{ApiError, err};
use crate::utils::lists::split_delimited;
use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_activitylog::package_add_activity::PackageAddActivity;
use aurcache_activitylog::package_delete_activity::PackageDeleteActivity;
use aurcache_activitylog::package_update_activity::PackageUpdateActivity;
use aurcache_common::build_state::BuildTrigger;
use aurcache_db::activities::ActivityType;
use aurcache_db::helpers::builds::{
    latest_successful_version_any_platform, latest_successful_version_expr,
};
use aurcache_db::helpers::downloads::DownloadCounter;
use aurcache_db::helpers::files::total_artifact_size_expr;
use aurcache_db::helpers::operations;
use aurcache_db::packages::SourceData;
use aurcache_db::prelude::{Dependencies, Files, Packages};
use aurcache_db::{dependencies, files, packages};
use aurcache_utils::package::add::package_add;
use aurcache_utils::package::live_check::{live_check, package_remove};
use aurcache_utils::package::update::{package_resync_dependencies, package_update};
use aurcache_utils::patch::SourcePatch;
use aurcache_utils::pkg::satisfies_constraint;
use aurcache_utils::services::Services;
use aurcache_utils::snapshot::SnapshotStore;
use pacman_mirrors::platforms::Platform;
use rocket::http::Status;
use rocket::response::status;
use sea_orm::FromQueryResult;
use tokio::sync::mpsc;
use tracing::warn;

use rocket::serde::json::Json;
use rocket::{State, delete, get, patch, post, put};
use sea_orm::ActiveValue::{NotSet, Set};
use sea_orm::{ActiveModelTrait, DatabaseConnection, JoinType, Order};
use sea_orm::{
    ColumnTrait, EntityTrait, ModelTrait, QueryFilter, QueryOrder, QuerySelect, QueryTrait,
    RelationTrait,
};
use std::str::FromStr;
use std::sync::Arc;
use utoipa::OpenApi;

/// Resolve an optional pkgbase to its row id, for endpoints whose per-package
/// scope is optional (settings). `None` means "global", not "not found".
pub(crate) async fn package_id_for(
    db: &DatabaseConnection,
    pkgbase: Option<&str>,
) -> Result<Option<i32>, ApiError> {
    match pkgbase {
        Some(pkgbase) => Ok(Some(package_by_pkgbase(db, pkgbase).await?.id)),
        None => Ok(None),
    }
}

/// Resolve a package by its pkgbase, the public identifier.
///
/// Row ids are deliberately absent from the public API — they are an
/// implementation detail. The two also cannot be accepted interchangeably:
/// some real pkgbases are entirely numeric (`1337` and `67` both exist in the
/// AUR), so a route taking either would resolve those names to whatever rows
/// happened to hold those ids — a wrong-package bug rather than a not-found
/// error. Hence no id fallback.
async fn package_by_pkgbase(
    db: &DatabaseConnection,
    pkgbase: &str,
) -> Result<packages::Model, ApiError> {
    Packages::find()
        .filter(packages::Column::Name.eq(pkgbase))
        .one(db)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?
        .ok_or_else(|| err(Status::NotFound, format!("no package '{pkgbase}'")))
}

#[derive(OpenApi)]
#[openapi(paths(
    package_add_endpoint,
    packages_add_endpoint,
    active_operations,
    bulk_add_progress,
    package_update_entity_endpoint,
    package_update_endpoint,
    package_del,
    package_dependency_options,
    package_dependency_replace,
    package_list,
    get_package,
    package_source_files,
    package_source_file,
    package_source_file_update,
    package_source_preview_files,
    package_source_preview_file
))]
pub struct PackageApi;

fn normalize_build_flags(build_flags: &[String]) -> Vec<String> {
    build_flags
        .iter()
        .map(|flag| flag.trim())
        .filter(|flag| !flag.is_empty())
        .map(ToString::to_string)
        .collect()
}

/// Parse the platform names a request carried, if any.
fn parse_platforms(platforms: Option<Vec<String>>) -> Result<Option<Vec<Platform>>, ApiError> {
    platforms
        .map(|v| {
            // `ParsePlatformError` names the rejected value (`unknown
            // platform 'x'`), which a fixed string could not.
            v.into_iter()
                .map(|s| Platform::from_str(&s))
                .collect::<Result<Vec<Platform>, _>>()
                .map_err(|e| err(Status::BadRequest, e))
        })
        .transpose()
}

#[utoipa::path(
    responses(
            (status = 202, description = "Bulk add started", body = BulkAddAccepted),
            (status = 400, description = "Invalid platform name, or no sources given"),
    )
)]
/// Add many packages, returning before any of them are added.
///
/// Adding a thousand packages is minutes of `git` checkouts, so this starts the
/// work and hands back an id rather than holding the request open. The job does
/// not depend on the caller staying: closing the connection leaves it running,
/// and its progress -- including anything that failed while nobody was
/// watching -- is read back from [`bulk_add_progress`].
#[post("/packages", data = "<input>")]
pub async fn packages_add_endpoint(
    services: &State<Services>,
    input: Json<AddPackages>,
    a: Authenticated,
    al: &State<ActivityLog>,
) -> Result<status::Accepted<Json<BulkAddAccepted>>, ApiError> {
    let input = input.into_inner();
    if input.sources.is_empty() {
        return Err(err(Status::BadRequest, "No sources given"));
    }
    let platforms = parse_platforms(input.platforms)?;
    let build_flags = input.build_flags.as_deref().map(normalize_build_flags);
    let total = i32::try_from(input.sources.len()).unwrap_or(i32::MAX);

    let job_id = operations::create(&services.db, operations::KIND_BULK_ADD, total)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;

    // Everything the task needs is cloned in: it outlives this request by
    // design, so it cannot borrow from it.
    let services_task = services.inner().clone();
    let db_task = services_task.db.clone();
    let al_task = al.inner().clone();
    let username = a.username.clone();

    tokio::spawn(async move {
        let (progress_tx, mut progress_rx) = mpsc::channel(crate::utils::PROGRESS_CHANNEL_CAPACITY);
        let worker = {
            tokio::spawn(async move {
                aurcache_utils::package::bulk_add::bulk_add(
                    &services_task,
                    platforms,
                    build_flags,
                    input.sources,
                    progress_tx,
                )
                .await;
            })
        };

        // Each outcome is written as it arrives rather than batched to the end.
        // A restore runs for minutes; a job that reports nothing until it
        // finishes reports nothing at all for the whole time anyone would want
        // to watch it. The writes are trivial next to the checkout each entry
        // represents.
        let mut completed = 0_i32;
        let mut failed = 0_i32;
        while let Some(entry) = progress_rx.recv().await {
            match &entry.outcome {
                BulkAddOutcome::Failed { .. } => failed += 1,
                _ => completed += 1,
            }
            if matches!(entry.outcome, BulkAddOutcome::Added) {
                al_task.record(
                    PackageAddActivity {
                        package: entry.name.clone(),
                    },
                    ActivityType::AddPackage,
                    username.clone(),
                );
            }
            if let Err(e) =
                operations::append(&db_task, job_id, completed, failed, &[entry], false).await
            {
                warn!("could not record bulk add {job_id} progress: {e}");
            }
        }

        // The channel closed, so the run is over one way or another -- including
        // if it panicked, which is exactly when a job must not be left claiming
        // to be running.
        if let Err(e) = worker.await {
            warn!("bulk add {job_id} ended abnormally: {e}");
        }
        if let Err(e) =
            operations::append::<_, BulkAddEntry>(&db_task, job_id, completed, failed, &[], true)
                .await
        {
            warn!("could not close bulk add {job_id}: {e}");
        }
    });

    Ok(status::Accepted(Json(BulkAddAccepted {
        job_id,
        accepted: total,
    })))
}
/// Every long-running operation still in flight.
///
/// A bulk add or a restore outlives the request that started it and the page
/// that was watching, so without this there is no way back to one: a browser
/// that reloaded, navigated away, or never started the job has no id to poll.
/// Listing them is what lets a job be found again and re-attached to.
///
/// Counters only. The entries live behind the per-job endpoints, which is where
/// a caller that has decided to watch one goes; sending every log to everyone
/// listing what is running would be the expensive part of a cheap request.
#[utoipa::path(
    responses((status = 200, description = "Operations still running", body = Vec<ActiveOperation>)),
)]
#[get("/operations")]
pub async fn active_operations(
    db: &State<DatabaseConnection>,
    _a: Authenticated,
) -> Result<Json<Vec<ActiveOperation>>, ApiError> {
    let running = operations::active(db.inner())
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;

    Ok(Json(
        running
            .into_iter()
            .map(|job| ActiveOperation {
                id: job.id,
                kind: job.kind,
                created_at: job.created_at,
                total: job.total,
                completed: job.completed,
                failed: job.failed,
            })
            .collect(),
    ))
}

#[utoipa::path(
    responses(
            (status = 200, description = "Progress of a bulk add", body = BulkAddProgress),
            (status = 404, description = "No such job"),
    ),
    params(
        ("id", description = "Job id returned when the bulk add was started"),
        ("after", description = "How many entries the caller already holds"),
    )
)]
/// Read a bulk add's progress from `after` onwards.
///
/// Offset-based rather than a stream, for the same reason build output is: an
/// observer can attach late and still see everything from the beginning, and
/// one that disconnects misses nothing, because the record is what the job
/// writes to rather than a side effect of someone watching.
#[get("/packages/bulk/<id>?<after>")]
pub async fn bulk_add_progress(
    db: &State<DatabaseConnection>,
    id: i32,
    after: Option<usize>,
    _a: Authenticated,
) -> Result<Json<BulkAddProgress>, ApiError> {
    let job = operations::get(db.inner(), id)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?
        .filter(|job| job.kind == operations::KIND_BULK_ADD)
        .ok_or_else(|| err(Status::NotFound, format!("no bulk add {id}")))?;

    let entries = operations::entries_after(&job.log, after.unwrap_or(0));

    Ok(Json(BulkAddProgress {
        id: job.id,
        total: job.total,
        completed: job.completed,
        failed: job.failed,
        finished: job.finished_at.is_some(),
        entries,
    }))
}

#[utoipa::path(
    responses(
            (status = 200, description = "Add new Package"),
    )
)]
#[post("/package", data = "<input>")]
pub async fn package_add_endpoint(
    services: &State<Services>,
    input: Json<AddPackage>,
    a: Authenticated,
    al: &State<ActivityLog>,
) -> Result<(), ApiError> {
    let input = input.into_inner();
    let platforms = parse_platforms(input.platforms)?;

    let new_pkg_name = package_add(
        services,
        platforms,
        input.build_flags.as_deref().map(normalize_build_flags),
        input.source,
        input.patched_files,
    )
    .await
    // Adding is driven by user input: an unknown AUR name, an unreachable git
    // remote or an unparseable PKGBUILD are all "this request cannot be
    // fulfilled" rather than a server fault, and the flow reports them as an
    // untyped `anyhow` error we cannot tell apart from an internal one.
    .map_err(|e| err(Status::BadRequest, e))?;

    al.record(
        PackageAddActivity {
            package: new_pkg_name,
        },
        ActivityType::AddPackage,
        a.username,
    );
    Ok(())
}

#[utoipa::path(
    responses(
            (status = 200, description = "Update parts of package"),
    ),
    params(
            ("pkgbase", description = "pkgbase of the package")
    )
)]
#[patch("/package/<pkgbase>", data = "<input>")]
pub async fn package_update_entity_endpoint(
    services: &State<Services>,
    input: Json<PackagePatch>,
    pkgbase: &str,
    _a: Authenticated,
) -> Result<(), ApiError> {
    let db = &services.db;

    // We cannot move things out of Json<T>, but we can move it out of T.
    let input = input.into_inner();
    let patch_changed = input.patch.is_some();
    let pkg = package_by_pkgbase(db, pkgbase).await?;

    // Dependencies are read per architecture — a PKGBUILD can declare
    // `depends_aarch64` separately — and the graph is the union across the
    // platforms a package is built for. Changing that set therefore changes
    // which dependencies are required, so it needs the same resync a patch
    // gets. Compared canonically against the stored value so a no-op write
    // (including the same set in a different order) does not trigger a
    // needless source checkout. Validated like the add endpoints: storing an
    // unknown name would fail every later build that reads the set.
    let requested_platforms = parse_platforms(input.platforms.clone())?;
    let platforms_changed = requested_platforms.as_ref().is_some_and(|requested| {
        Platform::join_canonical(requested) != Platform::canonicalize_joined(&pkg.platforms)
    });

    // Start building the update operation
    let update_pkg = packages::ActiveModel {
        id: Set(pkg.id),
        name: input.name.map_or(NotSet, Set),
        status: input.status.map_or(NotSet, Set),
        out_of_date: input.out_of_date.map_or(NotSet, Set),
        latest_build: input.latest_build.map_or(NotSet, Set),
        build_flags: input
            .build_flags
            .as_deref()
            .map_or(NotSet, |v| Set(normalize_build_flags(v).join(";"))),
        platforms: requested_platforms.map_or(NotSet, |v| Set(Platform::join_canonical(&v))),
        patch: input.patch.map_or(NotSet, Set),
        // Everything else is `NotSet`, left untouched, so a column added
        // later cannot be cleared by a patch that never mentions it. The
        // mirrored AUR metadata in particular belongs to the version-check
        // scheduler.
        ..Default::default()
    };

    // Execute the update query
    let updated = update_pkg
        .update(db)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;

    // A patch being set or cleared here (e.g. via the "reset patch" action)
    // can change `depends`/`makedepends` without bumping the package's
    // version, and so can a change to the platform set, so keep the dependency
    // graph in sync immediately. `package_resync_dependencies` recomputes the
    // whole graph from source, which both adds newly-required dependencies and
    // drops ones no longer needed.
    if patch_changed || platforms_changed {
        package_resync_dependencies(services, &updated)
            .await
            .map_err(|e| err(Status::InternalServerError, e))?;
    }

    Ok(())
}

#[utoipa::path(
    responses(
            (status = 200, description = "List the files in a package's source, available for viewing/editing", body = SourceFileList),
    ),
    params(
            ("pkgbase", description = "pkgbase of the package")
    )
)]
#[get("/package/<pkgbase>/source/files")]
pub async fn package_source_files(
    db: &State<DatabaseConnection>,
    store: &State<Arc<SnapshotStore>>,
    pkgbase: &str,
    _a: Authenticated,
) -> Result<Json<SourceFileList>, ApiError> {
    let db = db.inner();

    let pkg = package_by_pkgbase(db, pkgbase).await?;

    let files = store
        .list_files(&pkg.source_data)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;
    // A patch that no longer parses marks nothing here; opening the file is
    // where that is reported.
    let patched = pkg
        .patch
        .as_deref()
        .and_then(|raw| SourcePatch::parse(raw).ok())
        .map(|patch| patch.paths().map(str::to_string).collect())
        .unwrap_or_default();

    Ok(Json(SourceFileList { files, patched }))
}

#[utoipa::path(
    responses(
            (status = 200, description = "Get the pristine content, and (if applicable) patched content, of a source file", body = SourceFileContent),
    ),
    params(
            ("pkgbase", description = "pkgbase of the package"),
            ("path", description = "File path relative to the source root, e.g. 'PKGBUILD'")
    )
)]
#[get("/package/<pkgbase>/source/file?<path>")]
pub async fn package_source_file(
    db: &State<DatabaseConnection>,
    store: &State<Arc<SnapshotStore>>,
    pkgbase: &str,
    path: String,
    _a: Authenticated,
) -> Result<Json<SourceFileContent>, ApiError> {
    let db = db.inner();

    let pkg = package_by_pkgbase(db, pkgbase).await?;

    store
        .read_file_with_patch_status(&pkg.source_data, pkg.patch.as_deref(), &path)
        .await
        .map(Json)
        .map_err(|e| err(Status::NotFound, e))
}

#[utoipa::path(
    responses(
            (status = 200, description = "Save an edit to a source file as part of the package's patch"),
    ),
    params(
            ("pkgbase", description = "pkgbase of the package")
    )
)]
#[put("/package/<pkgbase>/source/file", data = "<input>")]
pub async fn package_source_file_update(
    services: &State<Services>,
    pkgbase: &str,
    input: Json<SourceFileUpdate>,
    _a: Authenticated,
) -> Result<(), ApiError> {
    let db = &services.db;
    let input = input.into_inner();

    let pkg = package_by_pkgbase(db, pkgbase).await?;

    // Diff against the pristine (unpatched) file, not the currently effective
    // one, so re-saving the same edit twice is idempotent.
    let original = services
        .store
        .read_file(&pkg.source_data, None, &input.path)
        .await
        .map_err(|e| err(Status::NotFound, e))?;

    let mut patch = pkg
        .patch
        .as_deref()
        .map(SourcePatch::parse)
        .transpose()
        .map_err(|e| err(Status::InternalServerError, e))?
        .unwrap_or_default();
    patch.merge_file(&input.path, &original, &input.content);

    let new_patch = if patch.is_empty() {
        None
    } else {
        Some(
            patch
                .to_json()
                .map_err(|e| err(Status::InternalServerError, e))?,
        )
    };

    // No need to validate the patch applies cleanly here: it was just
    // diffed fresh against the current pristine content above, so applying
    // it back is guaranteed to succeed.
    let update_pkg = packages::ActiveModel {
        id: Set(pkg.id),
        patch: Set(new_patch.clone()),
        ..Default::default()
    };
    update_pkg
        .update(db)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;

    // A patch can change `depends`/`makedepends` without bumping the
    // package's version, so resync the dependency graph immediately rather
    // than waiting for the next explicit "update" trigger.
    let mut resynced_pkg = pkg;
    resynced_pkg.patch = new_patch;
    package_resync_dependencies(services, &resynced_pkg)
        .await
        .map_err(|e| {
            err(
                Status::InternalServerError,
                format!("Patch saved, but failed to resync dependencies: {e}"),
            )
        })?;

    Ok(())
}

#[utoipa::path(
    responses(
            (status = 200, description = "List source files for a not-yet-added source", body = SourceFileList),
    )
)]
#[post("/package/source/preview/files", data = "<input>")]
pub async fn package_source_preview_files(
    store: &State<Arc<SnapshotStore>>,
    input: Json<SourcePreviewRequest>,
    _a: Authenticated,
) -> Result<Json<SourceFileList>, ApiError> {
    let files = store
        .list_files(&input.source)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;

    Ok(Json(SourceFileList {
        files,
        patched: Vec::new(),
    }))
}

#[utoipa::path(
    responses(
            (status = 200, description = "Get the pristine content of a source file for a not-yet-added source", body = SourceFileContent),
    )
)]
#[post("/package/source/preview/file", data = "<input>")]
pub async fn package_source_preview_file(
    store: &State<Arc<SnapshotStore>>,
    input: Json<SourcePreviewFileRequest>,
    _a: Authenticated,
) -> Result<Json<SourceFileContent>, ApiError> {
    let input = input.into_inner();
    let original_content = store
        .read_file(&input.source, None, &input.path)
        .await
        .map_err(|e| err(Status::NotFound, e))?;

    Ok(Json(SourceFileContent {
        path: input.path,
        original_content,
        patched_content: None,
        patch_error: None,
        stored_patch: None,
    }))
}

#[utoipa::path(
    responses(
            (status = 200, description = "Update package to newest AUR version"),
    ),
    params(
            ("pkgbase", description = "pkgbase of the package")
    )
)]
#[post("/package/<pkgbase>/update", data = "<input>")]
pub async fn package_update_endpoint(
    services: &State<Services>,
    pkgbase: &str,
    input: Json<UpdatePackage>,
    a: Authenticated,
    al: &State<ActivityLog>,
) -> Result<Json<Vec<i32>>, ApiError> {
    let db = &services.db;

    let pkg_model: packages::Model = package_by_pkgbase(db, pkgbase).await?;
    let package_name = pkg_model.name.clone();
    let forced = input.force;

    // An operator's Update or Rebuild, from the UI or the CLI.
    let pkg_update = package_update(services, pkg_model, forced, BuildTrigger::User)
        .await
        .map(|results| {
            Json(
                results
                    .into_iter()
                    .filter(|r| r.enqueued)
                    .map(|r| r.build_number)
                    .collect::<Vec<_>>(),
            )
        })
        // Same as adding: "already up to date", an unresolvable source, or a
        // patch that no longer applies are all caller-visible conditions.
        .map_err(|e| err(Status::BadRequest, e))?;

    al.record(
        PackageUpdateActivity {
            package: package_name,
            forced,
        },
        ActivityType::UpdatePackage,
        a.username,
    );
    Ok(pkg_update)
}

#[utoipa::path(
    responses(
            (status = 200, description = "Remove direct request flag from package and live-check it"),
    ),
    params(
            ("pkgbase", description = "pkgbase of the package")
    )
)]
#[delete("/package/<pkgbase>")]
pub async fn package_del(
    services: &State<Services>,
    pkgbase: &str,
    a: Authenticated,
    al: &State<ActivityLog>,
) -> Result<(), ApiError> {
    let db = &services.db;

    // query this before removing package ownership!
    let pkg = package_by_pkgbase(db, pkgbase).await?;

    // Deletes the package -- checkout included -- only if nothing depends on
    // it; otherwise it merely stops being requested and keeps everything.
    package_remove(db, &services.store, &services.repo, pkg.id)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;

    al.record(
        PackageDeleteActivity { package: pkg.name },
        ActivityType::RemovePackage,
        a.username,
    );

    Ok(())
}
#[utoipa::path(
    responses(
            (status = 200, description = "List of all packages", body = [SimplePackage]),
    ),
    params(
            ("limit", description = "limit of packages"),
            ("page", description = "page of packages"),
            ("dependencies", description = "include packages that are only here as dependencies")
    )
)]
#[get("/packages/list?<limit>&<page>&<dependencies>")]
pub async fn package_list(
    db: &State<DatabaseConnection>,
    limit: Option<u64>,
    page: Option<u64>,
    dependencies: Option<bool>,
    _a: Authenticated,
) -> Result<Json<Vec<SimplePackage>>, ApiError> {
    let db = db.inner();

    list_packages(db, limit, page, dependencies.unwrap_or(false))
        .await
        .map(Json)
        .map_err(|e| err(Status::InternalServerError, e))
}

/// List packages, by default only the ones somebody asked for.
///
/// `dependencies` opts into the rest. It defaults to off because that is what
/// every existing caller means by "the packages": a repository's dependency
/// closure is usually the larger part of it, and reading the list as the set of
/// things being maintained is the common case.
async fn list_packages(
    db: &DatabaseConnection,
    limit: Option<u64>,
    page: Option<u64>,
    dependencies: bool,
) -> Result<Vec<SimplePackage>, sea_orm::DbErr> {
    // Both of these are correlated subqueries over the package row, so they
    // report per row without a query per package. The "successful builds only"
    // rule matters: taking the newest attempt regardless of outcome meant a
    // failed build of a new version reported that version as built, and the
    // version check then compared upstream against it and cleared the
    // out-of-date flag — so an upstream release whose first build failed
    // stopped being flagged at all.

    let all: Vec<SimplePackage> = Packages::find()
        .select_only()
        .column(packages::Column::Name)
        .column(packages::Column::Id)
        .column(packages::Column::Status)
        .column_as(packages::Column::OutOfDate, "outofdate")
        .column_as(packages::Column::UpstreamVersion, "upstream_version")
        .column(packages::Column::DirectlyRequested)
        .apply_if((!dependencies).then_some(()), |query, ()| {
            query.filter(packages::Column::DirectlyRequested.eq(true))
        })
        // No COALESCE to an empty string: a package with no build has no
        // version, and `null` says that where `""` is indistinguishable from a
        // build that produced a blank one. The detail endpoint below already
        // reported it this way, so coercing here made one field mean two
        // different things depending on which route you asked.
        .column_as(latest_successful_version_expr(), "latest_version")
        .column_as(total_artifact_size_expr(), "total_size")
        .order_by(packages::Column::OutOfDate, Order::Desc)
        .order_by(packages::Column::Id, Order::Desc)
        .limit(limit)
        // Saturating: user input must never reach unchecked arithmetic — a
        // huge `page` would wrap the offset in release or panic in debug.
        .offset(
            page.zip(limit)
                .map(|(page, limit)| page.saturating_mul(limit)),
        )
        .into_model::<SimplePackage>()
        .all(db)
        .await?;

    Ok(all)
}

async fn list_package_relations(
    db: &DatabaseConnection,
    pkg_id: i32,
    direction: RelationDirection,
) -> Result<Vec<PackageDependency>, sea_orm::DbErr> {
    let (filter_col, relation) = match direction {
        RelationDirection::Dependencies => (
            dependencies::Column::DependentId,
            dependencies::Relation::Dependee.def(),
        ),
        RelationDirection::Dependents => (
            dependencies::Column::DependeeId,
            dependencies::Relation::Dependent.def(),
        ),
    };

    let rows = Dependencies::find()
        .select_only()
        .column_as(packages::Column::Id, "id")
        .column_as(packages::Column::Name, "name")
        .column(dependencies::Column::VersionConstraint)
        .column_as(packages::Column::Status, "status")
        // The repository version of the joined package, on the same rule.
        .column_as(latest_successful_version_expr(), "built_version")
        .join(JoinType::InnerJoin, relation)
        .filter(filter_col.eq(pkg_id))
        .order_by_asc(dependencies::Column::Id)
        .into_model::<DependencyRow>()
        .all(db)
        .await?;

    Ok(rows
        .into_iter()
        .map(|row| {
            // Deliberately the same rule the builder applies when it decides
            // whether to promote a dependent, so the page cannot disagree with
            // what the queue actually does. Unlike the builder this is not
            // scoped to one platform: the page is not either, and a package
            // built for several reports the newest success across them.
            let satisfied = row
                .built_version
                .as_deref()
                .is_some_and(|built| satisfies_constraint(built, &row.version_constraint));
            PackageDependency {
                id: row.id,
                name: row.name,
                version_constraint: row.version_constraint,
                status: row.status,
                built_version: row.built_version,
                satisfied,
            }
        })
        .collect())
}

/// The columns behind [`PackageDependency`]; `satisfied` is derived, not stored.
#[derive(FromQueryResult)]
struct DependencyRow {
    id: i32,
    name: String,
    version_constraint: String,
    status: i32,
    built_version: Option<String>,
}

#[derive(Copy, Clone)]
enum RelationDirection {
    Dependencies,
    Dependents,
}

/// Downloads for a package, over the file names it produces.
///
/// A package with no split list produces one file named after itself; one with
/// a split list produces those and nothing named after the pkgbase.
async fn download_total(
    db: &DatabaseConnection,
    buffer: &Arc<DownloadCounter>,
    name: &str,
    split: Option<&[String]>,
) -> Result<i64, ApiError> {
    let names: Vec<String> = match split {
        Some(names) if !names.is_empty() => names.to_vec(),
        _ => vec![name.to_string()],
    };
    buffer
        .total_for_packages(db, &names)
        .await
        .map_err(|e| err(Status::InternalServerError, e))
}

#[utoipa::path(
    responses(
            (status = 200, description = "Get package details
This requires 1 API call to the AUR (rate limited 4000 per day)
https://wiki.archlinux.org/title/Aurweb_RPC_interface", body = ExtendedPackage),
    ),
    params(
            ("pkgbase", description = "pkgbase of the package")
    )
)]
#[get("/package/<pkgbase>")]
pub async fn get_package(
    db: &State<DatabaseConnection>,
    downloads: &State<Arc<DownloadCounter>>,
    pkgbase: &str,
    _a: Authenticated,
) -> Result<Json<ExtendedPackage>, ApiError> {
    let db = db.inner();

    let pkg = package_by_pkgbase(db, pkgbase).await?;

    // Independent reads over one pooled connection: serial awaits would pay
    // each round trip in turn for queries that share only the package id.
    let (latest_version, dependencies, dependents, files) = tokio::join!(
        latest_successful_version_any_platform(db, pkg.id),
        list_package_relations(db, pkg.id, RelationDirection::Dependencies),
        list_package_relations(db, pkg.id, RelationDirection::Dependents),
        package_files(db, pkg.id),
    );
    let latest_version = latest_version
        .map_err(|e| err(Status::InternalServerError, e))?
        // Same rule as the list query: an enqueued build's empty version is not
        // a version.
        .filter(|v| !v.is_empty());
    let dependencies = dependencies.map_err(|e| err(Status::InternalServerError, e))?;
    let dependents = dependents.map_err(|e| err(Status::InternalServerError, e))?;
    let files = files.map_err(|e| err(Status::InternalServerError, e))?;

    let has_patch = pkg.patch.is_some();

    let (package_source, upstream_version) = package_source_and_version(&pkg)?;

    // Borrowed: the stored string is not used after this, only the parse.
    let split_packages: Option<Vec<String>> = pkg
        .split_packages
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok());

    // Read before the struct below consumes `pkg.name`.
    let download_count =
        download_total(db, downloads, &pkg.name, split_packages.as_deref()).await?;

    let ext_pkg = ExtendedPackage {
        // Mirrored from the package's checkout, so a git-sourced package
        // describes itself as fully as an AUR one.
        description: pkg.source_description.clone(),
        project_url: pkg.source_project_url.clone(),
        licenses: pkg.source_licenses.clone(),
        maintainer: pkg.source_maintainer.clone(),
        first_submitted: pkg.source_first_submitted,
        last_modified: pkg.source_last_modified,
        id: pkg.id,
        name: pkg.name,
        directly_requested: pkg.directly_requested,
        status: pkg.status,
        outofdate: pkg.out_of_date,
        latest_version,
        package_source,
        selected_platforms: split_delimited(&pkg.platforms, ';'),
        selected_build_flags: Some(split_delimited(&pkg.build_flags, ';')),
        upstream_version,
        split_packages,
        files,
        dependencies,
        dependents,
        has_patch,
        // Over the names this package actually produces: a split package's
        // downloads are its subpackages' downloads, and there is no file named
        // after the pkgbase to count.
        downloads: download_count,
    };

    Ok(Json(ext_pkg))
}

/// The `PackageSource` and upstream version for a package row, resolved
/// entirely from persisted fields — no AUR lookup happens on this path. The
/// version-check scheduler mirrors the same assignment, and `package::add`
/// fills the row in immediately, so the page cannot drift from either; the AUR
/// call it used to make was ~128ms of a ~130ms response and one of the AUR's
/// 4000 daily calls per page view.
fn package_source_and_version(
    pkg: &packages::Model,
) -> Result<(PackageSource, Option<String>), ApiError> {
    match &pkg.source_data {
        SourceData::Aur { .. } => {
            // The last check did not find it in the AUR. Its page still renders
            // — the metadata comes from the checkout — but it says the package
            // is gone from upstream.
            let source = if pkg.aur_missing == Some(true) {
                PackageSource::AurNotFound(AurNotFoundPackage {})
            } else {
                PackageSource::Aur(aur_source(pkg))
            };
            Ok((source, pkg.upstream_version.clone()))
        }
        SourceData::Git { spec } => Ok((
            PackageSource::Git(spec.clone()),
            // How current this version is depends on the version-check
            // interval; `None` means no check has run yet.
            pkg.upstream_version.clone(),
        )),
        SourceData::Upload { .. } => Err(err(
            Status::NotImplemented,
            "Upload sources are not yet supported",
        )),
    }
}

/// The artifacts currently in the repository for a package.
///
/// Read straight from the `files` rows rather than by listing the repository
/// directory: those rows are what the ingest and the delete path both maintain,
/// so this is the same list the server acts on, and the size comes with them
/// instead of costing a `stat` per artifact per page view.
///
/// Ordered by filename so the page is stable across requests; a split package
/// otherwise lists its parts in whatever order the rows came back in.
async fn package_files(
    db: &DatabaseConnection,
    pkg_id: i32,
) -> Result<Vec<PackageFile>, sea_orm::DbErr> {
    Ok(Files::find()
        .filter(files::Column::PackageId.eq(pkg_id))
        .order_by_asc(files::Column::Filename)
        .all(db)
        .await?
        .into_iter()
        .map(|f| PackageFile {
            filename: f.filename,
            platform: f.platform.to_string(),
            size: f.size,
        })
        .collect())
}

/// The AUR page for a pkgbase. Derived, never fetched.
fn aur_pkgbase_url(pkgbase: &str) -> String {
    format!("https://aur.archlinux.org/pkgbase/{pkgbase}")
}

/// The AUR-specific part of a package's source description.
///
/// Everything else a package page shows comes from its checkout — see
/// `aurcache_utils::package::metadata` — so this is only the flag the AUR
/// alone knows and the link back to it.
fn aur_source(pkg: &packages::Model) -> AurPackage {
    AurPackage {
        name: pkg.name.clone(),
        aur_flagged_outdated: pkg.aur_flagged_outdated.unwrap_or(false),
        aur_url: aur_pkgbase_url(&pkg.name),
    }
}

#[cfg(test)]
mod tests {
    use super::{RelationDirection, list_package_relations, list_packages};
    use aurcache_db::migration::Migrator;
    use aurcache_db::packages::SourceData;
    use aurcache_db::{dependencies, packages};
    use sea_orm::{ActiveModelTrait, Database, Set, TryIntoModel};
    use sea_orm_migration::MigratorTrait;

    #[tokio::test]
    async fn package_list_hides_dependencies_unless_asked() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

        packages::ActiveModel {
            name: Set("visible-package".to_string()),
            status: Set(1),
            out_of_date: Set(0),
            upstream_version: Set(Some("1.0.0".to_string())),
            latest_build: Set(None),
            build_flags: Set("--noconfirm".to_string()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(packages::SourceType::Aur),
            source_data: Set(SourceData::Aur {
                name: "visible-package".into(),
            }),
            directly_requested: Set(true),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        packages::ActiveModel {
            name: Set("hidden-dependency".to_string()),
            status: Set(1),
            out_of_date: Set(0),
            upstream_version: Set(Some("1.0.0".to_string())),
            latest_build: Set(None),
            build_flags: Set("--noconfirm".to_string()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(packages::SourceType::Aur),
            source_data: Set(SourceData::Aur {
                name: "hidden-dependency".into(),
            }),
            directly_requested: Set(false),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        let packages = list_packages(&db, None, None, false).await.unwrap();

        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "visible-package");
        assert!(packages[0].directly_requested);

        // Opting in returns both, and each says which kind it is -- the list
        // is only worth widening if the two can still be told apart.
        let mut packages = list_packages(&db, None, None, true).await.unwrap();
        packages.sort_by(|a, b| a.name.cmp(&b.name));

        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "hidden-dependency");
        assert!(!packages[0].directly_requested);
        assert_eq!(packages[1].name, "visible-package");
        assert!(packages[1].directly_requested);
    }

    #[tokio::test]
    async fn package_dependencies_include_link_targets() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

        let parent = packages::ActiveModel {
            name: Set("parent".to_string()),
            status: Set(1),
            out_of_date: Set(0),
            upstream_version: Set(Some("1.0.0".to_string())),
            latest_build: Set(None),
            build_flags: Set("--noconfirm".to_string()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(packages::SourceType::Aur),
            source_data: Set(SourceData::Aur {
                name: "parent".into(),
            }),
            directly_requested: Set(true),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap()
        .try_into_model()
        .unwrap();

        let child = packages::ActiveModel {
            name: Set("child".to_string()),
            status: Set(1),
            out_of_date: Set(0),
            upstream_version: Set(Some("2.0.0".to_string())),
            latest_build: Set(None),
            build_flags: Set("--noconfirm".to_string()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(packages::SourceType::Aur),
            source_data: Set(SourceData::Aur {
                name: "child".into(),
            }),
            directly_requested: Set(false),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap()
        .try_into_model()
        .unwrap();

        dependencies::ActiveModel {
            dependent_id: Set(parent.id),
            dependee_id: Set(child.id),
            version_constraint: Set(">=2.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        let deps = list_package_relations(&db, parent.id, RelationDirection::Dependencies)
            .await
            .unwrap();

        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].id, child.id);
        assert_eq!(deps[0].name, "child");
        assert_eq!(deps[0].version_constraint, ">=2.0");
    }

    #[tokio::test]
    async fn package_dependents_include_reverse_link_targets() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

        let dependency = packages::ActiveModel {
            name: Set("dependency".to_string()),
            status: Set(1),
            out_of_date: Set(0),
            upstream_version: Set(Some("1.0.0".to_string())),
            latest_build: Set(None),
            build_flags: Set("--noconfirm".to_string()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(packages::SourceType::Aur),
            source_data: Set(SourceData::Aur {
                name: "dependency".into(),
            }),
            directly_requested: Set(false),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap()
        .try_into_model()
        .unwrap();

        let parent = packages::ActiveModel {
            name: Set("parent".to_string()),
            status: Set(1),
            out_of_date: Set(0),
            upstream_version: Set(Some("2.0.0".to_string())),
            latest_build: Set(None),
            build_flags: Set("--noconfirm".to_string()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(packages::SourceType::Aur),
            source_data: Set(SourceData::Aur {
                name: "parent".into(),
            }),
            directly_requested: Set(true),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap()
        .try_into_model()
        .unwrap();

        dependencies::ActiveModel {
            dependent_id: Set(parent.id),
            dependee_id: Set(dependency.id),
            version_constraint: Set(">=1.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        let dependents = list_package_relations(&db, dependency.id, RelationDirection::Dependents)
            .await
            .unwrap();

        assert_eq!(dependents.len(), 1);
        assert_eq!(dependents[0].id, parent.id);
        assert_eq!(dependents[0].name, "parent");
        assert_eq!(dependents[0].version_constraint, ">=1.0");
    }
}

#[cfg(test)]
mod file_tests {
    use super::package_files;
    use aurcache_db::files;
    use aurcache_db::migration::Migrator;
    use pacman_mirrors::platforms::Platform;
    use sea_orm::{ActiveModelTrait, ConnectionTrait, Database, DatabaseConnection, Set};
    use sea_orm_migration::MigratorTrait;

    /// A migrated database with the packages these tests hang files off.
    ///
    /// The packages have to exist: `files.package_id` references them, and
    /// SQLite enforces that here because sqlx opens connections with
    /// `foreign_keys` on.
    async fn db_with_packages(ids: &[i32]) -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        for id in ids {
            db.execute_unprepared(&format!(
                "INSERT INTO packages \
                 (id, name, status, out_of_date, build_flags, platforms, source_type, source_data, directly_requested) \
                 VALUES ({id}, 'p{id}', 0, 0, '', 'x86_64', 'aur', '{{\"type\":\"aur\",\"name\":\"p{id}\"}}', 1)"
            ))
            .await
            .unwrap();
        }
        db
    }

    async fn file(db: &DatabaseConnection, name: &str, pkg_id: i32, size: Option<i64>) {
        files::ActiveModel {
            filename: Set(name.to_string()),
            platform: Set(Platform::X86_64),
            package_id: Set(pkg_id),
            size: Set(size),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap();
    }

    /// The page lists one row per artifact -- a split package has several --
    /// scoped to the package that owns them.
    #[tokio::test]
    async fn lists_only_this_package_s_artifacts() {
        let db = db_with_packages(&[1, 2]).await;

        file(&db, "hello-1.0-1-x86_64.pkg.tar.zst", 1, Some(1000)).await;
        file(&db, "hello-docs-1.0-1-x86_64.pkg.tar.zst", 1, Some(24)).await;
        file(&db, "other-1.0-1-x86_64.pkg.tar.zst", 2, Some(99)).await;

        let listed = package_files(&db, 1).await.unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(
            listed.iter().map(|f| f.size).sum::<Option<i64>>(),
            Some(1024)
        );
        assert!(listed.iter().all(|f| f.platform == "x86_64"));
    }

    /// Ordered by filename, so a split package does not reshuffle its parts
    /// between two loads of the same page.
    #[tokio::test]
    async fn artifacts_come_back_in_a_stable_order() {
        let db = db_with_packages(&[1]).await;

        file(&db, "zzz-1.0-1-x86_64.pkg.tar.zst", 1, Some(1)).await;
        file(&db, "aaa-1.0-1-x86_64.pkg.tar.zst", 1, Some(2)).await;

        let listed = package_files(&db, 1).await.unwrap();
        let names: Vec<&str> = listed.iter().map(|f| f.filename.as_str()).collect();
        assert_eq!(
            names,
            [
                "aaa-1.0-1-x86_64.pkg.tar.zst",
                "zzz-1.0-1-x86_64.pkg.tar.zst"
            ]
        );
    }

    /// The list column totals a package's artifacts, and applies the same
    /// all-or-nothing rule the detail page does: one unrecorded size makes the
    /// whole total unknown, rather than reporting a sum smaller than the files
    /// it covers. Exercised through the real SQL, since the rule lives in a
    /// `CASE` expression rather than in Rust.
    #[tokio::test]
    async fn the_list_totals_artifact_sizes_all_or_nothing() {
        use super::list_packages;
        use aurcache_db::packages;
        use aurcache_db::packages::SourceData;

        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

        let mut ids = std::collections::HashMap::new();
        for name in ["complete", "partial", "unbuilt"] {
            let row = packages::ActiveModel {
                name: Set(name.to_string()),
                status: Set(1),
                out_of_date: Set(0),
                upstream_version: Set(Some("1.0.0".to_string())),
                latest_build: Set(None),
                build_flags: Set(String::new()),
                platforms: Set("x86_64".to_string()),
                source_type: Set(packages::SourceType::Aur),
                source_data: Set(SourceData::Aur { name: name.into() }),
                directly_requested: Set(true),
                split_packages: Set(None),
                ..Default::default()
            }
            .insert(&db)
            .await
            .unwrap();
            ids.insert(name, row.id);
        }

        file(&db, "complete-a.pkg.tar.zst", ids["complete"], Some(1000)).await;
        file(&db, "complete-b.pkg.tar.zst", ids["complete"], Some(24)).await;
        file(&db, "partial-a.pkg.tar.zst", ids["partial"], Some(1000)).await;
        file(&db, "partial-b.pkg.tar.zst", ids["partial"], None).await;

        let listed = list_packages(&db, None, None, true).await.unwrap();
        let total = |name: &str| {
            listed
                .iter()
                .find(|p| p.name == name)
                .unwrap_or_else(|| panic!("{name} missing from the list"))
                .total_size
        };

        assert_eq!(total("complete"), Some(1024));
        assert_eq!(
            total("partial"),
            None,
            "a partial sum reached the column instead of being suppressed"
        );
        assert_eq!(
            total("unbuilt"),
            None,
            "a package with no artifacts should have no total, not zero"
        );
    }

    /// A row written before the size column carries `None`, which must survive
    /// to the response rather than being flattened to a zero-byte file.
    #[tokio::test]
    async fn an_unrecorded_size_stays_unknown() {
        let db = db_with_packages(&[1]).await;

        file(&db, "hello-1.0-1-x86_64.pkg.tar.zst", 1, None).await;

        let listed = package_files(&db, 1).await.unwrap();
        assert_eq!(listed[0].size, None);
    }
}

/// Every name a package answers to: its own, its split packages, and its
/// `provides` with any `=version` dropped.
///
/// This is what a replacement is measured against. A dependent's edge records
/// the constraint but not which of these names it declared, so covering all of
/// them is the only way to know a replacement covers a given dependent.
fn provided_names(pkg: &packages::Model) -> Vec<String> {
    let mut names = vec![pkg.name.clone()];
    names.extend(json_string_list(pkg.split_packages.as_deref()));
    names.extend(
        json_string_list(pkg.provides.as_deref())
            .into_iter()
            .map(|entry| match entry.split_once('=') {
                Some((name, _version)) => name.to_string(),
                None => entry,
            }),
    );
    names.sort_unstable();
    names.dedup();
    names
}

fn json_string_list(raw: Option<&str>) -> Vec<String> {
    raw.and_then(|value| serde_json::from_str::<Vec<String>>(value).ok())
        .unwrap_or_default()
}

async fn official_holds(services: &Services, name: &str) -> Result<bool, ApiError> {
    services
        .client
        .official
        .holds(name)
        .await
        .map_err(|e| err(Status::BadGateway, e))
}

/// Resolve one dependency edge by the two package names on its ends.
async fn dependency_edge(
    db: &DatabaseConnection,
    dependent: &str,
    dependency: &str,
) -> Result<(packages::Model, packages::Model, dependencies::Model), ApiError> {
    // The two endpoints are independent point lookups; the edge query below is
    // what genuinely depends on both.
    let (dependent, current) = tokio::join!(
        package_by_pkgbase(db, dependent),
        package_by_pkgbase(db, dependency),
    );
    let (dependent, current) = (dependent?, current?);
    let edge = Dependencies::find()
        .filter(dependencies::Column::DependentId.eq(dependent.id))
        .filter(dependencies::Column::DependeeId.eq(current.id))
        .one(db)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?
        .ok_or_else(|| {
            err(
                Status::NotFound,
                format!("{} does not depend on {}", dependent.name, current.name),
            )
        })?;
    Ok((dependent, current, edge))
}

/// The names `dependent` declares that `current` answers to.
///
/// The edge records the constraint but not the name behind it, so this reads
/// the dependent's source to recover it -- one source, already cached, for the
/// package whose page is asking. Empty means nothing the dependent declares
/// matches any more, which is a stale edge: droppable, but with nothing to
/// search for a replacement by.
async fn declared_names_for_edge(
    services: &Services,
    dependent: &packages::Model,
    current: &packages::Model,
) -> Result<Vec<String>, ApiError> {
    let sourceinfo = services
        .store
        .sourceinfo(&dependent.source_data, dependent.patch.as_deref())
        .await
        .map_err(|e| err(Status::BadGateway, e))?;
    let deps = aurcache_deps::deps_from_srcinfo(
        &sourceinfo,
        &aurcache_utils::pkg::architectures_for_platforms(&dependent.platforms),
    );
    let declared = aurcache_utils::pkg::DependencySet::of(&deps)
        .map_err(|e| err(Status::InternalServerError, e))?;

    let answers = provided_names(current);
    Ok(declared
        .names
        .into_iter()
        .filter(|name| answers.contains(name))
        .collect())
}

fn candidate_verdict(version: Option<&str>, constraint: &str) -> ReplacementVerdict {
    if constraint.is_empty() {
        return ReplacementVerdict::Satisfied;
    }
    match version {
        Some(version) if satisfies_constraint(version, constraint) => ReplacementVerdict::Satisfied,
        Some(_) => ReplacementVerdict::Unsatisfied,
        None => ReplacementVerdict::Unknown,
    }
}

#[utoipa::path(
    responses(
            (status = 200, description = "What could take over this dependency", body = DependencyOptions),
    ),
    params(
            ("pkgbase", description = "pkgbase of the depending package"),
            ("dependency", description = "pkgbase of the dependency to replace")
    )
)]
#[get("/package/<pkgbase>/dependency/<dependency>/options")]
pub async fn package_dependency_options(
    services: &State<Services>,
    pkgbase: &str,
    dependency: &str,
    _a: Authenticated,
) -> Result<Json<DependencyOptions>, ApiError> {
    let (dependent, current, edge) = dependency_edge(&services.db, pkgbase, dependency).await?;
    let declared_names = declared_names_for_edge(services, &dependent, &current).await?;

    let mut official = Vec::new();
    for name in &declared_names {
        if official_holds(services, name).await? {
            official.push(name.clone());
        }
    }

    // Tracked first: they are already here, so choosing one builds nothing new.
    // A package carrying a declared name outright leads one that merely
    // provides it, on the same rule resolution ranks by.
    let mut candidates = Vec::new();
    let tracked = Packages::find()
        .all(&services.db)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;
    let mut tracked_matches: Vec<&packages::Model> = tracked
        .iter()
        .filter(|package| package.id != current.id && package.id != dependent.id)
        .filter(|package| {
            provided_names(package)
                .iter()
                .any(|name| declared_names.contains(name))
        })
        .collect();
    tracked_matches.sort_by(|a, b| {
        let carries = |package: &packages::Model| !declared_names.contains(&package.name);
        carries(a)
            .cmp(&carries(b))
            .then_with(|| a.name.cmp(&b.name))
    });
    for package in tracked_matches {
        let version = latest_successful_version_any_platform(&services.db, package.id)
            .await
            .map_err(|e| err(Status::InternalServerError, e))?;
        candidates.push(DependencyCandidate {
            pkgbase: package.name.clone(),
            source: CandidateSource::Tracked,
            verdict: candidate_verdict(version.as_deref(), &edge.version_constraint),
            version,
        });
    }

    // Then the AUR, in the order resolution itself would rank them, minus
    // everything already offered above.
    let tracked_names: std::collections::HashSet<&str> = tracked
        .iter()
        .map(|package| package.name.as_str())
        .collect();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut aur_error = None;
    for name in &declared_names {
        let providers = match services.client.aur_providers(name).await {
            Ok(providers) => providers,
            // Best-effort: an unreachable AUR must not take the tracked
            // candidates down with it, and the caller is told which half of
            // the answer is missing rather than left to read an empty list as
            // "nothing exists".
            Err(e) => {
                aur_error = Some(e.to_string());
                break;
            }
        };
        for package in providers {
            if tracked_names.contains(package.package_base.as_str())
                || !seen.insert(package.package_base.clone())
            {
                continue;
            }
            candidates.push(DependencyCandidate {
                pkgbase: package.package_base,
                source: CandidateSource::Aur,
                verdict: candidate_verdict(Some(&package.version), &edge.version_constraint),
                version: Some(package.version),
            });
        }
    }

    Ok(Json(DependencyOptions {
        dependent: dependent.name,
        current: current.name,
        version_constraint: edge.version_constraint,
        declared_names,
        official,
        candidates,
        aur_error,
    }))
}

#[utoipa::path(
    responses(
            (status = 200, description = "Point the dependency somewhere else, or drop it"),
    ),
    params(
            ("pkgbase", description = "pkgbase of the depending package"),
            ("dependency", description = "pkgbase of the dependency being replaced")
    )
)]
#[put("/package/<pkgbase>/dependency/<dependency>", data = "<input>")]
pub async fn package_dependency_replace(
    services: &State<Services>,
    pkgbase: &str,
    dependency: &str,
    input: Json<ReplaceDependency>,
    _a: Authenticated,
) -> Result<(), ApiError> {
    let (dependent, current, edge) = dependency_edge(&services.db, pkgbase, dependency).await?;
    let declared_names = declared_names_for_edge(services, &dependent, &current).await?;

    match input.into_inner().replacement {
        None => {
            // Dropping is only honest when nothing has to be built for the
            // name any more. Otherwise the edge would come straight back the
            // next time the dependent is resolved, and the button would look
            // like it had failed.
            for name in &declared_names {
                if !official_holds(services, name).await? {
                    return Err(err(
                        Status::BadRequest,
                        format!(
                            "'{name}' is not published by the official repositories, so this dependency cannot be dropped"
                        ),
                    ));
                }
            }
            edge.delete(&services.db)
                .await
                .map_err(|e| err(Status::InternalServerError, e))?;
        }
        Some(replacement) => {
            if replacement == current.name {
                return Err(err(
                    Status::BadRequest,
                    format!("{} already depends on {replacement}", dependent.name),
                ));
            }
            if replacement == dependent.name {
                return Err(err(
                    Status::BadRequest,
                    "a package cannot depend on itself".to_string(),
                ));
            }
            if declared_names.is_empty() {
                return Err(err(
                    Status::BadRequest,
                    format!(
                        "{} no longer declares anything {} answers to, so there is nothing to replace -- drop the dependency instead",
                        dependent.name, current.name
                    ),
                ));
            }

            let package = ensure_replacement_exists(services, &dependent, &replacement).await?;
            let answers = provided_names(&package);
            if !declared_names.iter().any(|name| answers.contains(name)) {
                return Err(err(
                    Status::BadRequest,
                    format!(
                        "{replacement} answers to none of {}, so the edge would be undone at the next update",
                        declared_names.join(", ")
                    ),
                ));
            }

            repoint_edge(&services.db, edge, dependent.id, package.id)
                .await
                .map_err(|e| err(Status::InternalServerError, e))?;
        }
    }

    // The dependent's queue entry was made against the edge that just changed,
    // so it may now be wrong in either direction: free to start because what
    // held it up is no longer its dependency, or obliged to wait because the
    // replacement has not been built yet. Before `live_check`, which may delete
    // the old dependency and everything that hung off it.
    aurcache_utils::worker_complete::resync_pending_builds(&services.db, dependent.id)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;

    // The usual collection, now that the old dependency may be holding nothing
    // up. This is what makes emptying a package's dependents remove it: patch
    // the last edge away and the package goes with it, without a second
    // endpoint that knows how to remove packages.
    live_check(&services.db, &services.store, &services.repo, current.id)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;

    Ok(())
}

/// Move `edge` onto `replacement_id`.
///
/// There is one row per (dependent, dependency) pair, and the dependent may
/// already depend on the replacement under some other name, so an edge that
/// would collide is merged into the one already there rather than duplicated.
/// Neither constraint is merged into the other: both are recomputed from the
/// dependent's declarations at its next resync, and guessing here would only
/// disagree with that in the meantime.
async fn repoint_edge(
    db: &DatabaseConnection,
    edge: dependencies::Model,
    dependent_id: i32,
    replacement_id: i32,
) -> Result<(), sea_orm::DbErr> {
    let collides = Dependencies::find()
        .filter(dependencies::Column::DependentId.eq(dependent_id))
        .filter(dependencies::Column::DependeeId.eq(replacement_id))
        .one(db)
        .await?
        .is_some();

    if collides {
        edge.delete(db).await?;
    } else {
        let mut active: dependencies::ActiveModel = edge.into();
        active.dependee_id = Set(replacement_id);
        active.save(db).await?;
    }
    Ok(())
}

/// The row to point an edge at, adding it from the AUR if it is not here yet.
///
/// Added the way resolution would have added it -- as a dependency, on the
/// dependent's own platforms and build flags -- so a replacement chosen by
/// hand is indistinguishable from one resolution picked itself. That includes
/// its builds: it is queued like any added package, and the dependent's own
/// build then waits on it rather than on a package nothing will ever build.
async fn ensure_replacement_exists(
    services: &Services,
    dependent: &packages::Model,
    replacement: &str,
) -> Result<packages::Model, ApiError> {
    if let Some(package) = Packages::find()
        .filter(packages::Column::Name.eq(replacement))
        .one(&services.db)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?
    {
        return Ok(package);
    }

    aurcache_utils::package::add::add_dependency_package(
        services,
        replacement,
        &dependent.platforms,
        &dependent.build_flags,
    )
    .await
    .map_err(|e| err(Status::BadGateway, e))?;

    Packages::find()
        .filter(packages::Column::Name.eq(replacement))
        .one(&services.db)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?
        .ok_or_else(|| {
            err(
                Status::NotFound,
                format!("'{replacement}' could not be added from the AUR"),
            )
        })
}

#[cfg(test)]
mod dependency_tests {
    use super::{
        ReplacementVerdict, candidate_verdict, dependency_edge, provided_names, repoint_edge,
    };
    use aurcache_db::migration::Migrator;
    use aurcache_db::packages::SourceData;
    use aurcache_db::prelude::{Dependencies, Packages};
    use aurcache_db::{dependencies, packages};
    use aurcache_utils::package::live_check::live_check;
    use aurcache_utils::repository::Repository;
    use aurcache_utils::snapshot::SnapshotStore;
    use sea_orm::{
        ActiveModelTrait, ColumnTrait, Database, DatabaseConnection, EntityTrait, PaginatorTrait,
        QueryFilter, Set, TryIntoModel,
    };
    use sea_orm_migration::MigratorTrait;
    use serde_json::json;

    async fn memory_db() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        db
    }

    async fn package(
        db: &DatabaseConnection,
        name: &str,
        directly_requested: bool,
    ) -> packages::Model {
        packages::ActiveModel {
            name: Set(name.to_string()),
            status: Set(1),
            out_of_date: Set(0),
            upstream_version: Set(None),
            latest_build: Set(None),
            build_flags: Set(String::new()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(packages::SourceType::Aur),
            source_data: Set(SourceData::Aur { name: name.into() }),
            directly_requested: Set(directly_requested),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(db)
        .await
        .unwrap()
        .try_into_model()
        .unwrap()
    }

    async fn edge(db: &DatabaseConnection, dependent: i32, dependee: i32, constraint: &str) {
        dependencies::ActiveModel {
            dependent_id: Set(dependent),
            dependee_id: Set(dependee),
            version_constraint: Set(constraint.to_string()),
            ..Default::default()
        }
        .save(db)
        .await
        .unwrap();
    }

    /// A replacement is judged against every name the package answers to, so
    /// all three sources of them have to be here -- and a versioned `provides`
    /// contributes the name, not the whole entry.
    #[tokio::test]
    async fn provided_names_covers_the_name_the_splits_and_the_provides() {
        let db = memory_db().await;
        let mut package = package(&db, "libfoo", true).await;
        assert_eq!(provided_names(&package), vec!["libfoo"]);

        package.split_packages = Some(json!(["libfoo-docs"]).to_string());
        package.provides = Some(json!(["libfoo.so=1", "foo-compat"]).to_string());
        assert_eq!(
            provided_names(&package),
            vec!["foo-compat", "libfoo", "libfoo-docs", "libfoo.so"],
            "a versioned `provides` contributes the name, not the whole entry"
        );
    }

    /// An unbuilt candidate is `Unknown`, not `Unsatisfied`: the queue checks
    /// the constraint against each real build, so there is nothing to conclude
    /// yet and saying "no" would hide a usable option.
    #[test]
    fn a_candidate_is_judged_on_the_version_it_is_known_to_be_at() {
        assert_eq!(
            candidate_verdict(None, ""),
            ReplacementVerdict::Satisfied,
            "an unconstrained edge is met by anything"
        );
        assert_eq!(
            candidate_verdict(Some("2.0.0-1"), ">=2.0"),
            ReplacementVerdict::Satisfied
        );
        assert_eq!(
            candidate_verdict(Some("1.0.0-1"), ">=2.0"),
            ReplacementVerdict::Unsatisfied
        );
        assert_eq!(
            candidate_verdict(None, ">=2.0"),
            ReplacementVerdict::Unknown
        );
    }

    #[tokio::test]
    async fn an_edge_that_does_not_exist_is_not_found() {
        let db = memory_db().await;
        package(&db, "dependent", true).await;
        package(&db, "stranger", false).await;

        assert!(dependency_edge(&db, "dependent", "stranger").await.is_err());
        assert!(dependency_edge(&db, "dependent", "nonesuch").await.is_err());
    }

    /// Repointing the last edge onto something else leaves the old dependency
    /// holding nothing up, and the collection that follows takes it. This is
    /// what lets emptying a package's dependents remove it, with no second
    /// endpoint that knows how to remove packages.
    #[tokio::test]
    async fn repointing_the_last_edge_collects_the_old_dependency() {
        let db = memory_db().await;
        let dependent = package(&db, "dependent", true).await;
        let old = package(&db, "old-provider", false).await;
        let new = package(&db, "new-provider", false).await;
        edge(&db, dependent.id, old.id, ">=1.0").await;

        let moving = Dependencies::find().one(&db).await.unwrap().unwrap();
        repoint_edge(&db, moving, dependent.id, new.id)
            .await
            .unwrap();
        let checkouts = tempfile::tempdir().unwrap();
        let store = SnapshotStore::with_checkout_root(checkouts.path().to_path_buf());
        let repo = Repository::new(checkouts.path().join("repo"));
        live_check(&db, &store, &repo, old.id).await.unwrap();

        assert!(
            Packages::find_by_id(old.id)
                .one(&db)
                .await
                .unwrap()
                .is_none(),
            "nothing needs the old dependency any more"
        );
        let remaining = Dependencies::find().all(&db).await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].dependee_id, new.id);
        assert_eq!(
            remaining[0].version_constraint, ">=1.0",
            "the constraint travels with the edge"
        );
    }

    /// A dependent that already depends on the replacement under some other
    /// name would collide, since there is one row per pair. The edge is merged
    /// into the one already there rather than duplicated.
    #[tokio::test]
    async fn repointing_onto_an_edge_that_already_exists_merges() {
        let db = memory_db().await;
        let dependent = package(&db, "dependent", true).await;
        let old = package(&db, "old-provider", false).await;
        let new = package(&db, "new-provider", false).await;
        edge(&db, dependent.id, old.id, ">=1.0").await;
        edge(&db, dependent.id, new.id, ">=3.0").await;

        let moving = Dependencies::find()
            .filter(dependencies::Column::DependeeId.eq(old.id))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        repoint_edge(&db, moving, dependent.id, new.id)
            .await
            .unwrap();

        assert_eq!(Dependencies::find().count(&db).await.unwrap(), 1);
        let remaining = Dependencies::find().one(&db).await.unwrap().unwrap();
        assert_eq!(remaining.dependee_id, new.id);
        assert_eq!(remaining.version_constraint, ">=3.0");
    }

    /// A dependency something else still needs survives the repoint: the
    /// collection is reachability, not "did an edge just move".
    #[tokio::test]
    async fn a_dependency_another_package_still_needs_is_kept() {
        let db = memory_db().await;
        let dependent = package(&db, "dependent", true).await;
        let other = package(&db, "other", true).await;
        let old = package(&db, "old-provider", false).await;
        let new = package(&db, "new-provider", false).await;
        edge(&db, dependent.id, old.id, "").await;
        edge(&db, other.id, old.id, "").await;

        let moving = Dependencies::find()
            .filter(dependencies::Column::DependentId.eq(dependent.id))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        repoint_edge(&db, moving, dependent.id, new.id)
            .await
            .unwrap();
        let checkouts = tempfile::tempdir().unwrap();
        let store = SnapshotStore::with_checkout_root(checkouts.path().to_path_buf());
        let repo = Repository::new(checkouts.path().join("repo"));
        live_check(&db, &store, &repo, old.id).await.unwrap();

        assert!(
            Packages::find_by_id(old.id)
                .one(&db)
                .await
                .unwrap()
                .is_some()
        );
    }
}
