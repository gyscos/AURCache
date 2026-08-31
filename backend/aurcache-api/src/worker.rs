//! Remote-worker HTTP protocol (`/api/worker/*`) plus admin management
//! endpoints (`/api/workers`).
//!
//! Workers authenticate with mutual TLS: their client certificate is signed by
//! AURCache's internal CA and mapped to a `workers` row by the SHA-256
//! fingerprint of its public key. Enrollment endpoints (`register`, `ca`,
//! `register status`) are reachable without a client certificate so a worker
//! can bootstrap; all job endpoints require an *approved* worker's certificate.

use crate::models::authenticated::Authenticated;
use crate::utils::error::{ApiError, err};
use aurcache_ca::Ca;
use aurcache_common::api::worker::{ApprovalStatus, WorkerSummary};
use aurcache_common::builder::BuildStates;
use aurcache_common::worker::{
    ClaimRequest, CompleteReport, Heartbeat, JobDescriptor, JobStatus, RegisterRequest,
    RegisterStatus,
};
use aurcache_db::helpers::time::now_secs;
use aurcache_db::helpers::{worker_jobs, worker_store};
use aurcache_db::prelude::{Builds, Packages};
use aurcache_db::{builds, workers};
use aurcache_utils::build_logger::{BuildLogger, append_build_output};
use aurcache_utils::job_config::{build_job_config, mirrorlist_for};
use aurcache_utils::repo_ingest::{
    LeaseGuard, ingest_pkgs, is_debug_artifact, validate_artifact_names,
};
use aurcache_utils::snapshot::SnapshotStore;
use aurcache_utils::worker_complete;
use rocket::data::ToByteUnit;
use rocket::http::Status;
use rocket::mtls::Certificate;
use rocket::request::{FromRequest, Outcome, Request};
use rocket::serde::json::Json;
use rocket::{Data, State, get, post};
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QuerySelect};
use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use utoipa::OpenApi;

/// Lease/liveness tuning (env-overridable; see the design doc).
fn lease_ttl_secs() -> i64 {
    env_i64("LEASE_TTL", 60)
}
fn max_attempts() -> i32 {
    env_i64("MAX_ATTEMPTS", 3) as i32
}
/// How long a build must sit `ENQUEUED` before worker priority stops holding it
/// back. A backstop: `available()` is inferred from heartbeats and lease state,
/// so a worker can look healthy while never actually claiming (full disk, a bug).
/// This bounds the damage at one delay per job.
fn spill_delay_secs() -> i64 {
    env_i64("WORKER_SPILL_DELAY", 60)
}
/// How stale `last_seen` may be before a worker stops counting as available and
/// therefore stops holding jobs back. ~4x the 15s default heartbeat.
pub(crate) fn liveness_timeout_secs() -> i64 {
    env_i64("WORKER_LIVENESS_TIMEOUT", 60)
}
/// Worker certificates are transport plumbing, not a credential: authorization
/// is the `workers` row, so holding a valid certificate grants nothing on its
/// own. There is therefore nothing to gain from short validity, and no renewal
/// path exists (`ensure_enrolled` reuses a persisted certificate and the server
/// only signs when it has none) — so a short lifetime would simply brick the
/// worker. Match the server certificate's 10 years. See
/// `design/worker-routing.md` (Appendix).
fn worker_cert_validity_days() -> i64 {
    env_i64("WORKER_CERT_VALIDITY_DAYS", 3650)
}
fn env_i64(key: &str, default: i64) -> i64 {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Base URL workers should use for the public pacman repo (`[repo] Server`).
fn public_repo_url() -> String {
    env::var("AURCACHE_PUBLIC_URL").unwrap_or_else(|_| {
        format!(
            "http://localhost:{}",
            aurcache_common::ports::AURCACHE_MIRROR_PORT
        )
    })
}

/// A `[repo]` section for this instance with the host left as a placeholder.
///
/// Scheme, port and path come from `AURCACHE_PUBLIC_URL` because those describe
/// how the repository is *published*; only the host is replaced, because that
/// is the one part each worker knows better than the server does. A worker in
/// the compose network, one on the LAN and one embedded in this very container
/// all reach the same repository by different names.
fn repo_template() -> String {
    render_repo_template(&public_repo_url())
}

/// Pure form of [`repo_template`].
fn render_repo_template(public_url: &str) -> String {
    let trimmed = public_url.trim_end_matches('/');
    // Split off the scheme, then replace only the host portion of the
    // authority, keeping any port and path intact.
    let (scheme, rest) = match trimmed.split_once("://") {
        Some((scheme, rest)) => (scheme, rest),
        None => ("http", trimmed),
    };
    let (authority, path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, ""),
    };
    let port = authority.rsplit_once(':').map_or_else(
        || format!(":{}", aurcache_common::ports::AURCACHE_MIRROR_PORT),
        |(_, port)| format!(":{port}"),
    );
    let placeholder = aurcache_common::worker::REPO_HOST_PLACEHOLDER;
    format!("[repo]\nSigLevel = Never\nServer = {scheme}://{placeholder}{port}{path}/$arch\n")
}

/// Directory the server reads per-arch mirrorlists from (x86_64 only today).
fn mirrorlist_dir() -> PathBuf {
    PathBuf::from(env::var("AURCACHE_MIRRORLIST_DIR").unwrap_or_else(|_| "./repo".to_string()))
}

/// Conventional PKGDEST inside a worker's build environment. Workers may patch
/// this in their local `makepkg.conf`; it is only a default.
const WORKER_PKGDEST: &str = "/output";

/// Staging root where uploaded artifacts are buffered until the worker reports
/// the build complete, at which point they are ingested atomically.
fn staging_dir(build_id: i32) -> PathBuf {
    PathBuf::from("./worker-staging").join(build_id.to_string())
}

/// Request guard: a verified, CA-signed client certificate mapped to an
/// **approved** worker row. Pending/revoked/unknown workers are refused.
pub struct WorkerAuth {
    pub worker: workers::Model,
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for WorkerAuth {
    type Error = String;

    async fn from_request(req: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        let cert = match req.guard::<Certificate<'r>>().await {
            Outcome::Success(c) => c,
            Outcome::Forward(s) => return Outcome::Forward(s),
            Outcome::Error((s, _)) => {
                return Outcome::Error((s, "invalid client certificate".to_string()));
            }
        };
        // `Certificate` derefs to the TBS certificate; hash its SubjectPublicKeyInfo.
        let fingerprint = aurcache_ca::fingerprint_from_spki_der(cert.subject_pki.raw);

        let Some(db) = req.rocket().state::<DatabaseConnection>() else {
            return Outcome::Error((Status::InternalServerError, "no db".to_string()));
        };
        match worker_store::find_worker_by_fingerprint(db, &fingerprint).await {
            Ok(Some(worker)) if worker.status == ApprovalStatus::Approved => {
                let _ = worker_store::touch_last_seen(db, worker.id, None).await;
                Outcome::Success(Self { worker })
            }
            Ok(_) => Outcome::Error((Status::Forbidden, "worker not approved".to_string())),
            Err(e) => Outcome::Error((Status::InternalServerError, e.to_string())),
        }
    }
}

#[derive(OpenApi)]
#[openapi(paths(
    register_worker,
    register_status,
    get_ca,
    list_workers,
    approve_worker,
    revoke_worker
))]
pub struct WorkerApi;

/// Remote-worker **protocol** routes (mounted under `/api` on the dedicated
/// mTLS worker listener). Enrollment (`register`, `ca`, status) is reachable
/// without a client certificate; all job endpoints require an approved
/// worker's certificate.
#[must_use]
pub fn worker_protocol_routes() -> Vec<rocket::Route> {
    rocket::routes![
        register_worker,
        register_status,
        get_ca,
        get_ca_fingerprint,
        claim_job,
        job_source,
        job_logs,
        job_artifact,
        complete_job,
        heartbeat,
        job_status,
    ]
}

/// Worker **admin** routes (mounted under `/api` on the main HTTP API listener).
/// These use operator/session authentication, not mTLS, so they live on the
/// human-facing plane alongside the rest of the REST API and the web UI.
#[must_use]
pub fn worker_admin_routes() -> Vec<rocket::Route> {
    rocket::routes![list_workers, approve_worker, revoke_worker,]
}

// ----------------------------------------------------------------------------
// Enrollment (no client certificate required)
// ----------------------------------------------------------------------------

/// Register (or re-register) a worker. Idempotent by CSR fingerprint. The CSR is
/// signed immediately so the certificate is ready the moment an admin approves;
/// the worker only receives it (and may only use it) once approved.
#[utoipa::path(post, path = "/worker/register", responses((status = 200, body = RegisterStatus)))]
#[post("/worker/register", data = "<input>")]
pub async fn register_worker(
    db: &State<DatabaseConnection>,
    ca: &State<Ca>,
    input: Json<RegisterRequest>,
) -> Result<Json<RegisterStatus>, ApiError> {
    let db = db.inner();
    let input = input.into_inner();

    let fingerprint = aurcache_ca::fingerprint_from_csr_pem(&input.csr_pem)
        .map_err(|e| err(Status::BadRequest, e))?;

    let worker = worker_store::register_worker(
        db,
        &worker_store::WorkerRegistration {
            name: &input.name,
            fingerprint: &fingerprint,
            native_arches: &input.native_arches.join(","),
            emulated_arches: &input.emulated_arches.join(","),
            version: &input.version,
            package_affinity: &input.packages.join(","),
            priority: input.priority,
            // Clamped to at least 1: a worker reporting 0 would be treated as
            // permanently full and could never block a lower-priority worker,
            // silently defeating its own priority.
            concurrency: i32::try_from(input.concurrency.max(1)).unwrap_or(i32::MAX),
        },
    )
    .await
    .map_err(|e| err(Status::InternalServerError, e))?;

    // Issue the leaf certificate once and cache it on the worker row.
    if worker.signed_cert.is_none() {
        let signed = ca
            .sign_worker_csr(&input.csr_pem, worker_cert_validity_days())
            .map_err(|e| err(Status::InternalServerError, e))?;
        worker_store::store_signed_cert(db, worker.id, &signed.cert_pem, signed.not_after)
            .await
            .map_err(|e| err(Status::InternalServerError, e))?;
    }

    // Non-interactive enrollment: auto-approve when a configured mode matches.
    // Eligibility (pending only) is enforced inside `auto_approve_from_env`, so
    // a revoked worker is never re-approved by re-registering.
    if crate::worker_enroll::auto_approve_from_env(
        worker.status,
        &fingerprint,
        input.enrollment_token.as_deref(),
    ) {
        worker_store::approve_worker(db, worker.id)
            .await
            .map_err(|e| err(Status::InternalServerError, e))?;
        tracing::info!("Auto-approved worker '{}' ({fingerprint})", worker.name);
    }

    register_status_for(db, ca, &fingerprint).await
}

/// Poll enrollment status by fingerprint (the worker derives its own).
#[utoipa::path(get, path = "/worker/register/{fingerprint}/status", responses((status = 200, body = RegisterStatus)))]
#[get("/worker/register/<fingerprint>/status")]
pub async fn register_status(
    db: &State<DatabaseConnection>,
    ca: &State<Ca>,
    fingerprint: &str,
) -> Result<Json<RegisterStatus>, ApiError> {
    register_status_for(db.inner(), ca, fingerprint).await
}

async fn register_status_for(
    db: &DatabaseConnection,
    ca: &Ca,
    fingerprint: &str,
) -> Result<Json<RegisterStatus>, ApiError> {
    let worker = worker_store::find_worker_by_fingerprint(db, fingerprint)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?
        .ok_or_else(|| err(Status::NotFound, "worker not registered"))?;

    // Only release the certificate + CA once the worker is approved.
    let (signed_cert, ca_cert) = if worker.status == ApprovalStatus::Approved {
        (
            worker.signed_cert.clone(),
            Some(ca.ca_cert_pem().to_string()),
        )
    } else {
        (None, None)
    };

    Ok(Json(RegisterStatus {
        status: worker.status,
        signed_cert,
        ca_cert,
        repo_template: repo_template(),
    }))
}

/// Fetch the CA certificate so a worker can pin the server's identity.
#[utoipa::path(get, path = "/worker/ca", responses((status = 200, description = "PEM CA certificate")))]
#[get("/worker/ca")]
pub fn get_ca(ca: &State<Ca>) -> String {
    ca.ca_cert_pem().to_string()
}

/// Return the CA certificate's SHA-256 fingerprint so a worker can pin the
/// server before trusting any issued certificate (anti-MITM during enrollment).
#[utoipa::path(get, path = "/worker/ca/fingerprint", responses((status = 200, description = "CA fingerprint")))]
#[get("/worker/ca/fingerprint")]
pub fn get_ca_fingerprint(ca: &State<Ca>) -> Result<String, ApiError> {
    ca.ca_cert_fingerprint()
        .map_err(|e| err(Status::InternalServerError, e))
}

// ----------------------------------------------------------------------------
// Job lifecycle (approved worker certificate required)
// ----------------------------------------------------------------------------

/// Claim the next buildable job for this worker, or 204 if none.
///
/// The [`ClaimRequest`] body is accepted for wire compatibility but its contents
/// are **not** used: routing reads arches, package affinity and priority from
/// the worker's stored row. Each worker's decision depends on what every *other*
/// worker declared, so those values must come from one consistent source rather
/// than from whatever the caller asserts about itself. The row is refreshed on
/// every re-registration, which happens on each worker boot.
#[post("/worker/jobs/claim", data = "<_claim>")]
pub async fn claim_job(
    db: &State<DatabaseConnection>,
    store: &State<Arc<SnapshotStore>>,
    auth: WorkerAuth,
    _claim: Json<ClaimRequest>,
) -> Result<Option<Json<JobDescriptor>>, ApiError> {
    let db = db.inner();

    let Some(build) = worker_jobs::claim_job(
        db,
        auth.worker.id,
        lease_ttl_secs(),
        spill_delay_secs(),
        liveness_timeout_secs(),
    )
    .await
    .map_err(|e| err(Status::InternalServerError, e))?
    else {
        return Ok(None);
    };

    let descriptor = build_descriptor(db, store, &build)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;
    Ok(Some(Json(descriptor)))
}

async fn build_descriptor(
    db: &DatabaseConnection,
    store: &SnapshotStore,
    build: &aurcache_db::builds::Model,
) -> anyhow::Result<JobDescriptor> {
    let pkg = Packages::find_by_id(build.pkg_id)
        .one(db)
        .await?
        .ok_or_else(|| anyhow::anyhow!("package {} not found", build.pkg_id))?;

    let (makepkg_conf, pacman_conf) =
        // No `[repo]` section here: the worker appends one rendered from the
        // template it received at registration, using the host it actually
        // reaches this server on.
        build_job_config(db, pkg.id, Path::new(WORKER_PKGDEST)).await;

    let arch = build.platform.as_str().to_string();
    let mirrorlist = mirrorlist_for(&arch, &mirrorlist_dir()).await;

    // Best-effort PGP keys from the parsed .SRCINFO; the worker can still
    // self-extract if parsing failed.
    let pgp_keys = match store
        .sourceinfo(&pkg.source_data, pkg.patch.as_deref())
        .await
    {
        Ok(si) => si
            .base
            .pgp_fingerprints
            .iter()
            .map(ToString::to_string)
            .collect(),
        Err(_) => Vec::new(),
    };

    let build_flags = pkg
        .build_flags
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .collect();

    Ok(JobDescriptor {
        build_id: build.id,
        pkgbase: pkg.name,
        arch,
        build_flags,
        makepkg_conf,
        pacman_conf,
        mirrorlist,
        pgp_keys,
    })
}

/// Stream the (server-patched) source archive for a claimed job.
#[get("/worker/jobs/<build_id>/source")]
pub async fn job_source(
    db: &State<DatabaseConnection>,
    store: &State<Arc<SnapshotStore>>,
    auth: WorkerAuth,
    build_id: i32,
) -> Result<Vec<u8>, ApiError> {
    let db = db.inner();
    worker_complete::assert_owned_active(db, auth.worker.id, build_id)
        .await
        .map_err(|e| err(Status::Forbidden, e))?;

    let build = Builds::find_by_id(build_id)
        .one(db)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?
        .ok_or_else(|| err(Status::NotFound, "build not found"))?;
    let pkg = Packages::find_by_id(build.pkg_id)
        .one(db)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?
        .ok_or_else(|| err(Status::NotFound, "package not found"))?;

    store
        .archive_bytes(&pkg.source_data, pkg.patch.as_deref())
        .await
        .map_err(|e| err(Status::InternalServerError, e))
}

/// Append build log output.
#[post("/worker/jobs/<build_id>/logs", data = "<data>")]
pub async fn job_logs(
    db: &State<DatabaseConnection>,
    auth: WorkerAuth,
    build_id: i32,
    data: Data<'_>,
) -> Result<(), ApiError> {
    let db = db.inner();
    worker_complete::assert_owned_active(db, auth.worker.id, build_id)
        .await
        .map_err(|e| err(Status::Forbidden, e))?;

    let text = data
        .open(4.mebibytes())
        .into_string()
        .await
        .map_err(|e| err(Status::BadRequest, e))?;

    // The worker posts whole chunks, so this needs no buffering task of its own.
    append_build_output(db, build_id, &text)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;
    Ok(())
}

/// Upload one built artifact into the job's staging area.
#[post("/worker/jobs/<build_id>/artifacts/<filename>", data = "<data>")]
pub async fn job_artifact(
    db: &State<DatabaseConnection>,
    auth: WorkerAuth,
    build_id: i32,
    filename: &str,
    data: Data<'_>,
) -> Result<(), ApiError> {
    let db = db.inner();
    worker_complete::assert_owned_active(db, auth.worker.id, build_id)
        .await
        .map_err(|e| err(Status::Forbidden, e))?;

    // Reject path traversal; only a bare filename is allowed.
    let name = Path::new(filename)
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| *s == filename)
        .ok_or_else(|| err(Status::BadRequest, "invalid filename"))?;

    let dir = staging_dir(build_id);
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;

    let bytes = data
        .open(2.gibibytes())
        .into_bytes()
        .await
        .map_err(|e| err(Status::BadRequest, e))?;
    if !bytes.is_complete() {
        return Err(err(Status::PayloadTooLarge, "artifact exceeds size limit"));
    }
    tokio::fs::write(dir.join(name), bytes.value)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;
    Ok(())
}

/// Report a build as complete. Success ingests the staged artifacts into the
/// repo and promotes dependents; failure is terminal.
#[post("/worker/jobs/<build_id>/complete", data = "<input>")]
pub async fn complete_job(
    db: &State<DatabaseConnection>,
    auth: WorkerAuth,
    build_id: i32,
    input: Json<CompleteReport>,
) -> Result<(), ApiError> {
    let db = db.inner();
    let report = input.into_inner();

    let build = worker_complete::assert_owned_active(db, auth.worker.id, build_id)
        .await
        .map_err(|e| err(Status::Forbidden, e))?;

    let dir = staging_dir(build_id);
    if report.success {
        let files = read_staging(&dir)
            .await
            .map_err(|e| err(Status::InternalServerError, e))?;
        if files.is_empty() {
            return Err(err(Status::BadRequest, "no artifacts uploaded"));
        }

        // Sanity-check filenames against the package names we expect. Blocks
        // wrong-named uploads (not malicious contents — see design non-goals).
        let pkg = Packages::find_by_id(build.pkg_id)
            .one(db)
            .await
            .map_err(|e| err(Status::InternalServerError, e))?
            .ok_or_else(|| err(Status::NotFound, "package not found"))?;
        let expected = expected_pkgnames(&pkg);

        // Drop split debug packages before validating: they are not declared
        // pkgnames, so they would fail validation and requeue the build forever.
        // The server already sets OPTIONS=(!debug), so this only triggers for a
        // worker whose user makepkg.conf re-enables it.
        let logger = BuildLogger::new(build_id, db.clone());
        let (files, debug_files): (Vec<_>, Vec<_>) = files
            .into_iter()
            .partition(|(name, _)| !is_debug_artifact(&expected, name));
        for (name, _) in &debug_files {
            logger
                .append(format!("skipping debug package (not published): {name}\n"))
                .await;
        }
        if files.is_empty() {
            return Err(err(
                Status::BadRequest,
                "no publishable artifacts uploaded (only debug packages)",
            ));
        }

        let names: Vec<String> = files.iter().map(|(n, _)| n.clone()).collect();
        if let Err(e) = validate_artifact_names(&expected, &names) {
            logger.append(format!("rejected artifacts: {e}\n")).await;
            return Err(err(Status::BadRequest, e));
        }

        // Publishing is guarded by the lease itself: `assert_owned_active` above
        // is a stale read by the time the (slow) ingest runs, so the ingest
        // re-checks ownership under a row lock before committing anything.
        let ingested = ingest_pkgs(
            db,
            &logger,
            build.pkg_id,
            &build.platform,
            files,
            Some(LeaseGuard {
                build_id,
                worker_id: auth.worker.id,
            }),
        )
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;
        worker_complete::record_built_version(
            db,
            build_id,
            auth.worker.id,
            &ingested.version,
            ingested.total_size,
        )
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;
        worker_complete::complete_success(db, build_id, auth.worker.id)
            .await
            .map_err(|e| err(Status::InternalServerError, e))?;
    } else {
        if let Some(reason) = &report.reason
            && let Err(e) = append_build_output(
                db,
                build_id,
                &format!("worker reported failure: {reason}\n"),
            )
            .await
        {
            tracing::warn!("Failed to record failure reason for build {build_id}: {e}");
        }
        worker_complete::complete_failure(db, build_id, auth.worker.id)
            .await
            .map_err(|e| err(Status::InternalServerError, e))?;
    }
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

/// The package names the server expects a build to produce: the pkgbase plus any
/// split-package names recorded on the package row.
fn expected_pkgnames(pkg: &aurcache_db::packages::Model) -> Vec<String> {
    let mut names = vec![pkg.name.clone()];
    if let Some(json) = pkg.split_packages.as_deref()
        && let Ok(split) = serde_json::from_str::<Vec<String>>(json)
    {
        for name in split {
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }
    names
}

async fn read_staging(dir: &Path) -> anyhow::Result<Vec<(String, Vec<u8>)>> {
    let mut files = Vec::new();
    if !dir.exists() {
        return Ok(files);
    }
    let mut rd = tokio::fs::read_dir(dir).await?;
    while let Some(entry) = rd.next_entry().await? {
        if entry.file_type().await?.is_file() {
            let name = entry.file_name().to_string_lossy().to_string();
            let bytes = tokio::fs::read(entry.path()).await?;
            files.push((name, bytes));
        }
    }
    Ok(files)
}

/// Liveness heartbeat: renews leases for reported builds and requeues any this
/// worker silently dropped.
#[post("/worker/heartbeat", data = "<input>")]
pub async fn heartbeat(
    db: &State<DatabaseConnection>,
    auth: WorkerAuth,
    input: Json<Heartbeat>,
) -> Result<(), ApiError> {
    let db = db.inner();
    let hb = input.into_inner();
    worker_store::touch_last_seen(db, auth.worker.id, Some(&hb.version))
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;
    worker_jobs::heartbeat(
        db,
        auth.worker.id,
        &hb.active_build_ids,
        lease_ttl_secs(),
        max_attempts(),
    )
    .await
    .map_err(|e| err(Status::InternalServerError, e))?;
    Ok(())
}

/// Poll whether a cancel was requested for a specific job.
#[get("/worker/jobs/<build_id>/status")]
pub async fn job_status(
    db: &State<DatabaseConnection>,
    auth: WorkerAuth,
    build_id: i32,
) -> Result<Json<JobStatus>, ApiError> {
    let db = db.inner();
    let build = Builds::find_by_id(build_id)
        .one(db)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?
        .ok_or_else(|| err(Status::NotFound, "build not found"))?;
    if build.worker_id != Some(auth.worker.id) {
        return Err(err(Status::Forbidden, "not owner"));
    }
    // Cancel is signalled by moving the build out of ACTIVE while owned.
    let cancel_requested = build.status != Some(worker_jobs::STATUS_ACTIVE);
    Ok(Json(JobStatus { cancel_requested }))
}

// ----------------------------------------------------------------------------
// Admin management (operator auth)
// ----------------------------------------------------------------------------

/// The comma-separated lists the database stores, as actual lists.
///
/// Empty entries are dropped: an empty column splits to `[""]`, which would
/// render as a blank chip.
fn split_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(ToString::to_string)
        .collect()
}

/// How many builds a worker is running, and how its finished ones went.
#[derive(Default, Clone, Copy)]
struct BuildTally {
    active: i32,
    successful: i32,
    failed: i32,
}

/// One row per (worker, status) from the builds table.
#[derive(sea_orm::FromQueryResult)]
struct StatusCount {
    worker_id: Option<i32>,
    status: Option<i32>,
    count: i64,
}

/// Tally every worker's builds in one grouped query.
///
/// One query rather than three per worker: the workers page is a list, and a
/// count per row per outcome is how a list page starts issuing dozens of
/// queries to render.
async fn build_tallies(
    db: &DatabaseConnection,
) -> Result<HashMap<i32, BuildTally>, sea_orm::DbErr> {
    let rows = Builds::find()
        .select_only()
        .column(builds::Column::WorkerId)
        .column(builds::Column::Status)
        .column_as(builds::Column::Id.count(), "count")
        .filter(builds::Column::WorkerId.is_not_null())
        .group_by(builds::Column::WorkerId)
        .group_by(builds::Column::Status)
        .into_model::<StatusCount>()
        .all(db)
        .await?;

    let mut tallies: HashMap<i32, BuildTally> = HashMap::new();
    for row in rows {
        let (Some(worker_id), Some(status)) = (row.worker_id, row.status) else {
            continue;
        };
        // Saturating because these are display counts: a repository with more
        // than two billion builds of one worker should render a big number,
        // not panic the list.
        let count = i32::try_from(row.count).unwrap_or(i32::MAX);
        let tally = tallies.entry(worker_id).or_default();
        match status {
            BuildStates::ACTIVE_BUILD => tally.active = count,
            BuildStates::SUCCESSFUL_BUILD => tally.successful = count,
            BuildStates::FAILED_BUILD => tally.failed = count,
            // Enqueued and waiting-for-deps belong to no worker yet.
            _ => {}
        }
    }
    Ok(tallies)
}

/// A worker row as the operator's view of it.
///
/// Not the row itself: that carries `signed_cert`, and there is no reason to
/// hand a browser the certificate issued to a build machine.
fn summarise(worker: workers::Model, tally: BuildTally, now: i64, timeout: i64) -> WorkerSummary {
    WorkerSummary {
        id: worker.id,
        name: worker.name,
        status: worker.status,
        cert_fingerprint: worker.cert_fingerprint,
        native_arches: split_list(&worker.native_arches),
        emulated_arches: split_list(&worker.emulated_arches),
        package_affinity: split_list(&worker.package_affinity),
        priority: worker.priority,
        last_seen: worker.last_seen,
        version: worker.version,
        online: is_online(worker.last_seen, now, timeout),
        active_builds: tally.active,
        successful_builds: tally.successful,
        failed_builds: tally.failed,
    }
}

/// Whether a worker has checked in recently enough to count as connected.
///
/// A worker that has never checked in is not online, which is a different
/// statement from one that checked in and stopped -- `last_seen` tells those
/// apart and this deliberately does not.
fn is_online(last_seen: Option<i64>, now: i64, timeout: i64) -> bool {
    last_seen.is_some_and(|seen| now - seen <= timeout)
}

#[utoipa::path(get, path = "/workers", responses((status = 200, body = [WorkerSummary])))]
#[get("/workers")]
pub async fn list_workers(
    db: &State<DatabaseConnection>,
    _a: Authenticated,
) -> Result<Json<Vec<WorkerSummary>>, ApiError> {
    let db = db.inner();
    let workers = worker_store::list_workers(db)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;
    let tallies = build_tallies(db)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;
    let now = now_secs();
    let timeout = liveness_timeout_secs();
    Ok(Json(
        workers
            .into_iter()
            .map(|worker| {
                let tally = tallies.get(&worker.id).copied().unwrap_or_default();
                summarise(worker, tally, now, timeout)
            })
            .collect(),
    ))
}

#[utoipa::path(post, path = "/workers/{id}/approve", responses((status = 200)))]
#[post("/workers/<id>/approve")]
pub async fn approve_worker(
    db: &State<DatabaseConnection>,
    _a: Authenticated,
    id: i32,
) -> Result<(), ApiError> {
    let db = db.inner();
    worker_store::approve_worker(db, id)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;
    Ok(())
}

#[utoipa::path(post, path = "/workers/{id}/revoke", responses((status = 200)))]
#[post("/workers/<id>/revoke")]
pub async fn revoke_worker(
    db: &State<DatabaseConnection>,
    _a: Authenticated,
    id: i32,
) -> Result<(), ApiError> {
    let db = db.inner();
    worker_store::revoke_worker(db, id, max_attempts())
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;
    Ok(())
}

#[cfg(test)]
mod repo_template_tests {
    use super::render_repo_template;
    use aurcache_common::worker::REPO_HOST_PLACEHOLDER;

    /// Only the host is replaced: the scheme, port and path describe how the
    /// repository is published and remain the server's decision.
    #[test]
    fn replaces_only_the_host() {
        let t = render_repo_template("http://aurcache.example.com:9000/repo");
        assert!(t.contains(&format!(
            "Server = http://{REPO_HOST_PLACEHOLDER}:9000/repo/$arch"
        )));
        assert!(t.starts_with("[repo]\nSigLevel = Never\n"));
    }

    /// A URL without an explicit port still needs one, or the worker would
    /// render `http://host/$arch` and reach the web UI instead of the repo.
    #[test]
    fn supplies_the_repository_port_when_the_url_omits_it() {
        let t = render_repo_template("http://aurcache.example.com");
        assert!(t.contains(&format!(
            "Server = http://{REPO_HOST_PLACEHOLDER}:{}/$arch",
            aurcache_common::ports::AURCACHE_MIRROR_PORT
        )));
    }

    #[test]
    fn keeps_https_and_ignores_a_trailing_slash() {
        let t = render_repo_template("https://aurcache.example.com:8081/");
        assert!(t.contains(&format!(
            "Server = https://{REPO_HOST_PLACEHOLDER}:8081/$arch"
        )));
    }
}
