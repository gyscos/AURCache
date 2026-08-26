use crate::models::authenticated::Authenticated;
use crate::models::package::{
    AddPackage, PackagePatch, SourceFileContent, SourceFileList, SourceFileUpdate,
    SourcePreviewFileRequest, SourcePreviewRequest, UpdatePackage,
};
use crate::models::package::{
    AurNotFoundPackage, AurPackage, ExtendedPackage, PackageDependency, PackageSource,
    SimplePackage,
};
use crate::utils::error::{ApiError, err};
use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_activitylog::package_add_activity::PackageAddActivity;
use aurcache_activitylog::package_delete_activity::PackageDeleteActivity;
use aurcache_activitylog::package_update_activity::PackageUpdateActivity;
use aurcache_db::action::Action;
use aurcache_db::activities::ActivityType;
use aurcache_db::packages::SourceData;
use aurcache_db::prelude::{Builds, Dependencies, Packages};
use aurcache_db::{builds, dependencies, packages};
use aurcache_deps::AurClient;
use aurcache_types::build_state::BuildStates;
use aurcache_utils::package::add::package_add;
use aurcache_utils::package::live_check::package_remove;
use aurcache_utils::package::update::{package_resync_dependencies, package_update};
use aurcache_utils::patch::SourcePatch;
use aurcache_utils::pkg::satisfies_constraint;
use aurcache_utils::snapshot::SnapshotStore;
use pacman_mirrors::platforms::Platform;
use rocket::http::Status;
use sea_orm::FromQueryResult;

use rocket::serde::json::Json;
use rocket::{State, delete, get, patch, post, put};
use sea_orm::ActiveValue::{NotSet, Set};
use sea_orm::prelude::Expr;
use sea_orm::{ActiveModelTrait, DatabaseConnection, JoinType, Order};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect, RelationTrait};
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::broadcast::Sender;
use utoipa::OpenApi;

/// Resolve a package by its pkgbase, the public identifier.
///
/// Row ids are deliberately absent from the public API — they are an
/// implementation detail. The two also cannot be accepted interchangeably:
/// some real pkgbases are entirely numeric (`1337` and `67` both exist in the
/// AUR), so a route taking either would resolve those names to whatever rows
/// happened to hold those ids — a wrong-package bug rather than a not-found
/// error. Hence no id fallback.
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
    let platforms = match input.platforms.clone() {
        None => None,
        Some(v) => Some(
            v.into_iter()
                .map(|s| Platform::from_str(&s).ok())
                .collect::<Option<Vec<Platform>>>()
                .ok_or_else(|| err(Status::BadRequest, "Invalid platform name"))?,
        ),
    };

    let new_pkg_name = package_add(
        store,
        db,
        tx,
        platforms,
        input.build_flags.as_deref().map(normalize_build_flags),
        input.source.clone(),
        input.patched_files.clone(),
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

    // Start building the update operation
    let update_pkg = packages::ActiveModel {
        id: Set(pkg.id),
        name: input.name.map_or(NotSet, Set),
        status: input.status.map_or(NotSet, Set),
        out_of_date: input.out_of_date.map_or(NotSet, Set),
        upstream_version: NotSet,
        // Mirrored AUR metadata is owned by the version-check scheduler; a
        // package patch must not clear it.
        aur_description: NotSet,
        aur_maintainer: NotSet,
        aur_project_url: NotSet,
        aur_licenses: NotSet,
        aur_first_submitted: NotSet,
        aur_last_modified: NotSet,
        aur_flagged_outdated: NotSet,
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
    // version, so keep the dependency graph in sync immediately.
    if patch_changed {
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
    let original_content = store
        .read_file(&input.source, None, &input.path)
        .await
        .map_err(|e| err(Status::NotFound, e))?;

    Ok(Json(SourceFileContent {
        path: input.path.clone(),
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

    let pkg_update = package_update(store, db, pkg_model.clone(), input.force, tx)
        .await
        .map(|results| {
            Json(
                results
                    .into_iter()
                    .filter(|r| r.enqueued)
                    .map(|r| r.build_id)
                    .collect::<Vec<_>>(),
            )
        })
        // Same as adding: "already up to date", an unresolvable source, or a
        // patch that no longer applies are all caller-visible conditions.
        .map_err(|e| err(Status::BadRequest, e))?;

    al.add(
        PackageUpdateActivity {
            package: pkg_model.name,
            forced: input.force,
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
) -> Result<(), ApiError> {
    let db = db.inner();

    // query this before removing package ownership!
    let pkg = package_by_pkgbase(db, pkgbase).await?;

    package_remove(db, pkg.id)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;

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
            ("page", description = "page of packages")
    )
)]
#[get("/packages/list?<limit>&<page>")]
pub async fn package_list(
    db: &State<DatabaseConnection>,
    limit: Option<u64>,
    page: Option<u64>,
    _a: Authenticated,
) -> Result<Json<Vec<SimplePackage>>, ApiError> {
    let db = db.inner();

    list_directly_requested_packages(db, limit, page)
        .await
        .map(Json)
        .map_err(|e| err(Status::InternalServerError, e))
}

async fn list_directly_requested_packages(
    db: &DatabaseConnection,
    limit: Option<u64>,
    page: Option<u64>,
) -> Result<Vec<SimplePackage>, sea_orm::DbErr> {
    // correlated subquery: picks the version from builds for the package ordered by most
    // recent timestamp (end_time preferred, fallback to start_time)
    // Successful builds only. This is the version that is *in the repository*,
    // which is what the field is read as everywhere it is shown. Taking the
    // newest attempt regardless of outcome meant a failed build of a new
    // version reported that version as built, and the version check then
    // compared upstream against it and cleared the out-of-date flag — so an
    // upstream release whose first build failed stopped being flagged at all.
    //
    // `NULLIF` because `builds.version` is NOT NULL DEFAULT '': a build that
    // has been enqueued but has not determined a version yet holds an empty
    // string, which means "not known", not "the empty version".
    let latest_version_subquery = format!(
        "(SELECT NULLIF(b.version, '') \
        FROM builds b \
        WHERE b.pkg_id = packages.id AND b.status = {successful} \
        ORDER BY COALESCE(b.end_time, b.start_time) DESC \
        LIMIT 1)",
        successful = BuildStates::SUCCESSFUL_BUILD
    );

    let all: Vec<SimplePackage> = Packages::find()
        .select_only()
        .column(packages::Column::Name)
        .column(packages::Column::Id)
        .column(packages::Column::Status)
        .column_as(packages::Column::OutOfDate, "outofdate")
        .column_as(packages::Column::UpstreamVersion, "upstream_version")
        .filter(packages::Column::DirectlyRequested.eq(true))
        // No COALESCE to an empty string: a package with no build has no
        // version, and `null` says that where `""` is indistinguishable from a
        // build that produced a blank one. The detail endpoint below already
        // reported it this way, so coercing here made one field mean two
        // different things depending on which route you asked.
        .column_as(Expr::cust(&latest_version_subquery), "latest_version")
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

    // The version in the repository for the joined package, on the same
    // "successful builds only" rule the rest of this module uses.
    let built_version_subquery = format!(
        "(SELECT NULLIF(b.version, '') \
        FROM builds b \
        WHERE b.pkg_id = packages.id AND b.status = {successful} \
        ORDER BY COALESCE(b.end_time, b.start_time) DESC \
        LIMIT 1)",
        successful = BuildStates::SUCCESSFUL_BUILD
    );

    let rows = Dependencies::find()
        .select_only()
        .column_as(packages::Column::Id, "id")
        .column_as(packages::Column::Name, "name")
        .column(dependencies::Column::VersionConstraint)
        .column_as(packages::Column::Status, "status")
        .column_as(Expr::cust(&built_version_subquery), "built_version")
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
    pkgbase: &str,
    _a: Authenticated,
) -> Result<Json<ExtendedPackage>, ApiError> {
    let db = db.inner();

    let pkg = package_by_pkgbase(db, pkgbase).await?;

    // Query the latest build.version for this package (most recent by end_time then start_time)
    let latest_version_row = Builds::find()
        .select_only()
        .column(builds::Column::Version)
        .filter(builds::Column::PkgId.eq(pkg.id))
        // Successful only; see the list query above.
        .filter(builds::Column::Status.eq(BuildStates::SUCCESSFUL_BUILD))
        .order_by(builds::Column::EndTime, Order::Desc)
        .order_by(builds::Column::StartTime, Order::Desc)
        .limit(1)
        .into_tuple::<(String,)>()
        .one(db)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;

    // Same rule as the list query: an enqueued build's empty version is not a
    // version.
    let latest_version: Option<String> = latest_version_row.map(|(v,)| v).filter(|v| !v.is_empty());
    let dependencies = list_package_relations(db, pkg.id, RelationDirection::Dependencies)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;
    let dependents = list_package_relations(db, pkg.id, RelationDirection::Dependents)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;

    let has_patch = pkg.patch.is_some();
    let source_data = pkg.source_data.clone();

    let (package_source, version) = match source_data {
        SourceData::Aur { .. } => {
            // Read straight from the row. The version-check scheduler mirrors
            // this metadata, and `package::add` fills it in immediately for a
            // new package, so there is no live AUR lookup on this path at all —
            // it used to be ~128ms of a ~130ms response, and one of the AUR's
            // 4000 daily calls per page view.
            //
            // A package with nothing mirrored is one the AUR did not return:
            // reported as not found, which is what a live lookup concluded too.
            match cached_aur_package(&pkg) {
                Some(cached) => (PackageSource::Aur(cached), pkg.upstream_version.clone()),
                None => (
                    PackageSource::AurNotFound(AurNotFoundPackage {}),
                    pkg.upstream_version.clone(),
                ),
            }
        }
        SourceData::Git { spec } => (
            PackageSource::Git(spec),
            // How current this version is depends on the version-check
            // interval; `None` means no check has run yet.
            pkg.upstream_version,
        ),
        SourceData::Upload { .. } => {
            return Err(err(
                Status::NotImplemented,
                "Upload sources are not yet supported",
            ));
        }
    };

    let ext_pkg = ExtendedPackage {
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
        upstream_version: version,
        split_packages: pkg
            .split_packages
            .and_then(|s| serde_json::from_str(&s).ok()),
        dependencies,
        dependents,
        has_patch,
    };

    Ok(Json(ext_pkg))
}

/// The AUR page for a pkgbase. Derived, never fetched.
fn aur_pkgbase_url(pkgbase: &str) -> String {
    format!("https://aur.archlinux.org/pkgbase/{pkgbase}")
}

/// The AUR metadata mirrored onto the package row, if it has been checked.
///
/// `aur_last_modified` is the sentinel: the version-check scheduler always
/// writes it alongside the rest, while any individual field may legitimately be
/// null because the AUR reports no value for it.
///
/// Known limitation: a package later removed from the AUR keeps its last-known
/// metadata here, where a live lookup would report it as gone. The version
/// checker logs that case, and the alternative — re-querying the AUR on every
/// page view to detect a rare event — is the cost this exists to avoid.
fn cached_aur_package(pkg: &packages::Model) -> Option<AurPackage> {
    let last_modified = pkg.aur_last_modified?;

    Some(AurPackage {
        name: pkg.name.clone(),
        project_url: pkg.aur_project_url.clone(),
        description: pkg.aur_description.clone(),
        last_updated: u32::try_from(last_modified).unwrap_or(0),
        first_submitted: pkg
            .aur_first_submitted
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(0),
        licenses: pkg.aur_licenses.clone(),
        maintainer: pkg.aur_maintainer.clone(),
        aur_flagged_outdated: pkg.aur_flagged_outdated.unwrap_or(false),
        aur_url: aur_pkgbase_url(&pkg.name),
    })
}

#[cfg(test)]
mod tests {
    use super::{RelationDirection, list_directly_requested_packages, list_package_relations};
    use aurcache_db::migration::Migrator;
    use aurcache_db::packages::SourceData;
    use aurcache_db::{dependencies, packages};
    use sea_orm::{ActiveModelTrait, Database, Set, TryIntoModel};
    use sea_orm_migration::MigratorTrait;

    #[tokio::test]
    async fn package_list_only_returns_directly_requested_packages() {
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

        let packages = list_directly_requested_packages(&db, None, None)
            .await
            .unwrap();

        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "visible-package");
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
