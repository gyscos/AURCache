//! Remote-worker HTTP protocol (`/api/worker/*`) plus admin management
//! endpoints (`/api/workers`).
//!
//! Workers authenticate with mutual TLS: their client certificate is signed by
//! AURCache's internal CA and mapped to a `workers` row by the SHA-256
//! fingerprint of its public key. Enrollment endpoints (`register`, `ca`,
//! `register status`) are reachable without a client certificate so a worker
//! can bootstrap; all job endpoints require an *approved* worker's certificate.

use crate::models::authenticated::Authenticated;
use aurcache_ca::Ca;
use aurcache_db::helpers::{worker_jobs, worker_store};
use aurcache_db::prelude::{Builds, Packages};
use aurcache_db::workers;
use aurcache_deps::AurClient;
use aurcache_types::worker::{
    ClaimRequest, CompleteReport, Heartbeat, JobDescriptor, JobStatus, RegisterRequest,
    RegisterStatus, WorkerStatus,
};
use aurcache_utils::job_config::{build_job_config, mirrorlist_for};
use aurcache_utils::repo_ingest::{ingest_pkgs, validate_artifact_names};
use aurcache_utils::snapshot::SnapshotStore;
use aurcache_utils::build_logger::BuildLogger;
use aurcache_utils::worker_complete;
use rocket::data::ToByteUnit;
use rocket::http::Status;
use rocket::mtls::Certificate;
use rocket::request::{FromRequest, Outcome, Request};
use rocket::response::status::Custom;
use rocket::serde::json::Json;
use rocket::{Data, State, get, post};
use sea_orm::{DatabaseConnection, EntityTrait};
use std::env;
use std::path::{Path, PathBuf};
use utoipa::OpenApi;

/// Lease/liveness tuning (env-overridable; see the design doc).
fn lease_ttl_secs() -> i64 {
    env_i64("LEASE_TTL", 60)
}
fn max_attempts() -> i32 {
    env_i64("MAX_ATTEMPTS", 3) as i32
}
fn worker_cert_validity_days() -> i64 {
    env_i64("WORKER_CERT_VALIDITY_DAYS", 365)
}
fn env_i64(key: &str, default: i64) -> i64 {
    env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Base URL workers should use for the public pacman repo (`[repo] Server`).
fn public_repo_url() -> String {
    env::var("AURCACHE_PUBLIC_URL").unwrap_or_else(|_| {
        format!("http://localhost:{}", aurcache_types::ports::AURCACHE_MIRROR_PORT)
    })
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

fn err(status: Status, e: impl std::fmt::Display) -> Custom<String> {
    Custom(status, e.to_string())
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
            Ok(Some(worker)) if worker.status == WorkerStatus::APPROVED => {
                let _ = worker_store::touch_last_seen(db, worker.id, None).await;
                Outcome::Success(WorkerAuth { worker })
            }
            Ok(_) => Outcome::Error((Status::Forbidden, "worker not approved".to_string())),
            Err(e) => Outcome::Error((Status::InternalServerError, e.to_string())),
        }
    }
}

#[derive(OpenApi)]
#[openapi(paths(register_worker, register_status, get_ca, list_workers, approve_worker, revoke_worker))]
pub struct WorkerApi;

/// All worker-protocol and admin worker-management routes (mounted under `/api`).
#[must_use]
pub fn worker_routes() -> Vec<rocket::Route> {
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
        list_workers,
        approve_worker,
        revoke_worker,
    ]
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
) -> Result<Json<RegisterStatus>, Custom<String>> {
    let db = db as &DatabaseConnection;
    let input = input.into_inner();

    let fingerprint = aurcache_ca::fingerprint_from_csr_pem(&input.csr_pem)
        .map_err(|e| err(Status::BadRequest, e))?;

    let worker = worker_store::register_worker(
        db,
        &input.name,
        &fingerprint,
        &input.native_arches.join(","),
        &input.emulated_arches.join(","),
        &input.version,
    )
    .await
    .map_err(|e| err(Status::InternalServerError, e))?;

    // Issue the leaf certificate once and cache it on the worker row.
    if worker.signed_cert.is_none() {
        let signed = ca
            .sign_worker_csr(&input.csr_pem, worker_cert_validity_days())
            .map_err(|e| err(Status::InternalServerError, e))?;
        worker_store::store_signed_cert(
            db,
            worker.id,
            &signed.cert_pem,
            &signed.serial_hex,
            signed.not_after,
        )
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;
    }

    // Non-interactive enrollment: auto-approve when a configured mode matches.
    if worker.status != WorkerStatus::APPROVED
        && crate::worker_enroll::auto_approve_from_env(&fingerprint, input.enrollment_token.as_deref())
    {
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
) -> Result<Json<RegisterStatus>, Custom<String>> {
    register_status_for(db as &DatabaseConnection, ca, fingerprint).await
}

async fn register_status_for(
    db: &DatabaseConnection,
    ca: &Ca,
    fingerprint: &str,
) -> Result<Json<RegisterStatus>, Custom<String>> {
    let worker = worker_store::find_worker_by_fingerprint(db, fingerprint)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?
        .ok_or_else(|| err(Status::NotFound, "worker not registered"))?;

    // Only release the certificate + CA once the worker is approved.
    let (signed_cert, ca_cert) = if worker.status == WorkerStatus::APPROVED {
        (worker.signed_cert.clone(), Some(ca.ca_cert_pem().to_string()))
    } else {
        (None, None)
    };

    Ok(Json(RegisterStatus {
        status: worker.status,
        signed_cert,
        ca_cert,
    }))
}

/// Fetch the CA certificate so a worker can pin the server's identity.
#[utoipa::path(get, path = "/worker/ca", responses((status = 200, description = "PEM CA certificate")))]
#[get("/worker/ca")]
pub async fn get_ca(ca: &State<Ca>) -> String {
    ca.ca_cert_pem().to_string()
}

/// Return the CA certificate's SHA-256 fingerprint so a worker can pin the
/// server before trusting any issued certificate (anti-MITM during enrollment).
#[utoipa::path(get, path = "/worker/ca/fingerprint", responses((status = 200, description = "CA fingerprint")))]
#[get("/worker/ca/fingerprint")]
pub async fn get_ca_fingerprint(ca: &State<Ca>) -> Result<String, Custom<String>> {
    ca.ca_cert_fingerprint()
        .map_err(|e| err(Status::InternalServerError, e))
}

// ----------------------------------------------------------------------------
// Job lifecycle (approved worker certificate required)
// ----------------------------------------------------------------------------

/// Claim the next buildable job for the worker's arches, or 204 if none.
#[post("/worker/jobs/claim", data = "<input>")]
pub async fn claim_job(
    db: &State<DatabaseConnection>,
    store: &State<SnapshotStore>,
    auth: WorkerAuth,
    input: Json<ClaimRequest>,
) -> Result<Option<Json<JobDescriptor>>, Custom<String>> {
    let db = db as &DatabaseConnection;
    let input = input.into_inner();

    let Some(build) = worker_jobs::claim_job(
        db,
        auth.worker.id,
        &input.native_arches,
        &input.emulated_arches,
        lease_ttl_secs(),
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
        build_job_config(db, pkg.id, Path::new(WORKER_PKGDEST), &public_repo_url()).await?;

    let arch = build.platform.as_str().to_string();
    let mirrorlist = mirrorlist_for(&arch, &mirrorlist_dir()).await;

    // Best-effort PGP keys from the parsed .SRCINFO; the worker can still
    // self-extract if parsing failed.
    let client = AurClient::new();
    let pgp_keys = match store
        .sourceinfo(&client, &pkg.source_data, pkg.patch.as_deref())
        .await
    {
        Ok(si) => si
            .base
            .pgp_fingerprints
            .iter()
            .map(std::string::ToString::to_string)
            .collect(),
        Err(_) => Vec::new(),
    };

    let build_flags = pkg
        .build_flags
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(std::string::ToString::to_string)
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
    store: &State<SnapshotStore>,
    auth: WorkerAuth,
    build_id: i32,
) -> Result<Vec<u8>, Custom<String>> {
    let db = db as &DatabaseConnection;
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

    let client = AurClient::new();
    store
        .archive_bytes(&client, &pkg.source_data, pkg.patch.as_deref())
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
) -> Result<(), Custom<String>> {
    let db = db as &DatabaseConnection;
    worker_complete::assert_owned_active(db, auth.worker.id, build_id)
        .await
        .map_err(|e| err(Status::Forbidden, e))?;

    let text = data
        .open(4.mebibytes())
        .into_string()
        .await
        .map_err(|e| err(Status::BadRequest, e))?;

    let logger = BuildLogger::new(build_id, db.clone());
    logger.append(text.to_string()).await;
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
) -> Result<(), Custom<String>> {
    let db = db as &DatabaseConnection;
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
) -> Result<(), Custom<String>> {
    let db = db as &DatabaseConnection;
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
        let names: Vec<String> = files.iter().map(|(n, _)| n.clone()).collect();
        if let Err(e) = validate_artifact_names(&expected, &names) {
            BuildLogger::new(build_id, db.clone())
                .append(format!("rejected artifacts: {e}\n"))
                .await;
            return Err(err(Status::BadRequest, e));
        }

        let logger = BuildLogger::new(build_id, db.clone());
        let version = ingest_pkgs(db, &logger, build.pkg_id, &build.platform, files)
            .await
            .map_err(|e| err(Status::InternalServerError, e))?;
        worker_complete::record_built_version(db, build_id, &version)
            .await
            .map_err(|e| err(Status::InternalServerError, e))?;
        worker_complete::complete_success(db, build_id)
            .await
            .map_err(|e| err(Status::InternalServerError, e))?;
    } else {
        if let Some(reason) = &report.reason {
            BuildLogger::new(build_id, db.clone())
                .append(format!("worker reported failure: {reason}\n"))
                .await;
        }
        worker_complete::complete_failure(db, build_id)
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
    if let Some(json) = &pkg.split_packages
        && let Ok(split) = serde_json::from_str::<Vec<String>>(json)
    {
        for s in split {
            if !names.contains(&s) {
                names.push(s);
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
) -> Result<(), Custom<String>> {
    let db = db as &DatabaseConnection;
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
) -> Result<Json<JobStatus>, Custom<String>> {
    let db = db as &DatabaseConnection;
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

#[utoipa::path(get, path = "/workers", responses((status = 200, body = [aurcache_db::workers::Model])))]
#[get("/workers")]
pub async fn list_workers(
    db: &State<DatabaseConnection>,
    _a: Authenticated,
) -> Result<Json<Vec<workers::Model>>, Custom<String>> {
    let db = db as &DatabaseConnection;
    let workers = worker_store::list_workers(db)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;
    Ok(Json(workers))
}

#[utoipa::path(post, path = "/workers/{id}/approve", responses((status = 200)))]
#[post("/workers/<id>/approve")]
pub async fn approve_worker(
    db: &State<DatabaseConnection>,
    _a: Authenticated,
    id: i32,
) -> Result<(), Custom<String>> {
    let db = db as &DatabaseConnection;
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
) -> Result<(), Custom<String>> {
    let db = db as &DatabaseConnection;
    worker_store::revoke_worker(db, id)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;
    Ok(())
}
