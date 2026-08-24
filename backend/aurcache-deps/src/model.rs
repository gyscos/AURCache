use serde::Deserialize;
use thiserror::Error;

/// Errors that can occur when querying the AUR or resolving dependencies.
#[derive(Debug, Error)]
pub enum Error {
    /// The AUR RPC returned an error or an unexpected response.
    #[error("AUR RPC error: {0}")]
    Rpc(String),
    /// An underlying HTTP error from `reqwest`.
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    /// Reading or writing the local repo / official-repo DB cache failed.
    #[error("repository cache I/O failed")]
    Io(#[from] std::io::Error),
    /// A repository or RPC URL could not be constructed.
    #[error("invalid URL")]
    Url(#[from] url::ParseError),
    /// The AUR RPC returned a body that is not the JSON we expect.
    #[error("malformed AUR RPC response")]
    Json(#[from] serde_json::Error),
    /// A cached repository database could not be read.
    #[error("malformed repository database")]
    RepoDb(#[source] Box<dyn std::error::Error + Send + Sync>),
}

/// Dependency lists extracted from a package's metadata.
#[derive(Debug, Clone)]
pub struct PkgDeps {
    /// Runtime dependencies.
    pub depends: Vec<String>,
    /// Build-time dependencies.
    pub make_depends: Vec<String>,
    /// All sub-package names provided by the pkgbase.
    pub pkgnames: Vec<String>,
    /// Virtual packages provided by this pkgbase.
    pub provides: Vec<String>,
}

/// Where a resolved dependency was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DependencyResolution {
    /// Satisfied by an official Arch Linux repository or the local repo.
    Official,
    /// Satisfied by a package already present in the local AURCache repo.
    Local { pkgbase: String },
    /// Must be built from the AUR.
    Aur { pkgbase: String },
}

/// Package metadata returned by the AUR RPC v5 `/info` or `/search` endpoints.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
#[serde(default)]
pub struct Package {
    pub name: String,
    pub version: String,
    pub description: Option<String>,
    pub maintainer: Option<String>,
    #[serde(rename = "URL")]
    pub url: Option<String>,
    pub num_votes: u32,
    pub popularity: f32,
    pub out_of_date: Option<u32>,
    pub package_base: String,
    #[serde(rename = "PackageBaseID")]
    pub package_base_id: u32,
    pub first_submitted: u32,
    pub last_modified: u32,
    #[serde(rename = "URLPath")]
    pub url_path: Option<String>,
    #[serde(rename = "ID")]
    pub id: u32,
    pub depends: Option<Vec<String>>,
    pub make_depends: Option<Vec<String>>,
    pub opt_depends: Option<Vec<String>>,
    pub check_depends: Option<Vec<String>>,
    pub conflicts: Option<Vec<String>>,
    pub provides: Option<Vec<String>>,
    pub replaces: Option<Vec<String>>,
    pub groups: Option<Vec<String>>,
    pub license: Option<Vec<String>>,
    pub keywords: Option<Vec<String>>,
}

#[derive(Deserialize)]
pub(crate) struct PackageResponse {
    #[serde(rename = "type")]
    pub(crate) response_type: String,
    #[serde(default)]
    pub(crate) error: Option<String>,
    pub(crate) results: Vec<Package>,
}
