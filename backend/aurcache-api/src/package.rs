use crate::models::authenticated::Authenticated;
use crate::models::package::{
    AddPackage, PackagePatchModel, SourceFileContent, SourceFileList, SourceFileUpdate,
    SourcePreviewFileRequest, SourcePreviewRequest, UpdatePackage,
};
use crate::models::package::{
    AurNotFoundPackage, AurPackage, ExtendedPackageModel, PackageDependencyModel, PackageSource,
    SimplePackageModel,
};
use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_activitylog::package_add_activity::PackageAddActivity;
use aurcache_activitylog::package_delete_activity::PackageDeleteActivity;
use aurcache_activitylog::package_update_activity::PackageUpdateActivity;
use aurcache_db::activities::ActivityType;
use aurcache_db::packages::SourceData;
use aurcache_db::prelude::{Builds, Dependencies, Packages};
use aurcache_db::{builds, dependencies, packages};
use aurcache_deps::AurClient;
use aurcache_types::builder::Action;
use aurcache_utils::aur::api::get_package_info;
use aurcache_utils::package::add::package_add;
use aurcache_utils::package::live_check::package_remove;
use aurcache_utils::package::update::{package_resync_dependencies, package_update};
use aurcache_utils::patch::SourcePatch;
use aurcache_utils::snapshot::SnapshotStore;
use pacman_mirrors::platforms::Platform;
use rocket::http::Status;
use rocket::response::status::{BadRequest, Custom, NotFound};
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
) -> Result<(), BadRequest<String>> {
    let platforms = match input.platforms.clone() {
        None => None,
        Some(v) => Some(
            v.into_iter()
                .map(|s| Platform::from_str(&s).ok())
                .collect::<Option<Vec<Platform>>>()
                .ok_or_else(|| BadRequest("Invalid Platform name".to_string()))?,
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
    .map_err(|e| BadRequest(e.to_string()))?;

    al.add(
        PackageAddActivity {
            package: new_pkg_name,
        },
        ActivityType::AddPackage,
        a.username,
    )
    .await
    .map_err(|e| BadRequest(e.to_string()))?;
    Ok(())
}

#[utoipa::path(
    responses(
            (status = 200, description = "Update parts of package"),
    ),
    params(
            ("id", description = "Id of package")
    )
)]
#[patch("/package/<id>", data = "<input>")]
pub async fn package_update_entity_endpoint(
    db: &State<DatabaseConnection>,
    tx: &State<Sender<Action>>,
    store: &State<Arc<SnapshotStore>>,
    input: Json<PackagePatchModel>,
    id: i32,
    _a: Authenticated,
) -> Result<(), BadRequest<String>> {
    let db = db.inner();

    // We cannot move things out of Json<T>, but we can move it out of T.
    let input = input.into_inner();
    let patch_changed = input.patch.is_some();

    // Start building the update operation
    let update_pkg = packages::ActiveModel {
        id: Set(id),
        name: input.name.map_or(NotSet, Set),
        status: input.status.map_or(NotSet, Set),
        out_of_date: input.out_of_date.map_or(NotSet, Set),
        upstream_version: NotSet,
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
        .map_err(|e| BadRequest(e.to_string()))?;

    // A patch being set or cleared here (e.g. via the "reset patch" action)
    // can change `depends`/`makedepends` without bumping the package's
    // version, so keep the dependency graph in sync immediately.
    if patch_changed {
        let client = AurClient::new();
        package_resync_dependencies(&client, store, db, tx, &updated)
            .await
            .map_err(|e| BadRequest(e.to_string()))?;
    }

    Ok(())
}

#[utoipa::path(
    responses(
            (status = 200, description = "List the files in a package's source, available for viewing/editing", body = SourceFileList),
    ),
    params(
            ("id", description = "Id of package")
    )
)]
#[get("/package/<id>/source/files")]
pub async fn package_source_files(
    db: &State<DatabaseConnection>,
    store: &State<Arc<SnapshotStore>>,
    id: i32,
    _a: Authenticated,
) -> Result<Json<SourceFileList>, Custom<String>> {
    let db = db.inner();

    let pkg = Packages::find_by_id(id)
        .one(db)
        .await
        .map_err(|e| Custom(Status::InternalServerError, e.to_string()))?
        .ok_or_else(|| Custom(Status::NotFound, "id not found".to_string()))?;

    let client = AurClient::new();
    let files = store
        .list_files(&client, &pkg.source_data)
        .await
        .map_err(|e| Custom(Status::InternalServerError, e.to_string()))?;

    Ok(Json(SourceFileList { files }))
}

#[utoipa::path(
    responses(
            (status = 200, description = "Get the pristine content, and (if applicable) patched content, of a source file", body = SourceFileContent),
    ),
    params(
            ("id", description = "Id of package"),
            ("path", description = "File path relative to the source root, e.g. 'PKGBUILD'")
    )
)]
#[get("/package/<id>/source/file?<path>")]
pub async fn package_source_file(
    db: &State<DatabaseConnection>,
    store: &State<Arc<SnapshotStore>>,
    id: i32,
    path: String,
    _a: Authenticated,
) -> Result<Json<SourceFileContent>, Custom<String>> {
    let db = db.inner();

    let pkg = Packages::find_by_id(id)
        .one(db)
        .await
        .map_err(|e| Custom(Status::InternalServerError, e.to_string()))?
        .ok_or_else(|| Custom(Status::NotFound, "id not found".to_string()))?;

    let client = AurClient::new();

    let (original_content, patched_content, patch_error) = store
        .read_file_with_patch_status(&client, &pkg.source_data, pkg.patch.as_deref(), &path)
        .await
        .map_err(|e| Custom(Status::NotFound, e.to_string()))?;

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
            ("id", description = "Id of package")
    )
)]
#[put("/package/<id>/source/file", data = "<input>")]
pub async fn package_source_file_update(
    db: &State<DatabaseConnection>,
    tx: &State<Sender<Action>>,
    store: &State<Arc<SnapshotStore>>,
    id: i32,
    input: Json<SourceFileUpdate>,
    _a: Authenticated,
) -> Result<(), Custom<String>> {
    let db = db.inner();
    let input = input.into_inner();

    let pkg = Packages::find_by_id(id)
        .one(db)
        .await
        .map_err(|e| Custom(Status::InternalServerError, e.to_string()))?
        .ok_or_else(|| Custom(Status::NotFound, "id not found".to_string()))?;

    let client = AurClient::new();

    // Diff against the pristine (unpatched) file, not the currently effective
    // one, so re-saving the same edit twice is idempotent.
    let original = store
        .read_file(&client, &pkg.source_data, None, &input.path)
        .await
        .map_err(|e| Custom(Status::NotFound, e.to_string()))?;

    let mut patch = pkg
        .patch
        .as_deref()
        .map(SourcePatch::parse)
        .transpose()
        .map_err(|e| Custom(Status::InternalServerError, e.to_string()))?
        .unwrap_or_default();
    patch.merge_file(&input.path, &original, &input.content);

    let new_patch = if patch.is_empty() {
        None
    } else {
        Some(
            patch
                .to_json()
                .map_err(|e| Custom(Status::InternalServerError, e.to_string()))?,
        )
    };

    // No need to validate the patch applies cleanly here: it was just
    // diffed fresh against the current pristine content above, so applying
    // it back is guaranteed to succeed.
    let update_pkg = packages::ActiveModel {
        id: Set(id),
        patch: Set(new_patch.clone()),
        ..Default::default()
    };
    update_pkg
        .update(db)
        .await
        .map_err(|e| Custom(Status::InternalServerError, e.to_string()))?;

    // A patch can change `depends`/`makedepends` without bumping the
    // package's version, so resync the dependency graph immediately rather
    // than waiting for the next explicit "update" trigger.
    let mut resynced_pkg = pkg;
    resynced_pkg.patch = new_patch;
    package_resync_dependencies(&client, store, db, tx, &resynced_pkg)
        .await
        .map_err(|e| {
            Custom(
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
) -> Result<Json<SourceFileList>, Custom<String>> {
    let client = AurClient::new();
    let files = store
        .list_files(&client, &input.source)
        .await
        .map_err(|e| Custom(Status::InternalServerError, e.to_string()))?;

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
) -> Result<Json<SourceFileContent>, Custom<String>> {
    let client = AurClient::new();

    let original_content = store
        .read_file(&client, &input.source, None, &input.path)
        .await
        .map_err(|e| Custom(Status::NotFound, e.to_string()))?;

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
            ("id", description = "Id of package")
    )
)]
#[post("/package/<id>/update", data = "<input>")]
pub async fn package_update_endpoint(
    db: &State<DatabaseConnection>,
    id: i32,
    input: Json<UpdatePackage>,
    tx: &State<Sender<Action>>,
    store: &State<Arc<SnapshotStore>>,
    a: Authenticated,
    al: &State<ActivityLog>,
) -> Result<Json<Vec<i32>>, BadRequest<String>> {
    let db = db.inner();

    let pkg_model: packages::Model = Packages::find_by_id(id)
        .one(db)
        .await
        .map_err(|e| BadRequest(e.to_string()))?
        .ok_or_else(|| BadRequest("id not found".to_string()))?;

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
        .map_err(|e| BadRequest(e.to_string()))?;

    al.add(
        PackageUpdateActivity {
            package: pkg_model.name,
            forced: input.force,
        },
        ActivityType::UpdatePackage,
        a.username,
    )
    .await
    .map_err(|e| BadRequest(e.to_string()))?;
    Ok(pkg_update)
}

#[utoipa::path(
    responses(
            (status = 200, description = "Remove direct request flag from package and live-check it"),
    ),
    params(
            ("id", description = "Id of package")
    )
)]
#[delete("/package/<id>")]
pub async fn package_del(
    db: &State<DatabaseConnection>,
    id: i32,
    a: Authenticated,
    al: &State<ActivityLog>,
) -> Result<(), BadRequest<String>> {
    let db = db.inner();

    // query this before removing package ownership!
    let pkg = Packages::find_by_id(id)
        .one(db)
        .await
        .map_err(|e| BadRequest(e.to_string()))?
        .ok_or_else(|| BadRequest("id not found".to_string()))?;

    package_remove(db, id)
        .await
        .map_err(|e| BadRequest(e.to_string()))?;

    al.add(
        PackageDeleteActivity { package: pkg.name },
        ActivityType::RemovePackage,
        a.username,
    )
    .await
    .map_err(|e| BadRequest(e.to_string()))?;

    Ok(())
}
#[utoipa::path(
    responses(
            (status = 200, description = "List of all packages", body = [SimplePackageModel]),
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
) -> Result<Json<Vec<SimplePackageModel>>, NotFound<String>> {
    let db = db.inner();

    list_directly_requested_packages(db, limit, page)
        .await
        .map(Json)
        .map_err(|e| NotFound(e.to_string()))
}

async fn list_directly_requested_packages(
    db: &DatabaseConnection,
    limit: Option<u64>,
    page: Option<u64>,
) -> Result<Vec<SimplePackageModel>, sea_orm::DbErr> {
    // correlated subquery: picks the version from builds for the package ordered by most
    // recent timestamp (end_time preferred, fallback to start_time)
    let latest_version_subquery = "(SELECT version \
        FROM builds b \
        WHERE b.pkg_id = packages.id \
        ORDER BY COALESCE(b.end_time, b.start_time) DESC \
        LIMIT 1)";

    let all: Vec<SimplePackageModel> = Packages::find()
        .select_only()
        .column(packages::Column::Name)
        .column(packages::Column::Id)
        .column(packages::Column::Status)
        .column_as(packages::Column::OutOfDate, "outofdate")
        .column_as(packages::Column::UpstreamVersion, "upstream_version")
        .filter(packages::Column::DirectlyRequested.eq(true))
        // wrap the correlated subquery in COALESCE -> fallback to empty string
        .column_as(
            Expr::cust(format!("COALESCE({latest_version_subquery}, '')")),
            "latest_version",
        )
        .order_by(packages::Column::OutOfDate, Order::Desc)
        .order_by(packages::Column::Id, Order::Desc)
        .limit(limit)
        .offset(page.zip(limit).map(|(page, limit)| page * limit))
        .into_model::<SimplePackageModel>()
        .all(db)
        .await?;

    Ok(all)
}

async fn list_package_relations(
    db: &DatabaseConnection,
    pkg_id: i32,
    direction: RelationDirection,
) -> Result<Vec<PackageDependencyModel>, sea_orm::DbErr> {
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

    Dependencies::find()
        .select_only()
        .column_as(packages::Column::Id, "id")
        .column_as(packages::Column::Name, "name")
        .column(dependencies::Column::VersionConstraint)
        .join(JoinType::InnerJoin, relation)
        .filter(filter_col.eq(pkg_id))
        .order_by_asc(dependencies::Column::Id)
        .into_model::<PackageDependencyModel>()
        .all(db)
        .await
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
https://wiki.archlinux.org/title/Aurweb_RPC_interface", body = ExtendedPackageModel),
    ),
    params(
            ("id", description = "Id of package")
    )
)]
#[get("/package/<id>")]
pub async fn get_package(
    db: &State<DatabaseConnection>,
    id: i32,
    _a: Authenticated,
) -> Result<Json<ExtendedPackageModel>, Custom<String>> {
    let db = db.inner();

    let pkg = Packages::find()
        .filter(packages::Column::Id.eq(id))
        .one(db)
        .await
        .map_err(|e| Custom(Status::InternalServerError, e.to_string()))?
        .ok_or_else(|| Custom(Status::NotFound, "ID not found".to_string()))?;

    // Query the latest build.version for this package (most recent by end_time then start_time)
    let latest_version_row = Builds::find()
        .select_only()
        .column(builds::Column::Version)
        .filter(builds::Column::PkgId.eq(pkg.id))
        .order_by(builds::Column::EndTime, Order::Desc)
        .order_by(builds::Column::StartTime, Order::Desc)
        .limit(1)
        .into_tuple::<(String,)>()
        .one(db)
        .await
        .map_err(|e| Custom(Status::InternalServerError, e.to_string()))?;

    let latest_version: Option<String> = latest_version_row.map(|(v,)| v);
    let dependencies = list_package_relations(db, pkg.id, RelationDirection::Dependencies)
        .await
        .map_err(|e| Custom(Status::InternalServerError, e.to_string()))?;
    let dependents = list_package_relations(db, pkg.id, RelationDirection::Dependents)
        .await
        .map_err(|e| Custom(Status::InternalServerError, e.to_string()))?;

    let has_patch = pkg.patch.is_some();
    let source_data = pkg.source_data;

    let (package_source, version) = match source_data {
        SourceData::Aur { .. } => {
            let query_name = pkg
                .split_packages
                .as_deref()
                .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
                .and_then(|names| {
                    let first = names.first()?;
                    (names.len() > 1 || first != &pkg.name).then(|| first.clone())
                })
                .unwrap_or_else(|| pkg.name.clone());

            let aur_info = get_package_info(&query_name)
                .await
                .map_err(|e| Custom(Status::InternalServerError, e.to_string()))?;

            match aur_info {
                None => (
                    PackageSource::AurNotFound(AurNotFoundPackage {}),
                    pkg.upstream_version.unwrap_or_default(),
                ),
                Some(aur_info) => {
                    let aur_url = format!("https://aur.archlinux.org/pkgbase/{}", pkg.name);

                    (
                        PackageSource::Aur(AurPackage {
                            name: pkg.name.clone(),
                            project_url: aur_info.url,
                            description: aur_info.description,
                            last_updated: aur_info.last_modified,
                            first_submitted: aur_info.first_submitted,
                            licenses: aur_info.license.map(|l| l.join(", ")),
                            maintainer: aur_info.maintainer,
                            aur_flagged_outdated: aur_info.out_of_date.unwrap_or(0) != 0,
                            aur_url,
                        }),
                        aur_info.version,
                    )
                }
            }
        }
        SourceData::Git { spec } => (
            PackageSource::Git(spec),
            // How current this version is depends on the version-check interval.
            pkg.upstream_version.unwrap_or_default(),
        ),
        SourceData::Upload { .. } => {
            return Err(Custom(
                Status::NotImplemented,
                "Upload sources are not yet supported".to_string(),
            ));
        }
    };

    let ext_pkg = ExtendedPackageModel {
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
