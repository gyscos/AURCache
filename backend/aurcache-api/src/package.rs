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
use crate::utils::error::{ApiError, err};
use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_activitylog::package_add_activity::PackageAddActivity;
use aurcache_activitylog::package_delete_activity::PackageDeleteActivity;
use aurcache_activitylog::package_update_activity::PackageUpdateActivity;
use aurcache_db::action::Action;
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
use aurcache_deps::AurClient;
use aurcache_utils::package::add::package_add;
use aurcache_utils::package::live_check::package_remove;
use aurcache_utils::package::update::{package_resync_dependencies, package_update};
use aurcache_utils::patch::SourcePatch;
use aurcache_utils::pkg::satisfies_constraint;
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
    ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect, QueryTrait, RelationTrait,
};
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::broadcast::Sender;
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
    bulk_add_progress,
    package_update_entity_endpoint,
    package_update_endpoint,
    package_del,
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
        .map(|flag| flag.trim().to_string())
        .filter(|flag| !flag.is_empty())
        .collect()
}

/// Parse the platform names a request carried, if any.
fn parse_platforms(platforms: Option<Vec<String>>) -> Result<Option<Vec<Platform>>, ApiError> {
    platforms
        .map(|v| {
            v.into_iter()
                .map(|s| Platform::from_str(&s).ok())
                .collect::<Option<Vec<Platform>>>()
                .ok_or_else(|| err(Status::BadRequest, "Invalid platform name"))
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
    db: &State<DatabaseConnection>,
    input: Json<AddPackages>,
    tx: &State<Sender<Action>>,
    store: &State<Arc<SnapshotStore>>,
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

    let job_id = operations::create(db.inner(), operations::KIND_BULK_ADD, total)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;

    // Everything the task needs is cloned in: it outlives this request by
    // design, so it cannot borrow from it.
    let db_task = db.inner().clone();
    let tx_task = tx.inner().clone();
    let store_task = Arc::clone(store.inner());
    let al_task = al.inner().clone();
    let username = a.username.clone();

    tokio::spawn(async move {
        let (progress_tx, mut progress_rx) = mpsc::unbounded_channel();
        let worker = {
            let db = db_task.clone();
            let store = Arc::clone(&store_task);
            tokio::spawn(async move {
                aurcache_utils::package::bulk_add::bulk_add(
                    &store,
                    &db,
                    &tx_task,
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
            if matches!(entry.outcome, BulkAddOutcome::Added)
                && let Err(e) = al_task
                    .add(
                        PackageAddActivity {
                            package: entry.name.clone(),
                        },
                        ActivityType::AddPackage,
                        username.clone(),
                    )
                    .await
            {
                // The package is added; only the record of who asked is
                // missing. Not worth failing the job over.
                warn!("could not log activity for {}: {e}", entry.name);
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
    db: &State<DatabaseConnection>,
    input: Json<AddPackage>,
    tx: &State<Sender<Action>>,
    store: &State<Arc<SnapshotStore>>,
    a: Authenticated,
    al: &State<ActivityLog>,
) -> Result<(), ApiError> {
    let input = input.into_inner();
    let platforms = parse_platforms(input.platforms)?;

    let new_pkg_name = package_add(
        store,
        db,
        tx,
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

    al.add(
        PackageAddActivity {
            package: new_pkg_name,
        },
        ActivityType::AddPackage,
        a.username,
    )
    .await
    .map_err(|e| err(Status::InternalServerError, e))?;
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
    db: &State<DatabaseConnection>,
    tx: &State<Sender<Action>>,
    store: &State<Arc<SnapshotStore>>,
    input: Json<PackagePatch>,
    pkgbase: &str,
    _a: Authenticated,
) -> Result<(), ApiError> {
    let db = db.inner();

    // We cannot move things out of Json<T>, but we can move it out of T.
    let input = input.into_inner();
    let patch_changed = input.patch.is_some();
    let pkg = package_by_pkgbase(db, pkgbase).await?;

    // Dependencies are read per architecture — a PKGBUILD can declare
    // `depends_aarch64` separately — and the graph is the union across the
    // platforms a package is built for. Changing that set therefore changes
    // which dependencies are required, so it needs the same resync a patch
    // gets. Compared against the stored value so a no-op write does not
    // trigger a needless source checkout.
    let platforms_changed = input
        .platforms
        .as_ref()
        .is_some_and(|requested| requested.join(";") != pkg.platforms);

    // Start building the update operation
    let update_pkg = packages::ActiveModel {
        id: Set(pkg.id),
        name: input.name.map_or(NotSet, Set),
        status: input.status.map_or(NotSet, Set),
        out_of_date: input.out_of_date.map_or(NotSet, Set),
        upstream_version: NotSet,
        // Mirrored AUR metadata is owned by the version-check scheduler; a
        // package patch must not clear it.
        source_description: NotSet,
        source_maintainer: NotSet,
        source_project_url: NotSet,
        source_licenses: NotSet,
        source_first_submitted: NotSet,
        source_last_modified: NotSet,
        aur_flagged_outdated: NotSet,
        aur_missing: NotSet,
        latest_build: input.latest_build.map_or(NotSet, Set),
        build_flags: input
            .build_flags
            .as_deref()
            .map_or(NotSet, |v| Set(normalize_build_flags(v).join(";"))),
        platforms: input.platforms.map_or(NotSet, |v| Set(v.join(";"))),
        source_type: NotSet,
        source_data: NotSet,
        directly_requested: NotSet,
        split_packages: NotSet,
        provides: NotSet,
        patch: input.patch.map_or(NotSet, Set),
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
        let client = AurClient::new();
        package_resync_dependencies(&client, store, db, tx, &updated)
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

    Ok(Json(SourceFileList { files }))
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

    let (original_content, patched_content, patch_error) = store
        .read_file_with_patch_status(&pkg.source_data, pkg.patch.as_deref(), &path)
        .await
        .map_err(|e| err(Status::NotFound, e))?;

    Ok(Json(SourceFileContent {
        path,
        original_content,
        patched_content,
        patch_error,
    }))
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
    db: &State<DatabaseConnection>,
    tx: &State<Sender<Action>>,
    store: &State<Arc<SnapshotStore>>,
    pkgbase: &str,
    input: Json<SourceFileUpdate>,
    _a: Authenticated,
) -> Result<(), ApiError> {
    let db = db.inner();
    let input = input.into_inner();

    let pkg = package_by_pkgbase(db, pkgbase).await?;

    let client = AurClient::new();

    // Diff against the pristine (unpatched) file, not the currently effective
    // one, so re-saving the same edit twice is idempotent.
    let original = store
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
    package_resync_dependencies(&client, store, db, tx, &resynced_pkg)
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

    Ok(Json(SourceFileList { files }))
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
    db: &State<DatabaseConnection>,
    pkgbase: &str,
    input: Json<UpdatePackage>,
    tx: &State<Sender<Action>>,
    store: &State<Arc<SnapshotStore>>,
    a: Authenticated,
    al: &State<ActivityLog>,
) -> Result<Json<Vec<i32>>, ApiError> {
    let db = db.inner();

    let pkg_model: packages::Model = package_by_pkgbase(db, pkgbase).await?;
    let package_name = pkg_model.name.clone();
    let forced = input.force;

    let pkg_update = package_update(store, db, pkg_model, forced, tx)
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

    al.add(
        PackageUpdateActivity {
            package: package_name,
            forced,
        },
        ActivityType::UpdatePackage,
        a.username,
    )
    .await
    .map_err(|e| err(Status::InternalServerError, e))?;
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
    db: &State<DatabaseConnection>,
    pkgbase: &str,
    a: Authenticated,
    al: &State<ActivityLog>,
    store: &State<Arc<SnapshotStore>>,
) -> Result<(), ApiError> {
    let db = db.inner();

    // query this before removing package ownership!
    let pkg = package_by_pkgbase(db, pkgbase).await?;
    let source_data = pkg.source_data.clone();

    package_remove(db, pkg.id)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;

    // The clone made for this package, now that nothing refers to it. Failing
    // to remove it is not worth failing the delete over: the package is gone,
    // and the boot-time prune sweeps whatever is left.
    if let Err(e) = store.remove_checkout(&source_data).await {
        warn!("could not remove source checkout for {pkgbase}: {e}");
    }

    al.add(
        PackageDeleteActivity { package: pkg.name },
        ActivityType::RemovePackage,
        a.username,
    )
    .await
    .map_err(|e| err(Status::InternalServerError, e))?;

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
        .offset(page.zip(limit).map(|(page, limit)| page * limit))
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

    let latest_version = latest_successful_version_any_platform(db, pkg.id)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?
        // Same rule as the list query: an enqueued build's empty version is not
        // a version.
        .filter(|v| !v.is_empty());
    let dependencies = list_package_relations(db, pkg.id, RelationDirection::Dependencies)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;
    let dependents = list_package_relations(db, pkg.id, RelationDirection::Dependents)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;

    let files = package_files(db, pkg.id)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;

    let has_patch = pkg.patch.is_some();

    let (package_source, upstream_version) = package_source_and_version(&pkg)?;

    let split_packages: Option<Vec<String>> = pkg
        .split_packages
        .clone()
        .and_then(|s| serde_json::from_str(&s).ok());

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
        selected_platforms: pkg.platforms.split(';').map(ToString::to_string).collect(),
        selected_build_flags: Some(
            pkg.build_flags
                .split(';')
                .map(ToString::to_string)
                .collect(),
        ),
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
    use sea_orm::{ActiveModelTrait, Database, DatabaseConnection, Set};
    use sea_orm_migration::MigratorTrait;

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
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

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
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

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
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

        file(&db, "hello-1.0-1-x86_64.pkg.tar.zst", 1, None).await;

        let listed = package_files(&db, 1).await.unwrap();
        assert_eq!(listed[0].size, None);
    }
}
