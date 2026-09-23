//! Enrollment lifecycle: pin the server CA, submit a CSR, wait for approval,
//! persist the signed certificate, and hand back an authenticated client.

use anyhow::{Context, Result};
use aurcache_common::api::worker::ApprovalStatus;
use aurcache_common::worker::{RegisterRequest, RegisterStatus};
use std::time::Duration;

use crate::client::{WorkerClient, fetch_and_pin_ca};
use crate::config::CoreConfig;
use crate::identity::Identity;

/// Drop the CSR into the shared enrollment volume so a co-located backend can
/// auto-approve this worker (bundled single-host topology). Best-effort.
pub fn publish_csr_to_enrollment_dir(cfg: &CoreConfig, fingerprint: &str, csr_pem: &str) {
    let Some(dir) = &cfg.enrollment_dir else {
        return;
    };
    if let Err(e) = std::fs::create_dir_all(dir) {
        tracing::debug!("enrollment dir {} unavailable: {e}", dir.display());
        return;
    }
    let path = dir.join(format!("{fingerprint}.csr"));
    match std::fs::write(&path, csr_pem) {
        Ok(()) => tracing::info!("Published CSR for auto-approval at {}", path.display()),
        Err(e) => tracing::debug!("could not write {}: {e}", path.display()),
    }
}

/// Assemble the registration payload from the current configuration.
///
/// Registration is how a worker reports its configuration, and *all* of it can
/// have changed since the machine last booted -- or since the server last
/// delivered values -- so this is rebuilt from `cfg` every time rather than
/// cached.
pub fn register_request(cfg: &CoreConfig, csr_pem: String, kind: &str) -> RegisterRequest {
    RegisterRequest {
        name: cfg.name.clone(),
        native_arches: cfg.native_arches.clone(),
        emulated_arches: cfg.emulated_arches.clone(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        kind: kind.to_string(),
        csr_pem,
        enrollment_token: cfg.enrollment_token.clone(),
        packages: cfg.packages.clone(),
        priority: cfg.priority,
        concurrency: cfg.concurrency as u32,
        settings: Some(cfg.settings.declare()),
    }
}

/// Ensure the worker is enrolled and return an authenticated mTLS client.
///
/// The worker registers on **every** startup, not only the first. Registration
/// carries its name, arches, concurrency, priority and package affinity — all of
/// which come from the environment and any of which may have been edited since
/// the last boot — so skipping it would let the server's picture of the fleet
/// drift from reality. Registration is idempotent: it is keyed on the stable
/// SPKI fingerprint of the persisted keypair, and it never alters approval
/// state, so a revoked worker that comes back stays revoked (it does refresh
/// `last_seen`, which is how the UI can show that the machine is asking again).
///
/// When a signed certificate is already persisted, re-registration is
/// **best-effort**: the bundled topology starts the worker and the backend from
/// one `docker compose up`, so a worker will regularly reach the server before
/// it is listening. Failing to register then must not stop a worker that is
/// already able to build — it proceeds on its persisted configuration.
pub async fn ensure_enrolled(
    cfg: &CoreConfig,
    identity: &Identity,
    kind: &str,
) -> Result<WorkerClient> {
    tracing::info!("Worker fingerprint: {}", identity.fingerprint);

    let csr_pem = identity.generate_csr(&cfg.name)?;

    if identity.is_enrolled() {
        // Reuse the CA we already pinned rather than re-running trust-on-first-use.
        let ca_pem = identity.ca_pem()?;
        match WorkerClient::enrollment(&cfg.aurcache_url, &ca_pem) {
            Ok(client) => match client
                .register(&register_request(cfg, csr_pem.clone(), kind))
                .await
            {
                Ok(status) => {
                    tracing::info!("Re-registered (status: {})", status.status);
                    // Revocation is terminal, like on the fresh path below: a
                    // revoked worker that carried on would run forever failing
                    // every claim, instead of exiting loudly for the operator.
                    anyhow::ensure!(
                        status.status != ApprovalStatus::Revoked,
                        "worker is revoked"
                    );
                    // Adopt a certificate the server has re-issued instead of
                    // discarding it. The server re-issues when the one it had
                    // on file was signed by a CA it no longer has, and a worker
                    // that ignored the replacement kept presenting a
                    // certificate nothing could verify.
                    if let Some(client) = try_finish_enrollment(cfg, identity, &status)? {
                        return Ok(client);
                    }
                }
                Err(e) => {
                    // A rotated CA looks exactly like this: the pinned CA
                    // cannot verify the server's new certificate, so the
                    // connection fails before any of the above can run.
                    if let Some(client) =
                        recover_from_rotated_ca(cfg, identity, &csr_pem, kind, &e).await?
                    {
                        return Ok(client);
                    }
                }
            },
            Err(e) => tracing::warn!("Could not build enrollment client: {e:#}"),
        }

        tracing::info!("Reusing persisted worker certificate");
        return WorkerClient::authenticated(
            &cfg.aurcache_url,
            &ca_pem,
            &identity.cert_pem()?,
            &identity.key_pem(),
        );
    }

    // 1. Pin the server CA (verified against a configured fingerprint or TOFU).
    let ca_pem = fetch_and_pin_ca(&cfg.aurcache_url, cfg.server_ca_fingerprint.as_deref())
        .await
        .context("pinning server CA")?;

    let enroll_client = WorkerClient::enrollment(&cfg.aurcache_url, &ca_pem)?;

    // 2. Submit our CSR and publish it for co-located auto-approval.
    publish_csr_to_enrollment_dir(cfg, &identity.fingerprint, &csr_pem);

    let mut status = enroll_client
        .register(&register_request(cfg, csr_pem, kind))
        .await
        .context("registering")?;

    // 3. Poll until approved.
    loop {
        if let Some(client) = try_finish_enrollment(cfg, identity, &status)? {
            tracing::info!("Worker approved and enrolled");
            return Ok(client);
        }
        anyhow::ensure!(
            status.status != ApprovalStatus::Revoked,
            "worker was revoked by the server"
        );
        tracing::info!("Awaiting approval (status: {})…", status.status);
        tokio::time::sleep(Duration::from_secs(cfg.poll_interval)).await;
        status = enroll_client
            .register_status(&identity.fingerprint)
            .await
            .context("polling enrollment status")?;
    }
}

/// Re-pin the server's CA and enrol again, when the pinned one has stopped
/// working because the server's CA was regenerated.
///
/// Only attempted when `AURCACHE_SERVER_CA_FINGERPRINT` is configured, and the
/// newly fetched CA is checked against it. That is the whole reason this is
/// safe to do automatically: re-pinning on a failed connection is otherwise
/// indistinguishable from accepting whatever an attacker offers, which is
/// precisely what pinning exists to prevent.
///
/// Without a configured fingerprint the situation is reported rather than
/// papered over, because only the operator can tell a legitimate rotation from
/// an interception.
///
/// Returns `None` when recovery was not attempted or did not apply, leaving the
/// caller to carry on with the persisted configuration -- a server that is
/// merely not listening yet must not cost a worker its enrolment.
async fn recover_from_rotated_ca(
    cfg: &CoreConfig,
    identity: &Identity,
    csr_pem: &str,
    kind: &str,
    cause: &anyhow::Error,
) -> Result<Option<WorkerClient>> {
    let Some(expected) = cfg.server_ca_fingerprint.as_deref() else {
        tracing::warn!(
            "Re-registration failed ({cause:#}). If the server's CA was regenerated \
             this worker cannot tell that apart from an interception, so it will keep \
             using the CA it pinned. Set AURCACHE_SERVER_CA_FINGERPRINT to let it \
             recover on its own, or remove the pinned CA to enrol afresh."
        );
        return Ok(None);
    };

    let Ok(fresh_ca) = fetch_and_pin_ca(&cfg.aurcache_url, Some(expected)).await else {
        // Unreachable, or offering a CA that fails the fingerprint check. Either
        // way this is not a rotation we can act on.
        tracing::warn!(
            "Re-registration failed, continuing with the persisted configuration: {cause:#}"
        );
        return Ok(None);
    };

    if identity.ca_pem().is_ok_and(|pinned| pinned == fresh_ca) {
        // Same CA as before, so the failure was something else -- the server
        // starting up, most likely.
        tracing::warn!(
            "Re-registration failed, continuing with the persisted configuration: {cause:#}"
        );
        return Ok(None);
    }

    tracing::warn!(
        "The server's CA has changed and matches the configured fingerprint; \
         re-enrolling with the new one"
    );
    let client = WorkerClient::enrollment(&cfg.aurcache_url, &fresh_ca)?;
    publish_csr_to_enrollment_dir(cfg, &identity.fingerprint, csr_pem);
    let status = client
        .register(&register_request(cfg, csr_pem.to_string(), kind))
        .await
        .context("re-registering after the server CA changed")?;
    try_finish_enrollment(cfg, identity, &status)
}

/// If the status carries a signed certificate, persist it and build the
/// authenticated client. Returns `None` while still pending.
fn try_finish_enrollment(
    cfg: &CoreConfig,
    identity: &Identity,
    status: &RegisterStatus,
) -> Result<Option<WorkerClient>> {
    let (Some(cert), Some(ca)) = (&status.signed_cert, &status.ca_cert) else {
        return Ok(None);
    };
    identity.store_signed(cert, ca)?;
    let mut client = WorkerClient::authenticated(&cfg.aurcache_url, ca, cert, &identity.key_pem())?;

    // Render the server's `[repo]` template with the host we actually reach it
    // on. Done here, once, because it describes the deployment rather than any
    // one build.
    let section = crate::repo::render(
        &status.repo_template,
        &cfg.aurcache_url,
        cfg.repo_host.as_deref(),
        cfg.repo_url.as_deref(),
    );
    match section.lines().find(|l| l.starts_with("Server =")) {
        Some(server) => tracing::info!("Package repository for this worker: {server}"),
        None => tracing::warn!(
            "No usable [repo] section for this worker; builds cannot resolve \
             AURCache-built dependencies"
        ),
    }
    client.set_repo_section(section);
    Ok(Some(client))
}
