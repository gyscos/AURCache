//! Typed Rust client for the AURCache HTTP API.
//!
//! This crate exposes request/response models for the API together with
//! [`AurCacheClient`], a small async wrapper around the most common endpoints.

use anyhow::{Context, Result, bail};
use reqwest::Response;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Re-export of [`reqwest::Method`] for generic request helpers.
pub use reqwest::Method;

/// Information about the currently authenticated user.
#[derive(Debug, Serialize, Deserialize)]
pub struct UserInfo {
    /// Username resolved from the configured authentication backend.
    pub username: Option<String>,
    /// Whether this user currently has an API token stored server-side.
    pub has_api_token: bool,
}

/// Response returned when a personal API token is regenerated.
#[derive(Debug, Serialize, Deserialize)]
pub struct ApiTokenResponse {
    /// Newly generated plaintext token.
    ///
    /// The server only returns this value at generation time.
    pub token: String,
}

/// Aggregate dashboard statistics for the AURCache instance.
#[derive(Debug, Serialize, Deserialize)]
pub struct ListStats {
    /// Total number of builds recorded by the server.
    pub total_builds: u32,
    /// Number of successful builds.
    pub successful_builds: u32,
    /// Number of failed builds.
    pub failed_builds: u32,
    /// Average build duration in seconds.
    pub avg_build_time: u32,
    /// Repository size on disk in bytes.
    pub repo_size: u64,
    /// Number of directly requested packages.
    pub total_packages: u32,
    /// Relative trend for build count over recent periods.
    pub total_build_trend: f32,
    /// Relative trend for average build time over recent periods.
    pub avg_build_time_trend: f32,
}

/// A single graph datapoint for monthly build activity.
#[derive(Debug, Serialize, Deserialize)]
pub struct GraphDataPoint {
    /// Calendar month, in the range `1..=12`.
    pub month: i32,
    /// Calendar year.
    pub year: i32,
    /// Number of builds for the month.
    pub count: i32,
}

/// Search result returned from the AUR search proxy endpoint.
#[derive(Debug, Serialize, Deserialize)]
pub struct SearchResult {
    /// Package name.
    pub name: String,
    /// Upstream version string reported by the search backend.
    pub version: String,
}

/// Lightweight package listing entry.
#[derive(Debug, Serialize, Deserialize)]
pub struct SimplePackage {
    /// Internal package id.
    pub id: i32,
    /// Package name.
    pub name: String,
    /// Package/build status code.
    pub status: i32,
    /// Out-of-date flag encoded as an integer.
    pub outofdate: i32,
    /// Most recent built version, if any.
    pub latest_version: Option<String>,
    /// Latest upstream version known to AURCache.
    pub upstream_version: String,
}

/// Dependency edge between two tracked packages.
#[derive(Debug, Serialize, Deserialize)]
pub struct PackageDependency {
    /// Internal id of the related package.
    pub id: i32,
    /// Package name.
    pub name: String,
    /// Recorded version constraint for this relation.
    pub version_constraint: String,
}

/// Detailed package-source metadata.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "package_type", rename_all = "PascalCase")]
pub enum PackageSource {
    /// Source metadata for an AUR-backed package.
    Aur(AurPackage),
    /// Source metadata for a package that is expected in AUR but currently missing.
    AurNotFound(EmptyPackageSource),
    /// Source metadata for a git-backed package.
    Git(GitPackage),
    /// Source metadata for an uploaded archive package.
    Upload(EmptyPackageSource),
}

/// Metadata for a package fetched from the AUR.
#[derive(Debug, Serialize, Deserialize)]
pub struct AurPackage {
    /// Package name.
    pub name: String,
    /// Upstream project URL, when available.
    pub project_url: Option<String>,
    /// Human-readable package description.
    pub description: Option<String>,
    /// First-seen/update timestamp for the package in AUR.
    pub last_updated: u32,
    /// Initial submission timestamp.
    pub first_submitted: u32,
    /// License list flattened into a single string.
    pub licenses: Option<String>,
    /// AUR maintainer, if present.
    pub maintainer: Option<String>,
    /// Whether AUR reports the package as flagged out of date.
    pub aur_flagged_outdated: bool,
    /// Canonical AUR package page URL.
    pub aur_url: String,
}

/// Metadata for a git-backed package source.
#[derive(Debug, Serialize, Deserialize)]
pub struct GitPackage {
    /// Remote git repository URL.
    pub git_url: String,
    /// Git reference used by AURCache.
    pub git_ref: String,
    /// Subdirectory containing the build files.
    pub subfolder: String,
}

/// Empty marker payload used for source variants without extra fields.
#[derive(Debug, Serialize, Deserialize, Default)]
pub struct EmptyPackageSource {}

/// Full package details returned by the package detail endpoint.
#[derive(Debug, Serialize, Deserialize)]
pub struct ExtendedPackage {
    /// Internal package id.
    pub id: i32,
    /// Package name.
    pub name: String,
    /// Whether the package was directly requested by a user.
    pub directly_requested: bool,
    /// Package/build status code.
    pub status: i32,
    /// Out-of-date flag encoded as an integer.
    pub outofdate: i32,
    /// Most recent built version, if any.
    pub latest_version: Option<String>,
    /// Platforms selected for this package.
    pub selected_platforms: Vec<String>,
    /// Build flags selected for this package.
    pub selected_build_flags: Option<Vec<String>>,
    /// Latest upstream version known to AURCache.
    pub upstream_version: String,
    /// Detailed source information.
    pub package_source: PackageSource,
    /// Split-package names generated from this package base.
    pub split_packages: Option<Vec<String>>,
    /// Packages this package depends on.
    pub dependencies: Vec<PackageDependency>,
    /// Packages depending on this package.
    pub dependents: Vec<PackageDependency>,
}

/// Build record returned by build-related endpoints.
#[derive(Debug, Serialize, Deserialize)]
pub struct Build {
    /// Internal build id.
    pub id: i32,
    /// Internal package id.
    pub pkg_id: i32,
    /// Package name.
    pub pkg_name: String,
    /// Build version string.
    pub version: String,
    /// Build status code.
    pub status: i32,
    /// Start timestamp as Unix seconds.
    pub start_time: Option<i64>,
    /// End timestamp as Unix seconds.
    pub end_time: Option<i64>,
    /// Target platform for the build.
    pub platform: String,
}

/// Request payload for adding a package to AURCache.
#[derive(Debug, Serialize)]
pub struct AddPackageRequest {
    /// Optional platform selection for the package.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platforms: Option<Vec<String>>,
    /// Optional build flags to apply.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_flags: Option<Vec<String>>,
    /// Package source to add.
    pub source: AddPackageSource,
}

/// Source payload used when creating a package.
#[derive(Debug, Serialize)]
#[serde(tag = "type")]
pub enum AddPackageSource {
    /// Add a package by AUR name.
    #[serde(rename = "aur")]
    Aur { name: String },
    /// Add a package from a git repository.
    #[serde(rename = "git")]
    Git {
        /// Remote git repository URL.
        url: String,
        /// Git reference to build from.
        #[serde(rename = "ref")]
        git_ref: String,
        /// Subdirectory containing build files.
        subfolder: String,
    },
}

/// Request payload for triggering a package update check.
#[derive(Debug, Serialize)]
pub struct UpdatePackageRequest {
    /// Whether to force the update even when the version did not change.
    pub force: bool,
}

/// Request payload for partially updating package metadata.
#[derive(Debug, Serialize)]
pub struct PatchPackageRequest {
    /// Replacement package name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Replacement status code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<i32>,
    /// Replacement out-of-date flag.
    #[serde(rename = "out_of_date", skip_serializing_if = "Option::is_none")]
    pub out_of_date: Option<i32>,
    /// Replacement latest-build reference.
    ///
    /// `Some(None)` explicitly clears the latest-build value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_build: Option<Option<i32>>,
    /// Replacement build flag selection.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_flags: Option<Vec<String>>,
    /// Replacement platform selection.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platforms: Option<Vec<String>>,
}

/// Async HTTP client for the AURCache API.
pub struct AurCacheClient {
    base_url: String,
    token: Option<String>,
    client: reqwest::Client,
}

impl AurCacheClient {
    /// Creates a new client for the given base URL and optional bearer token.
    ///
    /// `base_url` should normally point at the API root, for example
    /// `http://localhost:8080/api`.
    pub fn new(base_url: String, token: Option<String>) -> Result<Self> {
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            token,
            client: reqwest::Client::builder()
                .build()
                .context("failed to build HTTP client")?,
        })
    }

    /// Calls the health endpoint and returns success when the instance is healthy.
    pub async fn health(&self) -> Result<()> {
        self.request_empty::<Value>(Method::GET, "/health", &[], None)
            .await
    }

    /// Fetches information about the currently authenticated user.
    pub async fn user_info(&self) -> Result<UserInfo> {
        self.request_json::<UserInfo, Value>(Method::GET, "/userinfo", &[], None)
            .await
    }

    /// Fetches aggregate dashboard statistics.
    pub async fn stats(&self) -> Result<ListStats> {
        self.request_json::<ListStats, Value>(Method::GET, "/stats", &[], None)
            .await
    }

    /// Fetches monthly dashboard graph datapoints.
    pub async fn graph(&self) -> Result<Vec<GraphDataPoint>> {
        self.request_json::<Vec<GraphDataPoint>, Value>(Method::GET, "/graph", &[], None)
            .await
    }

    /// Searches for packages through the AUR-backed search endpoint.
    pub async fn search(&self, query: &str) -> Result<Vec<SearchResult>> {
        self.request_json::<Vec<SearchResult>, Value>(
            Method::GET,
            "/search",
            &[("query".to_string(), query.to_string())],
            None,
        )
        .await
    }

    /// Regenerates the authenticated user's API token.
    pub async fn regenerate_api_token(&self) -> Result<ApiTokenResponse> {
        self.request_json::<ApiTokenResponse, Value>(Method::POST, "/token/regenerate", &[], None)
            .await
    }

    /// Lists directly requested packages.
    pub async fn list_packages(
        &self,
        limit: Option<u64>,
        page: Option<u64>,
    ) -> Result<Vec<SimplePackage>> {
        let query = optional_u64_query(&[("limit", limit), ("page", page)]);
        self.request_json::<Vec<SimplePackage>, Value>(Method::GET, "/packages/list", &query, None)
            .await
    }

    /// Fetches details for a single package id.
    pub async fn get_package(&self, id: i32) -> Result<ExtendedPackage> {
        self.request_json::<ExtendedPackage, Value>(
            Method::GET,
            &format!("/package/{id}"),
            &[],
            None,
        )
        .await
    }

    /// Adds a package using the supplied request payload.
    pub async fn add_package(&self, body: &AddPackageRequest) -> Result<()> {
        self.request_empty(Method::POST, "/package", &[], Some(body))
            .await
    }

    /// Triggers an update check for the given package id.
    ///
    /// Returns any package ids queued as a result of the update.
    pub async fn update_package(&self, id: i32, body: &UpdatePackageRequest) -> Result<Vec<i32>> {
        self.request_json(
            Method::POST,
            &format!("/package/{id}/update"),
            &[],
            Some(body),
        )
        .await
    }

    /// Partially updates package metadata.
    pub async fn patch_package(&self, id: i32, body: &PatchPackageRequest) -> Result<()> {
        self.request_empty(Method::PATCH, &format!("/package/{id}"), &[], Some(body))
            .await
    }

    /// Removes the direct-request flag from the given package.
    pub async fn delete_package(&self, id: i32) -> Result<()> {
        self.request_empty::<Value>(Method::DELETE, &format!("/package/{id}"), &[], None)
            .await
    }

    /// Lists builds, optionally filtered by package id.
    pub async fn list_builds(
        &self,
        package_id: Option<i32>,
        limit: Option<u64>,
        page: Option<u64>,
    ) -> Result<Vec<Build>> {
        let query = with_optional_i32(
            optional_u64_query(&[("limit", limit), ("page", page)]),
            "pkgid",
            package_id,
        );
        self.request_json::<Vec<Build>, Value>(Method::GET, "/builds", &query, None)
            .await
    }

    /// Fetches details for a single build id.
    pub async fn get_build(&self, id: i32) -> Result<Build> {
        self.request_json::<Build, Value>(Method::GET, &format!("/build/{id}"), &[], None)
            .await
    }

    /// Fetches raw build output text.
    ///
    /// When `start_line` is provided, lines before that offset are skipped.
    pub async fn build_output(&self, id: i32, start_line: Option<i32>) -> Result<String> {
        let query = optional_i32_query("startline", start_line);
        self.request_text::<Value>(Method::GET, &format!("/build/{id}/output"), &query, None)
            .await
    }

    /// Retries the given build and returns the new build id.
    pub async fn retry_build(&self, id: i32) -> Result<i32> {
        self.request_json::<i32, Value>(Method::POST, &format!("/build/{id}/retry"), &[], None)
            .await
    }

    /// Requests cancellation of the given build.
    pub async fn cancel_build(&self, id: i32) -> Result<()> {
        self.request_empty::<Value>(Method::POST, &format!("/build/{id}/cancel"), &[], None)
            .await
    }

    /// Deletes the given build record.
    pub async fn delete_build(&self, id: i32) -> Result<()> {
        self.request_empty::<Value>(Method::DELETE, &format!("/build/{id}"), &[], None)
            .await
    }

    /// Sends a request and decodes the response body as JSON.
    ///
    /// This is intended for endpoints that are not yet wrapped by a typed helper.
    pub async fn request_json<T, B>(
        &self,
        method: Method,
        path: &str,
        query: &[(String, String)],
        body: Option<&B>,
    ) -> Result<T>
    where
        T: DeserializeOwned,
        B: Serialize + ?Sized,
    {
        let response = self.send(method, path, query, body).await?;
        let response = ensure_success(response).await?;
        response
            .json::<T>()
            .await
            .with_context(|| format!("failed to decode JSON response from {path}"))
    }

    /// Sends a request that is expected to return no meaningful response body.
    pub async fn request_empty<B>(
        &self,
        method: Method,
        path: &str,
        query: &[(String, String)],
        body: Option<&B>,
    ) -> Result<()>
    where
        B: Serialize + ?Sized,
    {
        let response = self.send(method, path, query, body).await?;
        let _ = ensure_success(response).await?;
        Ok(())
    }

    /// Sends a request and returns the raw response body as text.
    pub async fn request_text<B>(
        &self,
        method: Method,
        path: &str,
        query: &[(String, String)],
        body: Option<&B>,
    ) -> Result<String>
    where
        B: Serialize + ?Sized,
    {
        let response = self.send(method, path, query, body).await?;
        let response = ensure_success(response).await?;
        response
            .text()
            .await
            .with_context(|| format!("failed to read text response from {path}"))
    }

    async fn send<B>(
        &self,
        method: Method,
        path: &str,
        query: &[(String, String)],
        body: Option<&B>,
    ) -> Result<Response>
    where
        B: Serialize + ?Sized,
    {
        let mut url = reqwest::Url::parse(&endpoint_url(&self.base_url, path))
            .with_context(|| format!("invalid URL for path {path}"))?;
        {
            let mut pairs = url.query_pairs_mut();
            for (key, value) in query {
                pairs.append_pair(key, value);
            }
        }

        let mut request = self.client.request(method, url);
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        if let Some(body) = body {
            request = request.json(body);
        }

        request.send().await.context("request failed")
    }
}

fn endpoint_url(base_url: &str, path: &str) -> String {
    if path.starts_with("http://") || path.starts_with("https://") {
        path.to_string()
    } else {
        format!(
            "{}/{}",
            base_url.trim_end_matches('/'),
            path.trim_start_matches('/')
        )
    }
}

fn optional_u64_query(values: &[(&str, Option<u64>)]) -> Vec<(String, String)> {
    values
        .iter()
        .filter_map(|(key, value)| value.map(|value| ((*key).to_string(), value.to_string())))
        .collect()
}

fn optional_i32_query(key: &str, value: Option<i32>) -> Vec<(String, String)> {
    value
        .map(|value| vec![(key.to_string(), value.to_string())])
        .unwrap_or_default()
}

fn with_optional_i32(
    mut query: Vec<(String, String)>,
    key: &str,
    value: Option<i32>,
) -> Vec<(String, String)> {
    if let Some(value) = value {
        query.push((key.to_string(), value.to_string()));
    }
    query
}

async fn ensure_success(response: Response) -> Result<Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }

    let body = response.text().await.unwrap_or_default();
    let body = normalize_error_body(&body);
    if body.is_empty() {
        bail!("request failed with HTTP {status}");
    }
    bail!("request failed with HTTP {status}: {body}")
}

fn normalize_error_body(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if let Ok(message) = serde_json::from_str::<String>(trimmed) {
        return message;
    }
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        return serde_json::to_string_pretty(&value).unwrap_or_else(|_| trimmed.to_string());
    }
    trimmed.to_string()
}

#[cfg(test)]
mod tests {
    use super::endpoint_url;

    #[test]
    fn endpoint_url_trims_slashes() {
        assert_eq!(
            endpoint_url("http://localhost:8080/api/", "/packages/list"),
            "http://localhost:8080/api/packages/list"
        );
    }
}
