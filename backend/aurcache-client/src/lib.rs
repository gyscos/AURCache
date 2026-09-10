//! Typed Rust client for the AURCache HTTP API.
//!
//! This crate exposes request/response models for the API together with
//! [`AurCacheClient`], a small async wrapper around the most common endpoints.

use anyhow::{Context, Result};
use reqwest::Url;
// The API shapes are defined once, in aurcache-common, and used by the server,
// this client, and the browser frontend alike. Types still declared below are
// ones whose server-side counterpart has a different shape or name; converging
// those is the remaining half of the job.
pub use aurcache_common::api::activity::Activity;
pub use aurcache_common::api::aur::ApiPackage;
pub use aurcache_common::api::builds::BuildSummary as Build;
pub use aurcache_common::api::dump::{
    RestoreAccepted, RestoreEntry, RestoreOutcome, RestoreProgress,
};
pub use aurcache_common::api::operations::{ActiveOperation, kind as operation_kind};
pub use aurcache_common::api::package::{
    AddPackages as AddPackagesRequest, BulkAddAccepted, BulkAddEntry, BulkAddOutcome,
    BulkAddProgress, ExtendedPackage, PackageDependency, PackageFile, SimplePackage,
};
pub use aurcache_common::api::package::{
    AurNotFoundPackage, AurPackage, PackageSource, UploadPackage,
};
pub use aurcache_common::api::package::{
    CandidateSource, DependencyCandidate, DependencyOptions, ReplaceDependency, ReplacementVerdict,
};
// The add and preview requests are the server's own shapes rather than copies:
// they were duplicated here, so a field added to one was silently absent from
// the other.
pub use aurcache_common::api::package::{
    AddPackage as AddPackageRequest, SourcePreviewFileRequest, SourcePreviewRequest,
};
pub use aurcache_common::api::package::{SourceFileContent, SourceFileList, SourceFileUpdate};
pub use aurcache_common::api::repo::RepoInfo;
pub use aurcache_common::api::settings::{SettingResponse, SettingValue};
pub use aurcache_common::api::stats::{GraphDataPoint, ListStats, UserInfo};
pub use aurcache_common::api::waiting::WaitingReason;
pub use aurcache_common::api::worker::{ApprovalStatus, WorkerJoinInfo, WorkerSummary as Worker};
pub use aurcache_common::settings::{
    ApplicationSettings, Setting, SettingSource, SettingsEntry, SettingsMeta,
};
pub use aurcache_common::source::{GitSourceSpec, SourceData, looks_like_git_url};
use reqwest::Response;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Re-export of [`reqwest::Method`] for generic request helpers.
pub use reqwest::Method;

/// Response returned when a personal API token is regenerated.
#[derive(Debug, Serialize, Deserialize)]
pub struct ApiTokenResponse {
    /// Newly generated plaintext token.
    ///
    /// The server only returns this value at generation time.
    pub token: String,
}

/// Search result returned from the AUR search proxy endpoint.
///
/// The server's own shape rather than a copy of it: this was a duplicate
/// declaration, so a field added to one silently did not exist on the other.
pub use aurcache_common::api::aur::ApiPackage as SearchResult;

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
/// Where a request for the API actually landed.
///
/// Reaching *something* is not the same as reaching the API. Pointed at the
/// web UI, the server answers `/health` with a redirect to its login page, and
/// that page returns a perfectly good 200 -- so a bare status check calls the
/// URL healthy and the mistake resurfaces as an unrelated complaint about the
/// token at the next call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiReachability {
    /// The API answered directly.
    Api,
    /// Something answered, but not the API, and `detail` says how that was
    /// established: the request was redirected away, or the answer was a web
    /// page where the API returns none.
    NotApi { detail: String },
}

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
    /// The API base URL this client was built with.
    ///
    /// Exposed because tooling has to talk about the *deployment* as well as
    /// the API: the pacman repository and the worker protocol are the same host
    /// on other ports, and this is where that host comes from.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// How this instance publishes its pacman repository.
    pub async fn repo_info(&self) -> Result<RepoInfo> {
        self.request_json::<RepoInfo, Value>(Method::GET, "/repo/info", &[], None)
            .await
    }

    pub async fn health(&self) -> Result<()> {
        match self.probe_api().await? {
            ApiReachability::Api => Ok(()),
            ApiReachability::NotApi { detail } => Err(anyhow::anyhow!(
                "{} answered, but {detail}: that is the web UI, not the API",
                endpoint_url(&self.base_url, "/health")
            )),
        }
    }

    /// Ask the health endpoint whether this URL is the API, and where the
    /// request ended up if it is not.
    pub async fn probe_api(&self) -> Result<ApiReachability> {
        let requested = endpoint_url(&self.base_url, "/health");
        let response = self
            .send::<Value>(Method::GET, "/health", &[], None)
            .await?;
        let response = ensure_success(response).await?;
        let landed_on = response.url().to_string();
        if !same_endpoint(&requested, &landed_on) {
            return Ok(ApiReachability::NotApi {
                detail: format!("the request was redirected to {landed_on}"),
            });
        }
        // Not every wrong URL redirects. The UI is a single-page app served by
        // the same host, so with a token in hand `/health` comes back as a
        // perfectly good 200 -- carrying `index.html`. The API answers it with
        // an empty body, so the page itself is the tell.
        let body = response
            .text()
            .await
            .with_context(|| format!("failed to read the response from {requested}"))?;
        Ok(if looks_like_html(&body) {
            ApiReachability::NotApi {
                detail: "the answer was a web page".to_string(),
            }
        } else {
            ApiReachability::Api
        })
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

    /// Lists packages.
    ///
    /// Only the directly requested ones unless `dependencies` is set, which
    /// adds the packages that are present only because something needs them.
    pub async fn list_packages(
        &self,
        limit: Option<u64>,
        page: Option<u64>,
        dependencies: bool,
    ) -> Result<Vec<SimplePackage>> {
        let query = Query::default()
            .opt("limit", limit)
            .opt("page", page)
            .opt("dependencies", dependencies.then_some(true));
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

    /// Starts adding several packages in one request, returning before any of
    /// them are added.
    ///
    /// The server resolves every AUR name to its pkgbase in one batched RPC
    /// call, so this is not the same as calling [`Self::add_package`] in a
    /// loop: adding N packages that way costs N AUR requests for the
    /// resolution alone. Poll [`Self::bulk_add_progress`] with the returned id
    /// to follow it, or do not -- the work does not depend on being watched.
    pub async fn add_packages(&self, body: &AddPackagesRequest) -> Result<BulkAddAccepted> {
        self.request_json(Method::POST, "/packages", &[], Some(body))
            .await
    }

    /// Reads a bulk add's progress, returning only the entries after the first
    /// `after` of them.
    ///
    /// Pass the number already held so each poll returns just what is new;
    /// pass zero to get the run from the beginning, including whatever happened
    /// before this caller started watching.
    pub async fn bulk_add_progress(&self, job_id: i32, after: usize) -> Result<BulkAddProgress> {
        self.request_json::<BulkAddProgress, ()>(
            Method::GET,
            &format!("/packages/bulk/{job_id}"),
            &[("after".to_string(), after.to_string())],
            None,
        )
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

    /// What could take over one of a package's dependencies.
    pub async fn dependency_options(
        &self,
        pkgbase: &str,
        dependency: &str,
    ) -> Result<DependencyOptions> {
        self.request_json::<DependencyOptions, Value>(
            Method::GET,
            &format!("/package/{pkgbase}/dependency/{dependency}/options"),
            &[],
            None,
        )
        .await
    }

    /// Points one of a package's dependencies at `replacement`, or drops the
    /// dependency entirely when it is `None`.
    ///
    /// Dropping is accepted only where the official repositories publish the
    /// name, since anything else would be undone at the next update.
    pub async fn replace_dependency(
        &self,
        pkgbase: &str,
        dependency: &str,
        replacement: Option<&str>,
    ) -> Result<()> {
        self.request_empty(
            Method::PUT,
            &format!("/package/{pkgbase}/dependency/{dependency}"),
            &[],
            Some(&ReplaceDependency {
                replacement: replacement.map(ToString::to_string),
            }),
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

    /// Long-running operations still in flight.
    ///
    /// A bulk add or restore outlives the page that started it, so this is how
    /// a browser finds one again -- after a reload, or from a tab that never
    /// started it.
    pub async fn active_operations(&self) -> Result<Vec<ActiveOperation>> {
        self.request_json::<Vec<ActiveOperation>, Value>(Method::GET, "/operations", &[], None)
            .await
    }

    /// Fetches raw build output text.
    ///
    /// `offset` is how many *bytes* of the log the caller already has; the
    /// response starts there. Bytes rather than lines because the server seeks
    /// to the offset in a file, which is proportional to what is being read
    /// rather than to the size of the whole log. An offset returned by a
    /// previous call always lands on a character boundary.
    pub async fn build_output(
        &self,
        pkgbase: &str,
        number: i32,
        offset: Option<u64>,
    ) -> Result<String> {
        let query = Query::default().opt("offset", offset);
        self.request_text::<Value>(
            Method::GET,
            &format!("/package/{pkgbase}/build/{number}/output"),
            query.pairs(),
            None,
        )
        .await
    }

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

    /// Reads one setting: the value in force, and where it came from.
    ///
    /// Separate from [`Self::settings`] because not every setting is in that
    /// response — the config files are large text blobs, and sending both of
    /// them with every settings fetch would be wasteful.
    pub async fn get_setting(&self, pkgbase: Option<&str>, key: &str) -> Result<SettingResponse> {
        self.request_json::<SettingResponse, Value>(
            Method::GET,
            &self.settings_path(pkgbase, Some(key)),
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

    /// The image and worker-protocol port a new worker's `docker run` needs.
    pub async fn worker_join_info(&self) -> Result<WorkerJoinInfo> {
        self.request_json::<WorkerJoinInfo, Value>(Method::GET, "/workers/join-info", &[], None)
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
        // Read the body first: `json()` consumes the response, leaving nothing
        // to explain the failure with beyond serde's "expected value at line 1
        // column 1", which describes the symptom and not the cause.
        let body = response
            .text()
            .await
            .with_context(|| format!("failed to read the response from {path}"))?;
        serde_json::from_str(&body).with_context(|| json_decode_context(path, &body))
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

    /// Fetch a binary response body.
    ///
    /// Separate from [`Self::request_text`] because a dump is a `.tar.gz`:
    /// decoding it as UTF-8 would corrupt it.
    pub async fn request_bytes<B>(
        &self,
        method: Method,
        path: &str,
        query: &[(String, String)],
        body: Option<&B>,
    ) -> Result<Vec<u8>>
    where
        B: Serialize + ?Sized,
    {
        let response = self.send(method, path, query, body).await?;
        let response = ensure_success(response).await?;
        Ok(response
            .bytes()
            .await
            .with_context(|| format!("failed to read response body from {path}"))?
            .to_vec())
    }

    /// Restore a dump, returning before it has finished.
    ///
    /// A dry run reports what it *would* do and no job id, because it changed
    /// nothing to watch.
    pub async fn restore(
        &self,
        archive: Vec<u8>,
        dry_run: bool,
        on_existing: &str,
        clear: bool,
        secrets: &str,
    ) -> Result<RestoreAccepted> {
        let mut url = reqwest::Url::parse(&endpoint_url(&self.base_url, "/restore"))
            .context("invalid restore URL")?;
        url.query_pairs_mut()
            .append_pair("dry_run", &dry_run.to_string())
            .append_pair("on_existing", on_existing)
            .append_pair("clear", &clear.to_string())
            .append_pair("secrets", secrets);
        let response = self
            .client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/gzip")
            .body(archive);
        let response = match &self.token {
            Some(token) => response.bearer_auth(token),
            None => response,
        };
        let response = ensure_success(response.send().await.context("request failed")?).await?;
        response
            .json()
            .await
            .context("failed to read the restore response")
    }

    /// Read a restore's progress, returning only entries after the first
    /// `after` of them.
    pub async fn restore_progress(&self, job_id: i32, after: usize) -> Result<RestoreProgress> {
        self.request_json::<RestoreProgress, ()>(
            Method::GET,
            &format!("/restore/{job_id}"),
            &[("after".to_string(), after.to_string())],
            None,
        )
        .await
    }

    /// Download a lite export of the server's authored state.
    ///
    /// `include_secrets` adds the CA private key, the worker certificates and
    /// the API token hashes. The resulting file can mint worker identities this
    /// server accepts; treat it as a credential.
    pub async fn dump(&self, include_secrets: bool) -> Result<Vec<u8>> {
        self.request_bytes::<()>(
            Method::GET,
            "/dump",
            &[("include_secrets".to_string(), include_secrets.to_string())],
            None,
        )
        .await
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

/// Whether a response came back from the endpoint that was asked for.
///
/// Compared by scheme, host, port and path rather than as text: an empty query
/// string is spelled with a trailing `?` in the response's URL and not in ours,
/// and a trailing slash is not a different endpoint by any useful definition.
/// Textual comparison calls both a redirect and reports a correct URL as the
/// web UI.
fn same_endpoint(requested: &str, landed_on: &str) -> bool {
    match (Url::parse(requested), Url::parse(landed_on)) {
        (Ok(a), Ok(b)) => {
            a.scheme() == b.scheme()
                && a.host_str() == b.host_str()
                && a.port_or_known_default() == b.port_or_known_default()
                && a.path().trim_end_matches('/') == b.path().trim_end_matches('/')
        }
        _ => requested.trim_end_matches('/') == landed_on.trim_end_matches('/'),
    }
}

/// Why a JSON decode failed, in terms the caller can act on.
///
/// A body opening with `<` is the web UI's HTML, which means the base URL names
/// the site rather than the API. That is the common way to arrive here, and
/// serde describes it as "expected value at line 1 column 1" -- an accurate
/// account of the first byte and no help at all.
fn json_decode_context(path: &str, body: &str) -> String {
    if looks_like_html(body) {
        format!(
            "expected JSON from {path} but the server returned an HTML page: the URL looks \
             like the web UI rather than the API (try adding `/api`)"
        )
    } else {
        format!("failed to decode JSON response from {path}")
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

    /// The web UI answers with HTML and a 200, so the decode is where the
    /// mistake first becomes visible -- and "expected value at line 1 column 1"
    /// sends the reader looking at the wrong thing entirely.
    #[test]
    fn an_html_body_is_reported_as_the_wrong_url() {
        let msg = super::json_decode_context("/userinfo", "<!doctype html><html>...");
        assert!(msg.contains("HTML"), "{msg}");
        assert!(msg.contains("/api"), "{msg}");

        let msg = super::json_decode_context("/userinfo", "{\"unexpected\": true}");
        assert!(!msg.contains("HTML"), "{msg}");
    }

    /// The UI is served by the same host as the API, so a wrong URL need not
    /// redirect anywhere: `/health` answers 200 with `index.html`, where the
    /// API answers with nothing at all.
    #[test]
    fn a_page_is_not_an_api_answer() {
        assert!(super::looks_like_html("\n<!doctype html><html>"));
        assert!(!super::looks_like_html(""));
        assert!(!super::looks_like_html("{}"));
    }

    /// A redirect that only adds a trailing slash -- or the empty `?` reqwest
    /// puts on a URL built with no query parameters -- has not gone anywhere.
    /// Compared as text, that empty query reports every correct URL as the web
    /// UI.
    #[test]
    fn a_trailing_slash_is_the_same_endpoint() {
        assert!(super::same_endpoint(
            "http://host:8080/api/health",
            "http://host:8080/api/health/"
        ));
        assert!(super::same_endpoint(
            "http://host:8080/api/health",
            "http://host:8080/api/health?"
        ));
        assert!(!super::same_endpoint(
            "http://host:8080/health",
            "http://host:8080/api/login"
        ));
    }

    #[test]
    fn endpoint_url_trims_slashes() {
        assert_eq!(
            endpoint_url("http://localhost:8080/api/", "/packages/list"),
            "http://localhost:8080/api/packages/list"
        );
    }
}
