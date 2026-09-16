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
use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_activitylog::failure_activity::WorkerSettingRejectedActivity;
use aurcache_activitylog::worker_activity::{
    WorkerApproveActivity, WorkerEnrollActivity, WorkerRevokeActivity,
};
use aurcache_ca::Ca;
use aurcache_common::api::worker::{
    ApprovalStatus, WorkerConfigView, WorkerJoinInfo, WorkerSummary,
};
use aurcache_common::builder::BuildStates;
use aurcache_common::settings::{ApplicationSettings, Setting};
use aurcache_common::worker::{
    ClaimRequest, CompleteReport, Heartbeat, HeartbeatResponse, JobDescriptor, JobStatus,
    MirrorlistPreference, RegisterRequest, RegisterStatus,
};
use aurcache_common::worker_config::{EffectiveConfig, SettingStatus};
use aurcache_db::activities::ActivityType;
use aurcache_db::helpers::time::now_secs;
use aurcache_db::helpers::{worker_jobs, worker_store};
use aurcache_db::prelude::{Builds, Packages};
use aurcache_db::{builds, workers};
use aurcache_utils::build_logger::append_build_output;
use aurcache_utils::job_config::{build_job_config, mirrorlist_for};
use aurcache_utils::publish::publish_build;
use aurcache_utils::repository::Repository;
use aurcache_utils::settings::general::SettingsTraits;
use aurcache_utils::snapshot::SnapshotStore;
use aurcache_utils::vcs_check::job_vcs_sources;
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
pub(crate) fn public_repo_url() -> String {
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
///
/// The host substitution is shared with the CLI, which answers the same
/// question for a `pacman.conf` — see [`aurcache_common::repo`].
fn render_repo_template(public_url: &str) -> String {
    let server = aurcache_common::repo::server_url_for_host(
        public_url,
        aurcache_common::worker::REPO_HOST_PLACEHOLDER,
    );
    format!("[repo]\nSigLevel = Never\nServer = {server}\n")
}

/// Directory the server reads per-arch mirrorlists from.
fn mirrorlist_dir() -> PathBuf {
    PathBuf::from(env::var("AURCACHE_MIRRORLIST_DIR").unwrap_or_else(|_| "./repo".to_string()))
}

/// Checksum identifying one mirrorlist's content.
///
/// Computed only here. A worker never digests anything -- it echoes back the
/// value it was sent -- so this can change algorithm without any worker
/// needing to agree, at the cost of one resend per worker.
fn mirrorlist_checksum(content: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(content.as_bytes()))
}

/// Decide what to put in a job descriptor for the mirrorlist.
///
/// Returns `(content, checksum, unchanged)`. The mirrorlist is deployment
/// configuration rather than build configuration, but it is delivered on the
/// claim because it *changes* -- `MIRROR_RANK_SCHEDULE` rewrites it on a
/// schedule -- and the claim is the only exchange that recurs. Revalidating a
/// checksum keeps that from meaning the same bytes with every job.
fn resolve_mirrorlist(
    held: &MirrorlistPreference,
    arch: &str,
    current: Option<String>,
) -> (Option<String>, Option<String>, bool) {
    // A worker with its own mirrorlist would discard anything sent, so nothing
    // is computed or sent for it.
    if matches!(held, MirrorlistPreference::Local) {
        return (None, None, false);
    }
    let Some(content) = current else {
        // No mirrorlist for this arch: the worker drops whatever it cached and
        // falls back to its image's own. `unchanged` stays false precisely so
        // it can tell this apart from "keep what you have".
        return (None, None, false);
    };
    let checksum = mirrorlist_checksum(&content);
    let MirrorlistPreference::Server { checksums } = held else {
        unreachable!("Local handled above");
    };
    if checksums.get(arch).map(String::as_str) == Some(checksum.as_str()) {
        (None, None, true)
    } else {
        (Some(content), Some(checksum), false)
    }
}

/// Conventional PKGDEST inside a worker's build environment. Workers may patch
/// this in their local `makepkg.conf`; it is only a default.
const WORKER_PKGDEST: &str = "/output";

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
    worker_config,
    worker_join_info,
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
    rocket::routes![
        list_workers,
        worker_config,
        worker_join_info,
        approve_worker,
        revoke_worker,
    ]
}

/// Default worker image, published alongside every release. Overridden per
/// deployment with `AURCACHE_WORKER_IMAGE` (a private registry, a pinned tag).
const DEFAULT_WORKER_IMAGE: &str = "ghcr.io/lukas-heiligenbrunner/aurcache-worker:latest";

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
    al: &State<ActivityLog>,
    input: Json<RegisterRequest>,
) -> Result<Json<RegisterStatus>, ApiError> {
    let db = db.inner();
    let input = input.into_inner();

    let fingerprint = aurcache_ca::fingerprint_from_csr_pem(&input.csr_pem)
        .map_err(|e| err(Status::BadRequest, e))?;

    // Asked before registering, because registering is an upsert and would
    // erase the difference. A worker re-registers on every startup, so only the
    // first time is an event: logging the rest would turn an ordinary restart
    // -- or a crash loop -- into a log nobody can read past.
    let known = worker_store::find_worker_by_fingerprint(db, &fingerprint)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?
        .is_some();

    // Stored as the JSON the worker sent, because the server renders a
    // declaration and never reasons about it: a setting whose kind this server
    // predates has to survive the trip to the page that shows it.
    let declaration = input.settings.as_ref().and_then(|settings| {
        serde_json::to_string(settings)
            .map_err(|e| {
                tracing::warn!(
                    "worker {} sent a declaration that could not be stored: {e}",
                    input.name
                );
            })
            .ok()
    });

    let worker = worker_store::register_worker(
        db,
        &worker_store::WorkerRegistration {
            name: &input.name,
            fingerprint: &fingerprint,
            native_arches: &input.native_arches.join(","),
            emulated_arches: &input.emulated_arches.join(","),
            version: &input.version,
            kind: &input.kind,
            package_affinity: &input.packages.join(","),
            priority: input.priority,
            // Clamped to at least 1: a worker reporting 0 would be treated as
            // permanently full and could never block a lower-priority worker,
            // silently defeating its own priority.
            concurrency: i32::try_from(input.concurrency.max(1)).unwrap_or(i32::MAX),
            settings_declaration: declaration.as_deref(),
        },
    )
    .await
    .map_err(|e| err(Status::InternalServerError, e))?;

    if !known {
        al.record(
            WorkerEnrollActivity {
                worker: worker.name.clone(),
            },
            ActivityType::WorkerEnroll,
            None,
        );
    }

    // Issue the leaf certificate, or re-issue one this CA did not sign.
    //
    // The second case is a CA that has been regenerated -- a deployment that
    // lost the directory holding it, most often. Every certificate issued by
    // the old CA is then unverifiable, and a worker presenting one fails mTLS
    // with `BadSignature` on every request. Caching the certificate for ever
    // meant re-registration handed the same dead certificate back, so the
    // worker could never recover on its own however many times it restarted.
    let needs_cert = match worker.signed_cert.as_deref() {
        None => true,
        Some(cert) => !ca.issued(cert),
    };
    if needs_cert {
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
        // No user: nobody clicked this. The log says the server did it.
        al.record(
            WorkerApproveActivity {
                worker: worker.name.clone(),
            },
            ActivityType::WorkerApprove,
            None,
        );
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
#[post("/worker/jobs/claim", data = "<claim>")]
pub async fn claim_job(
    db: &State<DatabaseConnection>,
    store: &State<Arc<SnapshotStore>>,
    auth: WorkerAuth,
    claim: Json<ClaimRequest>,
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

    let descriptor = build_descriptor(db, store, &build, &claim.mirrorlist)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;
    Ok(Some(Json(descriptor)))
}

async fn build_descriptor(
    db: &DatabaseConnection,
    store: &SnapshotStore,
    build: &aurcache_db::builds::Model,
    held_mirrorlist: &MirrorlistPreference,
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
    let (mirrorlist, mirrorlist_checksum, mirrorlist_unchanged) = resolve_mirrorlist(
        held_mirrorlist,
        &arch,
        mirrorlist_for(&arch, &mirrorlist_dir()).await,
    );

    // Best-effort, from the parsed .SRCINFO: the `validpgpkeys` the worker must
    // trust, and the `git+` sources whose commit it should report back. A
    // parse failure costs neither outright -- the worker can still self-extract
    // the keys, and an unreported source leaves the commit recorded at queue
    // time standing.
    let (pgp_keys, vcs_sources) = match store
        .sourceinfo(&pkg.source_data, pkg.patch.as_deref())
        .await
    {
        Ok(si) => (
            si.base
                .pgp_fingerprints
                .iter()
                .map(ToString::to_string)
                .collect(),
            job_vcs_sources(&si),
        ),
        Err(_) => (Vec::new(), Vec::new()),
    };

    let build_flags = pkg
        .build_flags
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .collect();

    // Resolved here rather than on the worker: settings are the server's,
    // with the package overriding the global default.
    let persistent_builddir =
        ApplicationSettings::get::<bool>(Setting::PersistentBuilddir, Some(build.pkg_id), db)
            .await
            .value;

    Ok(JobDescriptor {
        build_id: build.id,
        pkgbase: pkg.name,
        arch,
        build_flags,
        persistent_builddir,
        makepkg_conf,
        pacman_conf,
        mirrorlist,
        mirrorlist_checksum,
        mirrorlist_unchanged,
        pgp_keys,
        vcs_sources,
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

/// The pkgbase a build belongs to.
///
/// Worker routes are keyed by the internal build id while logs are stored under
/// `<pkgbase>/<number>`, so the name has to be looked up once per request.
async fn build_pkgbase(db: &DatabaseConnection, pkg_id: i32) -> Result<String, ApiError> {
    Packages::find_by_id(pkg_id)
        .one(db)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?
        .map(|p| p.name)
        .ok_or_else(|| err(Status::NotFound, "package not found"))
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
    let build = worker_complete::assert_owned_active(db, auth.worker.id, build_id)
        .await
        .map_err(|e| err(Status::Forbidden, e))?;

    let text = data
        .open(4.mebibytes())
        .into_string()
        .await
        .map_err(|e| err(Status::BadRequest, e))?;

    // Logs are stored under the build's public identity, and the worker
    // endpoints are the only ones keyed by the internal id. `build` comes from
    // the ownership check above, so this costs one lookup for the pkgbase.
    let pkgbase = build_pkgbase(db, build.pkg_id).await?;

    // The worker posts whole chunks, so this needs no buffering task of its own.
    append_build_output(&pkgbase, build.number, &text)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;
    Ok(())
}

/// The size a request says its body is, from `Content-Length`, when it says.
///
/// What lets an artifact over the limit be refused before it is read. Without
/// it the only way to find out is to read up to the limit: a 40 GiB package
/// arrived as 20 GiB copied to the server's disk, then deleted, then refused.
pub struct DeclaredLength(Option<u64>);

#[rocket::async_trait]
impl<'r> FromRequest<'r> for DeclaredLength {
    type Error = std::convert::Infallible;

    async fn from_request(req: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        Outcome::Success(Self(
            req.headers()
                .get_one("Content-Length")
                .and_then(|v| v.trim().parse().ok()),
        ))
    }
}

/// Stream one built artifact into the job's staging area.
///
/// The body is copied to disk in chunks rather than buffered in memory: a
/// package file can be multi-gigabyte, and `into_bytes` would hold it all in
/// RAM, which is what made the previous upload path stall and time out under
/// memory pressure.
#[post("/worker/jobs/<build_id>/artifacts/<filename>", data = "<data>")]
pub async fn job_artifact(
    db: &State<DatabaseConnection>,
    repo: &State<Arc<Repository>>,
    auth: WorkerAuth,
    build_id: i32,
    filename: &str,
    declared: DeclaredLength,
    data: Data<'_>,
) -> Result<(), ApiError> {
    let db = db.inner();
    let build = worker_complete::assert_owned_active(db, auth.worker.id, build_id)
        .await
        .map_err(|e| err(Status::Forbidden, e))?;

    // A backstop against a runaway or hostile PKGBUILD filling the server's
    // disk, not a statement about what a package should weigh -- which is why
    // it is a setting, and per package: `unreal-engine` is past 20 GiB, and
    // allowing it that should not allow everything else the same.
    let limit = ApplicationSettings::get::<aurcache_utils::settings::ByteSize>(
        Setting::MaxArtifactSize,
        Some(build.pkg_id),
        db,
    )
    .await;
    let (limit, source) = (limit.value.0, limit.source);
    let too_large = || {
        err(
            Status::PayloadTooLarge,
            format!(
                "{filename} is larger than this package's artifact limit of {} \
                 (max_artifact_size, from {source:?}); raise it for the package or globally",
                aurcache_common::units::format_size(limit)
            ),
        )
    };
    if declared.0.is_some_and(|len| len > limit) {
        return Err(too_large());
    }

    // Reject path traversal; only a bare filename is allowed.
    let name = Path::new(filename)
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| *s == filename)
        .ok_or_else(|| err(Status::BadRequest, "invalid filename"))?;

    let dir = repo.staging_dir(build_id);
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;

    let dest = dir.join(name);
    // `open` stops reading at the limit without saying so; `is_complete` is the
    // only thing that tells a whole artifact from one cut off there. `into_file`
    // also flushes, so the file is fully written by the time ingest reads it.
    let stored = data.open(limit.bytes()).into_file(&dest).await;
    let problem = match &stored {
        Err(e) => Some(err(Status::BadRequest, e)),
        Ok(file) if !file.is_complete() => Some(too_large()),
        Ok(file) if file.n.written == 0 => Some(err(Status::BadRequest, "empty artifact")),
        Ok(_) => None,
    };
    if let Some(problem) = problem {
        // Leave no partial file behind for the later ingest to trip over.
        drop(stored);
        let _ = tokio::fs::remove_file(&dest).await;
        return Err(problem);
    }
    Ok(())
}

/// Report a build as complete.
///
/// A success is accepted and answered at once: the artifacts are all uploaded,
/// so the worker's part is done, and the build moves to `PUBLISHING` while the
/// server puts it in the repository in the background (see
/// [`publish_build`]). Nothing that can go wrong from there is the worker's to
/// fix. A failure is terminal.
#[post("/worker/jobs/<build_id>/complete", data = "<input>")]
pub async fn complete_job(
    db: &State<DatabaseConnection>,
    repo: &State<Arc<Repository>>,
    al: &State<ActivityLog>,
    auth: WorkerAuth,
    build_id: i32,
    input: Json<CompleteReport>,
) -> Result<(), ApiError> {
    // The worker only ever sees the status code -- `error_for_status` keeps
    // nothing else -- and a refused completion is how a build that finished
    // becomes a build that failed. So the reason is logged here, on the side
    // that knows it. Refusing quietly is what made a stale `files` row take an
    // afternoon to find.
    let outcome = complete_job_inner(
        db.inner(),
        repo.inner(),
        al.inner(),
        &auth,
        build_id,
        input.into_inner(),
    )
    .await;
    if let Err(e) = &outcome {
        tracing::warn!(
            "rejected completion of build {build_id} from worker {}: {} -- {}",
            auth.worker.id,
            e.0,
            e.1
        );
    }
    outcome
}

async fn complete_job_inner(
    db: &DatabaseConnection,
    repo: &Arc<Repository>,
    activity: &ActivityLog,
    auth: &WorkerAuth,
    build_id: i32,
    report: CompleteReport,
) -> Result<(), ApiError> {
    let build = Builds::find_by_id(build_id)
        .one(db)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?
        .ok_or_else(|| err(Status::NotFound, "build not found"))?;

    // A report about a build this worker ran that has already moved on is a
    // repeat, and is answered 200 without changing anything: the worker's
    // earlier request got through and only its answer was lost.
    //
    // * A cancelled report acknowledges an abort the server carried out
    //   (operator Stop, or the reaper abandoning the build).
    // * A success repeats a completion that was accepted: the build is being
    //   published, was published, or failed to publish -- which fails with no
    //   end reason, unlike a cancel or an abandonment.
    //
    // A real ownership check: without it, any authenticated worker that
    // guessed an id could remove another worker's staging directory.
    if build.worker_id == Some(auth.worker.id) {
        let failed = build.status == Some(worker_jobs::STATUS_FAILED);
        let acknowledged_abort = report.canceled && !report.success && failed;
        let repeated_success = report.success
            && (matches!(
                build.status,
                Some(BuildStates::PUBLISHING | BuildStates::SUCCESSFUL_BUILD)
            ) || (failed && build.end_reason.is_none()));
        if acknowledged_abort {
            let _ = tokio::fs::remove_dir_all(repo.staging_dir(build_id)).await;
            return Ok(());
        }
        if repeated_success {
            return Ok(());
        }
    }

    let build = worker_complete::assert_owned_active(db, auth.worker.id, build_id)
        .await
        .map_err(|e| err(Status::Forbidden, e))?;

    // Before the branch below: an OOM-killed build never reaches the success
    // path, and that is the build whose memory figure matters most.
    if let Some(peak) = report.peak_memory_bytes
        && let Err(e) = worker_complete::record_peak_memory(db, build_id, peak).await
    {
        tracing::warn!("Failed to record peak memory for build {build_id}: {e}");
    }

    if report.success {
        // What the worker actually checked out replaces what the server
        // guessed when it queued the build. Only from a success, and only when
        // the worker reported something: one that predates this, or could not
        // resolve a source, leaves the queue-time record standing -- the older
        // commit, so the error is a redundant rebuild rather than a missed one.
        if !report.vcs_commits.is_empty()
            && let Err(e) = aurcache_db::helpers::builds::record_build_vcs_sources(
                db,
                build_id,
                &report.vcs_commits,
            )
            .await
        {
            tracing::warn!("Failed to record built VCS sources for build {build_id}: {e}");
        }
        worker_complete::accept_for_publishing(db, build_id, auth.worker.id)
            .await
            .map_err(|e| err(Status::Forbidden, e))?;
        let (db, repo, activity) = (db.clone(), Arc::clone(repo), activity.clone());
        tokio::spawn(async move { publish_build(&db, &repo, &activity, build_id).await });
        return Ok(());
    }

    if let Some(reason) = &report.reason
        && let Ok(pkgbase) = build_pkgbase(db, build.pkg_id).await
        && let Err(e) = append_build_output(
            &pkgbase,
            build.number,
            &format!("worker reported failure: {reason}\n"),
        )
        .await
    {
        tracing::warn!("Failed to record failure reason for build {build_id}: {e}");
    }
    worker_complete::complete_failure(db, build_id, auth.worker.id)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;
    let _ = tokio::fs::remove_dir_all(repo.staging_dir(build_id)).await;
    Ok(())
}

/// Liveness heartbeat: renews leases for reported builds, requeues any this
/// worker silently dropped, and returns the builds the server wants it to stop
/// (abandoned, or cancelled by an operator). The abort list rides this same
/// answer rather than a second poll, so a cancelled or lost build is usually
/// stopped on the next 5 s tick.
#[post("/worker/heartbeat", data = "<input>")]
pub async fn heartbeat(
    db: &State<DatabaseConnection>,
    auth: WorkerAuth,
    al: &State<ActivityLog>,
    input: Json<Heartbeat>,
) -> Result<Json<HeartbeatResponse>, ApiError> {
    let db = db.inner();
    let hb = input.into_inner();
    worker_store::touch_last_seen(db, auth.worker.id, Some(&hb.version))
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;
    // Sent only when it changed, so this is normally absent. A report that
    // cannot be stored must not cost the worker its heartbeat -- the leases
    // this renews are what keep its builds alive.
    if let Some(effective) = &hb.effective {
        match serde_json::to_string(effective) {
            Ok(json) => {
                if let Err(e) =
                    worker_store::store_effective_config(db, auth.worker.id, &json).await
                {
                    tracing::warn!(
                        "could not store worker {}'s configuration report: {e}",
                        auth.worker.id
                    );
                }
            }
            Err(e) => tracing::warn!("unreadable configuration report from a worker: {e}"),
        }

        // A worker sends this only when it changes, so this is once per worker
        // process rather than once per heartbeat: the log records a machine
        // coming up running something other than what it was configured with.
        let refused: Vec<String> = effective
            .settings
            .iter()
            .filter(|(_, setting)| setting.status == SettingStatus::Rejected)
            .map(|(key, _)| key.clone())
            .collect();
        if !refused.is_empty() {
            al.record(
                WorkerSettingRejectedActivity {
                    worker: auth.worker.name.clone(),
                    settings: refused,
                },
                ActivityType::WorkerSettingRejected,
                None,
            );
        }
    }
    let outcome = worker_jobs::heartbeat(
        db,
        auth.worker.id,
        &hb.active_build_ids,
        lease_ttl_secs(),
        max_attempts(),
    )
    .await
    .map_err(|e| err(Status::InternalServerError, e))?;
    Ok(Json(HeartbeatResponse {
        cancel: outcome.cancel_requested,
    }))
}

/// Poll whether a cancel was requested for a specific job.
///
/// A build the worker still owns is cancelled when the server moved it out of
/// `ACTIVE` (an operator Stop, or the reaper abandoning it). A build that left
/// this worker's hands entirely — retried, reclaimed by someone else, or
/// already terminal — reads as cancelled too: the worker must stop touching it.
/// Only an `ACTIVE` row owned by *another* worker is refused, because that is
/// the one case where answering would leak a job descriptor the reporter never
/// had. Without row reuse that refusal is unreachable in practice, and 404 does
/// the rest of the withholding.
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
    let mine = build.worker_id == Some(auth.worker.id);
    if build.status == Some(worker_jobs::STATUS_ACTIVE) && !mine {
        return Err(err(Status::Forbidden, "not owner"));
    }
    // Cancel is signalled by leaving ACTIVE while owned, or by no longer being
    // this worker's to run.
    let cancel_requested = build.status != Some(worker_jobs::STATUS_ACTIVE) || !mine;
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
        kind: worker.kind,
        online: is_online(worker.last_seen, now, timeout),
        active_builds: tally.active,
        successful_builds: tally.successful,
        failed_builds: tally.failed,
        settings_rejected: rejected_settings(worker.effective_config.as_deref(), worker.id),
    }
}

/// How many settings the worker could not use the configured value for.
///
/// A count rather than the report itself: this rides the workers list, which is
/// polled, and what the list needs is only whether there is something to look
/// at. `None` when the worker has reported nothing -- "not known" and "nothing
/// wrong" are different answers, and the page shows them differently.
fn rejected_settings(effective: Option<&str>, id: i32) -> Option<i32> {
    let report: EffectiveConfig = parse_stored(effective?, id, "configuration report")?;
    i32::try_from(
        report
            .settings
            .values()
            .filter(|setting| setting.status == SettingStatus::Rejected)
            .count(),
    )
    .ok()
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

/// The image and port a copy-and-run `docker run` needs, for the Workers page
/// to show one when the fleet is empty. The host is not here on purpose — the
/// browser knows the address the page was reached on and the server does not.
#[utoipa::path(get, path = "/workers/join-info", responses((status = 200, body = WorkerJoinInfo)))]
#[get("/workers/join-info")]
pub async fn worker_join_info(_a: Authenticated) -> Json<WorkerJoinInfo> {
    let image = env::var("AURCACHE_WORKER_IMAGE")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_WORKER_IMAGE.to_string());
    let worker_port = env::var("AURCACHE_WORKER_PORT")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(aurcache_common::ports::AURCACHE_WORKER_PORT);
    Json(WorkerJoinInfo { image, worker_port })
}

/// What one worker can be configured with, and what it is running.
///
/// Both halves are stored as the worker sent them and parsed here. A stored
/// blob that no longer parses is reported as absent rather than failing the
/// request: it means a worker newer than this server in a way the shared
/// vocabulary did not cover, and the rest of the page is still worth showing.
#[utoipa::path(get, path = "/workers/{id}/config", responses((status = 200, body = WorkerConfigView)))]
#[get("/workers/<id>/config")]
pub async fn worker_config(
    db: &State<DatabaseConnection>,
    _a: Authenticated,
    id: i32,
) -> Result<Json<WorkerConfigView>, ApiError> {
    let db = db.inner();
    let worker = worker_store::find_worker(db, id)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?
        .ok_or_else(|| err(Status::NotFound, "no such worker"))?;

    Ok(Json(WorkerConfigView {
        worker_id: worker.id,
        settings: worker
            .settings_declaration
            .as_deref()
            .and_then(|json| parse_stored(json, id, "declaration")),
        effective: worker
            .effective_config
            .as_deref()
            .and_then(|json| parse_stored(json, id, "configuration report")),
    }))
}

/// Read one of the stored worker-configuration blobs, saying so rather than
/// failing when it cannot be read.
fn parse_stored<T: serde::de::DeserializeOwned>(json: &str, id: i32, what: &str) -> Option<T> {
    serde_json::from_str(json)
        .map_err(|e| {
            tracing::warn!("worker {id}'s stored {what} could not be read: {e}");
        })
        .ok()
}

#[utoipa::path(post, path = "/workers/{id}/approve", responses((status = 200)))]
#[post("/workers/<id>/approve")]
pub async fn approve_worker(
    db: &State<DatabaseConnection>,
    a: Authenticated,
    al: &State<ActivityLog>,
    id: i32,
) -> Result<(), ApiError> {
    let db = db.inner();
    let worker = worker_store::approve_worker(db, id)
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;
    al.record(
        WorkerApproveActivity {
            worker: worker.name,
        },
        ActivityType::WorkerApprove,
        a.username,
    );
    Ok(())
}

#[utoipa::path(post, path = "/workers/{id}/revoke", responses((status = 200)))]
#[post("/workers/<id>/revoke")]
pub async fn revoke_worker(
    db: &State<DatabaseConnection>,
    a: Authenticated,
    al: &State<ActivityLog>,
    id: i32,
) -> Result<(), ApiError> {
    let db = db.inner();
    let worker = worker_store::revoke_worker(db, id, max_attempts())
        .await
        .map_err(|e| err(Status::InternalServerError, e))?;
    al.record(
        WorkerRevokeActivity {
            worker: worker.name,
        },
        ActivityType::WorkerRevoke,
        a.username,
    );
    Ok(())
}

#[cfg(test)]
mod repo_template_tests {
    use super::render_repo_template;
    use aurcache_common::worker::MirrorlistPreference;
    use std::collections::BTreeMap;

    fn holding(arch: &str, checksum: &str) -> MirrorlistPreference {
        let mut checksums = BTreeMap::new();
        checksums.insert(arch.to_string(), checksum.to_string());
        MirrorlistPreference::Server { checksums }
    }

    /// A worker that holds nothing is sent the content, with the checksum it
    /// should echo back next time. This is also what a worker predating the
    /// field does, since `MirrorlistPreference` defaults to holding nothing.
    #[test]
    fn a_worker_holding_nothing_is_sent_the_mirrorlist() {
        let (content, checksum, unchanged) = super::resolve_mirrorlist(
            &MirrorlistPreference::default(),
            "x86_64",
            Some("Server = http://mirror/\n".to_string()),
        );
        assert_eq!(content.as_deref(), Some("Server = http://mirror/\n"));
        assert!(checksum.is_some(), "a checksum must accompany the content");
        assert!(!unchanged);
    }

    /// The point of the whole exchange: matching checksum, no bytes resent.
    #[test]
    fn a_matching_checksum_withholds_the_content() {
        let list = "Server = http://mirror/\n".to_string();
        let (_, checksum, _) = super::resolve_mirrorlist(
            &MirrorlistPreference::default(),
            "x86_64",
            Some(list.clone()),
        );
        let (content, checksum2, unchanged) =
            super::resolve_mirrorlist(&holding("x86_64", &checksum.unwrap()), "x86_64", Some(list));
        assert!(content.is_none(), "content should not be resent");
        assert!(checksum2.is_none());
        assert!(unchanged, "the worker must be told to keep what it has");
    }

    /// A checksum held for one architecture says nothing about another, which
    /// is why the claim carries a map rather than a single value.
    #[test]
    fn a_checksum_for_another_arch_does_not_match() {
        let list = "Server = http://mirror/\n".to_string();
        let (_, checksum, _) = super::resolve_mirrorlist(
            &MirrorlistPreference::default(),
            "x86_64",
            Some(list.clone()),
        );
        let (content, _, unchanged) = super::resolve_mirrorlist(
            &holding("x86_64", &checksum.unwrap()),
            "aarch64",
            Some(list),
        );
        assert!(content.is_some(), "aarch64 has not been sent this list");
        assert!(!unchanged);
    }

    /// `unchanged == false` with no content is how "the server has none" is
    /// told apart from "keep what you have" -- the worker must drop a stale
    /// entry rather than reuse it.
    #[test]
    fn no_server_mirrorlist_is_not_reported_as_unchanged() {
        let (content, checksum, unchanged) =
            super::resolve_mirrorlist(&holding("x86_64", "whatever"), "x86_64", None);
        assert!(content.is_none());
        assert!(checksum.is_none());
        assert!(!unchanged);
    }

    /// A worker with its own mirrorlist is sent nothing at all.
    #[test]
    fn a_local_worker_is_sent_nothing() {
        let (content, checksum, unchanged) = super::resolve_mirrorlist(
            &MirrorlistPreference::Local,
            "x86_64",
            Some("Server = http://mirror/\n".to_string()),
        );
        assert!(content.is_none());
        assert!(checksum.is_none());
        assert!(!unchanged);
    }

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
