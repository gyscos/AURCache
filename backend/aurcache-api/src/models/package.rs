use aurcache_db::packages::{GitSourceSpec, SourceData};
use rocket::serde::{Deserialize, Serialize};
use sea_orm::FromQueryResult;
use utoipa::ToSchema;

#[derive(Deserialize, ToSchema, Clone)]
#[serde(crate = "rocket::serde")]
pub struct AddPackage {
    pub(crate) platforms: Option<Vec<String>>,
    pub(crate) build_flags: Option<Vec<String>>,
    pub(crate) source: SourceData,
    /// Optional initial patch (raw JSON [`SourcePatch`]) to apply before the
    /// source is fetched/parsed for the first time. Lets a package that
    /// fails to parse upstream (e.g. a malformed PKGBUILD) be fixed up and
    /// added in one step, instead of having to add it broken and edit it
    /// afterwards.
    #[serde(default)]
    pub(crate) patch: Option<String>,
}

#[derive(Deserialize, ToSchema)]
#[serde(crate = "rocket::serde")]
pub struct UpdatePackage {
    pub(crate) force: bool,
}

#[derive(Serialize, ToSchema)]
pub struct SourceFileList {
    pub files: Vec<String>,
}

#[derive(Serialize, ToSchema)]
pub struct SourceFileContent {
    pub path: String,
    pub content: String,
    /// Whether this file currently differs from the pristine upstream source.
    pub patched: bool,
}

#[derive(Deserialize, ToSchema)]
#[serde(crate = "rocket::serde")]
pub struct SourceFileUpdate {
    pub path: String,
    pub content: String,
}

/// Request body for the pre-add source preview endpoints: identifies a
/// not-yet-added source (and an in-progress patch, if any) so its files can
/// be listed/edited before `POST /package` is ever called.
#[derive(Deserialize, ToSchema, Clone)]
#[serde(crate = "rocket::serde")]
pub struct SourcePreviewRequest {
    pub source: SourceData,
}

/// Request body to merge an edit into an in-progress (pre-add) patch.
#[derive(Deserialize, ToSchema, Clone)]
#[serde(crate = "rocket::serde")]
pub struct SourcePreviewFileUpdate {
    pub source: SourceData,
    #[serde(default)]
    pub patch: Option<String>,
    pub path: String,
    pub content: String,
}

/// Response for [`SourcePreviewFileUpdate`]: the merged patch, plus whether
/// it now parses cleanly (dependencies/version can only be resolved once it
/// does).
#[derive(Serialize, ToSchema)]
pub struct SourcePreviewPatchResult {
    pub patch: Option<String>,
    pub parses: bool,
    pub parse_error: Option<String>,
}

#[derive(FromQueryResult, Deserialize, ToSchema, Serialize, Default)]
pub struct PackagePatchModel {
    pub name: Option<String>,
    pub status: Option<i32>,
    pub out_of_date: Option<i32>,
    pub latest_build: Option<Option<i32>>,
    pub build_flags: Option<Vec<String>>,
    pub platforms: Option<Vec<String>>,
    /// Multi-file unified diff applied on top of the fetched source.
    /// `Some(None)` clears an existing patch, `None` leaves it untouched.
    pub patch: Option<Option<String>>,
}

#[derive(FromQueryResult, Deserialize, ToSchema, Serialize)]
pub struct SimplePackageModel {
    pub id: i32,
    pub name: String,
    pub status: i32,
    pub outofdate: i32,
    pub latest_version: Option<String>,
    pub upstream_version: String,
}

#[derive(Deserialize, ToSchema, Serialize, Clone)]
pub struct ExtendedPackageModel {
    pub id: i32,
    pub name: String,
    pub directly_requested: bool,
    pub status: i32,
    pub outofdate: i32,
    pub latest_version: Option<String>,
    pub selected_platforms: Vec<String>,
    pub selected_build_flags: Option<Vec<String>>,
    // todo this should be renamed to "latest_upstream_version" or sth
    pub upstream_version: String,
    pub package_source: PackageSource,
    pub split_packages: Option<Vec<String>>,
    pub dependencies: Vec<PackageDependencyModel>,
    pub dependents: Vec<PackageDependencyModel>,
    /// Whether the package currently has a source patch applied.
    pub has_patch: bool,
}

#[derive(Deserialize, ToSchema, Serialize, Clone, sea_orm::FromQueryResult)]
pub struct PackageDependencyModel {
    pub id: i32,
    pub name: String,
    pub version_constraint: String,
}

#[derive(Deserialize, ToSchema, Serialize, Clone)]
#[serde(tag = "package_type", rename_all = "PascalCase")]
pub enum PackageSource {
    Aur(AurPackage),
    AurNotFound(AurNotFoundPackage),
    Git(GitSourceSpec),
    Upload(UploadPackage),
}

// todo upload package
#[derive(Deserialize, ToSchema, Serialize, Default, Clone)]
pub struct UploadPackage {}

#[derive(Deserialize, ToSchema, Serialize, Default, Clone)]
pub struct AurNotFoundPackage {}

#[derive(Deserialize, ToSchema, Serialize, Default, Clone)]
pub struct AurPackage {
    pub(crate) name: String,
    pub project_url: Option<String>,
    pub description: Option<String>,
    pub last_updated: u32,
    pub first_submitted: u32,
    pub licenses: Option<String>,
    pub maintainer: Option<String>,
    pub aur_flagged_outdated: bool,
    pub aur_url: String,
}
