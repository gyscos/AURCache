use crate::source::{GitSourceSpec, SourceData};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use utoipa::ToSchema;

#[derive(Deserialize, Serialize, ToSchema, Clone)]
pub struct AddPackage {
    pub platforms: Option<Vec<String>>,
    pub build_flags: Option<Vec<String>>,
    pub source: SourceData,
    /// Optional initial patch, expressed as full file contents (path -> new
    /// content) rather than a diff - the backend diffs each entry against
    /// the source's pristine content itself. Lets a package that fails to
    /// parse upstream (e.g. a malformed PKGBUILD) be fixed up and added in
    /// one step, instead of having to add it broken and edit it afterwards.
    #[serde(default)]
    pub patched_files: Option<BTreeMap<String, String>>,
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct UpdatePackage {
    pub force: bool,
}

#[derive(Serialize, Deserialize, ToSchema)]
pub struct SourceFileList {
    pub files: Vec<String>,
}

/// Effective content of a single source file for an already-added package.
/// The pristine content is always included so the UI can fall back to it
/// (and offer a "revert" action) even if the stored patch no longer applies
/// cleanly to the current upstream source.
#[derive(Serialize, Deserialize, ToSchema)]
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

#[derive(Deserialize, Serialize, ToSchema)]
pub struct SourceFileUpdate {
    pub path: String,
    pub content: String,
}

/// Request body for the pre-add source preview endpoints: identifies a
/// not-yet-added source so its (pristine) files can be listed/read before
/// `POST /package` is ever called.
#[derive(Deserialize, Serialize, ToSchema, Clone)]
pub struct SourcePreviewRequest {
    pub source: SourceData,
}

/// Request body to read a single pristine file of a not-yet-added source.
#[derive(Deserialize, Serialize, ToSchema, Clone)]
pub struct SourcePreviewFileRequest {
    pub source: SourceData,
    pub path: String,
}

#[derive(Deserialize, ToSchema, Serialize, Default)]
#[cfg_attr(feature = "db", derive(sea_orm::FromQueryResult))]
pub struct PackagePatch {
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

#[derive(Deserialize, ToSchema, Serialize)]
#[cfg_attr(feature = "db", derive(sea_orm::FromQueryResult))]
pub struct SimplePackage {
    pub id: i32,
    pub name: String,
    pub status: i32,
    pub outofdate: i32,
    pub latest_version: Option<String>,
    /// `None` until a version check has determined it. The column is nullable
    /// and rows land in this list before their first check — the dependency
    /// migration inserts them without one, and adding such a package
    /// explicitly only flips `directly_requested`. Typed as a plain `String`
    /// this failed to decode, which took down the whole route, not one row.
    pub upstream_version: Option<String>,
}

#[derive(Deserialize, ToSchema, Serialize, Clone, Debug, PartialEq)]
pub struct ExtendedPackage {
    pub id: i32,
    pub name: String,
    pub directly_requested: bool,
    pub status: i32,
    pub outofdate: i32,
    pub latest_version: Option<String>,
    pub selected_platforms: Vec<String>,
    pub selected_build_flags: Option<Vec<String>>,
    // todo this should be renamed to "latest_upstream_version" or sth
    /// `None` while it is still unknown, matching [`SimplePackage`]. Coercing
    /// it to `""` here would make one field mean two things depending on the
    /// route, which is what it used to do.
    pub upstream_version: Option<String>,
    pub package_source: PackageSource,
    pub split_packages: Option<Vec<String>>,
    pub dependencies: Vec<PackageDependency>,
    pub dependents: Vec<PackageDependency>,
    /// Whether the package currently has a source patch applied.
    pub has_patch: bool,
}

#[derive(Deserialize, ToSchema, Serialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "db", derive(sea_orm::FromQueryResult))]
pub struct PackageDependency {
    pub id: i32,
    pub name: String,
    pub version_constraint: String,
}

#[derive(Deserialize, ToSchema, Serialize, Clone, Debug, PartialEq)]
#[serde(tag = "package_type", rename_all = "PascalCase")]
pub enum PackageSource {
    Aur(AurPackage),
    AurNotFound(AurNotFoundPackage),
    Git(GitSourceSpec),
    Upload(UploadPackage),
}

// todo upload package
#[derive(Deserialize, ToSchema, Serialize, Default, Clone, Debug, PartialEq)]
pub struct UploadPackage {}

#[derive(Deserialize, ToSchema, Serialize, Default, Clone, Debug, PartialEq)]
pub struct AurNotFoundPackage {}

#[derive(Deserialize, ToSchema, Serialize, Default, Clone, Debug, PartialEq)]
pub struct AurPackage {
    pub name: String,
    pub project_url: Option<String>,
    pub description: Option<String>,
    pub last_updated: u32,
    pub first_submitted: u32,
    pub licenses: Option<String>,
    pub maintainer: Option<String>,
    pub aur_flagged_outdated: bool,
    pub aur_url: String,
}
