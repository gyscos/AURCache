use aurcache_db::packages::{GitSourceSpec, SourceData};
use rocket::serde::{Deserialize, Serialize};
use sea_orm::FromQueryResult;
use std::collections::BTreeMap;
use utoipa::ToSchema;

#[derive(Deserialize, ToSchema, Clone)]
#[serde(crate = "rocket::serde")]
pub struct AddPackage {
    pub(crate) platforms: Option<Vec<String>>,
    pub(crate) build_flags: Option<Vec<String>>,
    pub(crate) source: SourceData,
    /// Optional initial patch, expressed as full file contents (path -> new
    /// content) rather than a diff - the backend diffs each entry against
    /// the source's pristine content itself. Lets a package that fails to
    /// parse upstream (e.g. a malformed PKGBUILD) be fixed up and added in
    /// one step, instead of having to add it broken and edit it afterwards.
    #[serde(default)]
    pub(crate) patched_files: Option<BTreeMap<String, String>>,
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

/// Effective content of a single source file for an already-added package.
/// The pristine content is always included so the UI can fall back to it
/// (and offer a "revert" action) even if the stored patch no longer applies
/// cleanly to the current upstream source.
#[derive(Serialize, ToSchema)]
pub struct SourceFileContent {
    pub path: String,
    pub original_content: String,
    /// Content with the stored patch applied, if this file is part of the
    /// patch and it still applies cleanly. `None` if there's no patch for
    /// this file, or if the stored diff no longer applies (see
    /// `patch_error`) - in that case the UI should fall back to
    /// `original_content`.
    pub patched_content: Option<String>,
    /// Set if this file is part of the stored patch but applying it failed
    /// (e.g. upstream changed enough that the diff's context no longer
    /// matches).
    pub patch_error: Option<String>,
}

#[derive(Deserialize, ToSchema)]
#[serde(crate = "rocket::serde")]
pub struct SourceFileUpdate {
    pub path: String,
    pub content: String,
}

/// Request body for the pre-add source preview endpoints: identifies a
/// not-yet-added source so its (pristine) files can be listed/read before
/// `POST /package` is ever called.
#[derive(Deserialize, ToSchema, Clone)]
#[serde(crate = "rocket::serde")]
pub struct SourcePreviewRequest {
    pub source: SourceData,
}

/// Request body to read a single pristine file of a not-yet-added source.
#[derive(Deserialize, ToSchema, Clone)]
#[serde(crate = "rocket::serde")]
pub struct SourcePreviewFileRequest {
    pub source: SourceData,
    pub path: String,
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
