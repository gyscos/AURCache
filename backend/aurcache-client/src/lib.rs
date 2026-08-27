//! Typed Rust client for the AURCache HTTP API.
//!
//! This crate exposes request/response models for the API together with
//! [`AurCacheClient`], a small async wrapper around the most common endpoints.

use anyhow::{Context, Result};
// The API shapes are defined once, in aurcache-types, and used by the server,
// this client, and the browser frontend alike. Types still declared below are
// ones whose server-side counterpart has a different shape or name; converging
// those is the remaining half of the job.
pub use aurcache_types::api::activity::Activity;
pub use aurcache_types::api::aur::ApiPackage;
pub use aurcache_types::api::builds::BuildSummary as Build;
pub use aurcache_types::api::package::{
    AurNotFoundPackage, AurPackage, PackageSource, UploadPackage,
};
pub use aurcache_types::api::package::{ExtendedPackage, PackageDependency, SimplePackage};
// The add and preview requests are the server's own shapes rather than copies:
// they were duplicated here, so a field added to one was silently absent from
// the other.
pub use aurcache_types::api::package::{
    AddPackage as AddPackageRequest, SourcePreviewFileRequest, SourcePreviewRequest,
};
pub use aurcache_types::api::package::{SourceFileContent, SourceFileList, SourceFileUpdate};
pub use aurcache_types::api::settings::{SettingResponse, SettingValue};
pub use aurcache_types::api::stats::{GraphDataPoint, UserInfo};
pub use aurcache_types::api::waiting::WaitingReason;
pub use aurcache_types::settings::{
    ApplicationSettings, Setting, SettingSource, SettingsEntry, SettingsMeta,
};
pub use aurcache_types::source::{GitSourceSpec, SourceData, looks_like_git_url};
use reqwest::Response;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Re-export of [`reqwest::Method`] for generic request helpers.
pub use reqwest::Method;

/// A registered build worker.
#[derive(Debug, Serialize, Deserialize)]
pub struct Worker {
    /// Internal worker id.
    pub id: i32,
    /// Operator-facing worker name reported at enrollment.
    pub name: String,
    /// Enrollment status: `pending`, `approved`, or `revoked`.
    pub status: String,
    /// SHA-256 fingerprint of the worker's certificate/CSR (stable identity).
    pub cert_fingerprint: String,
    /// Comma-separated architectures the worker builds natively.
    pub native_arches: String,
    /// Comma-separated architectures the worker can build via emulation.
    pub emulated_arches: String,
    /// Unix seconds of the last heartbeat/contact, if ever seen.
    pub last_seen: Option<i64>,
    /// Worker software version reported at enrollment/heartbeat.
    pub version: Option<String>,
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

/// Search result returned from the AUR search proxy endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchResult {
    /// Package name.
    pub name: String,
    /// Upstream version string reported by the search backend.
    pub version: String,
}

/// Request payload for triggering a package update check.
#[derive(Debug, Serialize)]
pub struct UpdatePackageRequest {
    /// Whether to force the update even when the version did not change.
    pub force: bool,
}

/// Request payload for partially updating package metadata.
#[derive(Debug, Default, Serialize)]
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
        let query = Query::default().opt("limit", limit).opt("page", page);
        self.request_json::<Vec<SimplePackage>, Value>(
            Method::GET,
            "/packages/list",
            query.pairs(),
            None,
        )
        .await
    }

    /// Fetches details for a single package id.
    pub async fn get_package(&self, pkgbase: &str) -> Result<ExtendedPackage> {
        self.request_json::<ExtendedPackage, Value>(
            Method::GET,
            &format!("/package/{pkgbase}"),
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

    /// Triggers an update check for the given package.
    ///
    /// Returns the build number, within that package, of each build queued as
    /// a result of the update — one per platform that got one.
    pub async fn update_package(
        &self,
        pkgbase: &str,
        body: &UpdatePackageRequest,
    ) -> Result<Vec<i32>> {
        self.request_json(
            Method::POST,
            &format!("/package/{pkgbase}/update"),
            &[],
            Some(body),
        )
        .await
    }

    /// Partially updates package metadata.
    pub async fn patch_package(&self, pkgbase: &str, body: &PatchPackageRequest) -> Result<()> {
        self.request_empty(
            Method::PATCH,
            &format!("/package/{pkgbase}"),
            &[],
            Some(body),
        )
        .await
    }

    /// Removes the direct-request flag from the given package.
    pub async fn delete_package(&self, pkgbase: &str) -> Result<()> {
        self.request_empty::<Value>(Method::DELETE, &format!("/package/{pkgbase}"), &[], None)
            .await
    }

    /// Lists builds, optionally filtered by package id.
    pub async fn list_builds(
        &self,
        pkgbase: Option<&str>,
        limit: Option<u64>,
        page: Option<u64>,
    ) -> Result<Vec<Build>> {
        let query = Query::default().opt("limit", limit).opt("page", page);
        // Builds for one package are a sub-resource, not a query filter: a
        // pkgbase may contain `+`, which is literal in a path but decodes to a
        // space in a query value.
        let path = match pkgbase {
            Some(pkgbase) => format!("/package/{pkgbase}/builds"),
            None => "/builds".to_string(),
        };
        self.request_json::<Vec<Build>, Value>(Method::GET, &path, query.pairs(), None)
            .await
    }

    /// Lists the files in a package's source tree.
    pub async fn list_source_files(&self, pkgbase: &str) -> Result<SourceFileList> {
        self.request_json::<SourceFileList, Value>(
            Method::GET,
            &format!("/package/{pkgbase}/source/files"),
            &[],
            None,
        )
        .await
    }

    /// Reads one source file, pristine and patched.
    pub async fn get_source_file(&self, pkgbase: &str, path: &str) -> Result<SourceFileContent> {
        let query = Query::default().opt("path", Some(path));
        self.request_json::<SourceFileContent, Value>(
            Method::GET,
            &format!("/package/{pkgbase}/source/file"),
            query.pairs(),
            None,
        )
        .await
    }

    /// Replaces one source file's content. The server stores the difference
    /// from the pristine source as the package's patch; writing back the
    /// original content is therefore how a file is un-patched.
    pub async fn put_source_file(&self, pkgbase: &str, path: &str, content: &str) -> Result<()> {
        let body = SourceFileUpdate {
            path: path.to_string(),
            content: content.to_string(),
        };
        self.request_empty(
            Method::PUT,
            &format!("/package/{pkgbase}/source/file"),
            &[],
            Some(&body),
        )
        .await
    }

    /// Fetches details for a single build id.
    /// Fetch one build by its public identity, `<pkgbase>/<number>`.
    pub async fn get_build(&self, pkgbase: &str, number: i32) -> Result<Build> {
        self.request_json::<Build, Value>(
            Method::GET,
            &format!("/package/{pkgbase}/build/{number}"),
            &[],
            None,
        )
        .await
    }

    /// Fetches raw build output text.
    ///
    /// When `start_line` is provided, lines before that offset are skipped.
    pub async fn build_output(
        &self,
        pkgbase: &str,
        number: i32,
        start_line: Option<i32>,
    ) -> Result<String> {
        let query = Query::default().opt("startline", start_line);
        self.request_text::<Value>(
            Method::GET,
            &format!("/package/{pkgbase}/build/{number}/output"),
            query.pairs(),
            None,
        )
        .await
    }

    /// Retries the given build and returns the new build id.
    /// Re-runs the given build, returning the new build's number within the
    /// same package.
    pub async fn retry_build(&self, pkgbase: &str, number: i32) -> Result<i32> {
        self.request_json::<i32, Value>(
            Method::POST,
            &format!("/package/{pkgbase}/build/{number}/retry"),
            &[],
            None,
        )
        .await
    }

    /// Requests cancellation of the given build.
    pub async fn cancel_build(&self, pkgbase: &str, number: i32) -> Result<()> {
        self.request_empty::<Value>(
            Method::POST,
            &format!("/package/{pkgbase}/build/{number}/cancel"),
            &[],
            None,
        )
        .await
    }

    /// Deletes the given build record.
    pub async fn delete_build(&self, pkgbase: &str, number: i32) -> Result<()> {
        self.request_empty::<Value>(
            Method::DELETE,
            &format!("/package/{pkgbase}/build/{number}"),
            &[],
            None,
        )
        .await
    }

    /// Lists the files of a source that has not been added yet.
    ///
    /// The source travels in the body rather than being named by pkgbase,
    /// because there is no package to name: this is what lets a PKGBUILD be
    /// inspected — and fixed — before the package exists.
    pub async fn preview_source_files(&self, source: &SourceData) -> Result<SourceFileList> {
        self.request_json(
            Method::POST,
            "/package/source/preview/files",
            &[],
            Some(&SourcePreviewRequest {
                source: source.clone(),
            }),
        )
        .await
    }

    /// Reads one pristine file from a source that has not been added yet.
    pub async fn preview_source_file(
        &self,
        source: &SourceData,
        path: &str,
    ) -> Result<SourceFileContent> {
        self.request_json(
            Method::POST,
            "/package/source/preview/file",
            &[],
            Some(&SourcePreviewFileRequest {
                source: source.clone(),
                path: path.to_string(),
            }),
        )
        .await
    }

    /// Fetches every setting with the value in force and where it came from.
    ///
    /// Pass a `pkgbase` for the per-package view, where a setting the package
    /// does not override reports the global value it inherits.
    pub async fn settings(&self, pkgbase: Option<&str>) -> Result<ApplicationSettings> {
        self.request_json::<ApplicationSettings, Value>(
            Method::GET,
            &self.settings_path(pkgbase, None),
            &[],
            None,
        )
        .await
    }

    /// Stores a value for one setting, overriding whatever it inherits.
    ///
    /// The value is sent as a string whatever its type: the server owns the
    /// parsing, so a client that formats a number differently cannot store
    /// something the server would reject on read.
    pub async fn patch_setting(&self, pkgbase: Option<&str>, key: &str, value: &str) -> Result<()> {
        self.request_empty(
            Method::PATCH,
            &self.settings_path(pkgbase, Some(key)),
            &[],
            Some(&SettingValue {
                value: value.to_string(),
            }),
        )
        .await
    }

    /// Drops this scope's stored value, so the setting inherits again.
    pub async fn reset_setting(&self, pkgbase: Option<&str>, key: &str) -> Result<()> {
        self.request_empty::<Value>(
            Method::DELETE,
            &self.settings_path(pkgbase, Some(key)),
            &[],
            None,
        )
        .await
    }

    /// Settings are a sub-resource of the package in the per-package scope, so
    /// every one of the calls above has the same two shapes.
    fn settings_path(&self, pkgbase: Option<&str>, key: Option<&str>) -> String {
        let mut path = match pkgbase {
            Some(pkgbase) => format!("/package/{pkgbase}/settings"),
            None => "/settings".to_string(),
        };
        if let Some(key) = key {
            path.push('/');
            path.push_str(key);
        }
        path
    }

    /// The most recent entries in the activity log, newest first.
    pub async fn activities(&self, limit: Option<u64>) -> Result<Vec<Activity>> {
        let query = Query::default().opt("limit", limit);
        self.request_json::<Vec<Activity>, Value>(Method::GET, "/activity", query.pairs(), None)
            .await
    }

    /// Lists all enrolled remote build workers and their status.
    pub async fn list_workers(&self) -> Result<Vec<Worker>> {
        self.request_json::<Vec<Worker>, Value>(Method::GET, "/workers", &[], None)
            .await
    }

    /// Approves a pending worker, signing its CSR so it can build.
    pub async fn approve_worker(&self, id: i32) -> Result<()> {
        self.request_empty::<Value>(Method::POST, &format!("/workers/{id}/approve"), &[], None)
            .await
    }

    /// Revokes a worker, immediately refusing its certificate on the next call.
    pub async fn revoke_worker(&self, id: i32) -> Result<()> {
        self.request_empty::<Value>(Method::POST, &format!("/workers/{id}/revoke"), &[], None)
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
        ensure_success(response).await?;
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

/// Accumulates the `?key=value` pairs of a request, skipping absent values.
#[derive(Default)]
struct Query(Vec<(String, String)>);

impl Query {
    /// Append `key=value` if `value` is `Some`.
    fn opt<T: ToString>(mut self, key: &str, value: Option<T>) -> Self {
        if let Some(value) = value {
            self.0.push((key.to_string(), value.to_string()));
        }
        self
    }

    fn pairs(&self) -> &[(String, String)] {
        &self.0
    }
}

async fn ensure_success(response: Response) -> Result<Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }

    let body = response.text().await.unwrap_or_default();
    Err(ApiError::from_response(status, &body).into())
}

/// A structured error for a non-2xx API response.
///
/// Server error pages are not guaranteed to be plain text (Rocket's default
/// error pages, for instance, are HTML), so this normalizes the body into a
/// short, human-readable message instead of dumping raw markup, and exposes
/// the status code so callers (like the CLI) can react to specific cases
/// such as authentication failures.
#[derive(Debug)]
pub struct ApiError {
    pub status: reqwest::StatusCode,
    pub message: String,
}

impl ApiError {
    fn from_response(status: reqwest::StatusCode, body: &str) -> Self {
        let message = if status == reqwest::StatusCode::UNAUTHORIZED {
            "Authentication failed: missing or invalid API token".to_string()
        } else {
            normalize_error_body(body)
        };
        Self { status, message }
    }

    /// Whether this error corresponds to an HTTP 401 Unauthorized response.
    pub fn is_unauthorized(&self) -> bool {
        self.status == reqwest::StatusCode::UNAUTHORIZED
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.message.is_empty() {
            write!(f, "request failed with HTTP {}", self.status)
        } else {
            write!(
                f,
                "request failed with HTTP {}: {}",
                self.status, self.message
            )
        }
    }
}

impl std::error::Error for ApiError {}

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
    // Fall back for non-JSON bodies: avoid dumping raw HTML error pages
    // (e.g. Rocket's default catchers) in favor of a short, readable message.
    if looks_like_html(trimmed) {
        return String::new();
    }
    trimmed.to_string()
}

fn looks_like_html(body: &str) -> bool {
    let lower = body.trim_start().to_ascii_lowercase();
    lower.starts_with("<!doctype html") || lower.starts_with("<html")
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
