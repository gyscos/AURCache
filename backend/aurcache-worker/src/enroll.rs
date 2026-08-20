//! Enrollment lifecycle: pin the server CA, submit a CSR, wait for approval,
//! persist the signed certificate, and hand back an authenticated client.

use anyhow::{Context, Result};
use aurcache_types::worker::{RegisterRequest, RegisterStatus, WorkerStatus};
use std::time::Duration;

use crate::client::{WorkerClient, fetch_and_pin_ca};
use crate::config::Config;
use crate::identity::Identity;

/// Drop the CSR into the shared enrollment volume so a co-located backend can
/// auto-approve this worker (bundled single-host topology). Best-effort.
pub fn publish_csr_to_enrollment_dir(cfg: &Config, fingerprint: &str, csr_pem: &str) {
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

/// Ensure the worker is enrolled and return an authenticated mTLS client.
///
/// If a signed certificate is already persisted it is reused; otherwise the
/// worker pins the CA, registers, and polls until approved.
pub async fn ensure_enrolled(cfg: &Config, identity: &Identity) -> Result<WorkerClient> {
    tracing::info!("Worker fingerprint: {}", identity.fingerprint);

    if identity.is_enrolled() {
        tracing::info!("Reusing persisted worker certificate");
        return WorkerClient::authenticated(
            &cfg.aurcache_url,
            &identity.ca_pem()?,
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
    let csr_pem = identity.generate_csr(&cfg.name)?;
    publish_csr_to_enrollment_dir(cfg, &identity.fingerprint, &csr_pem);

    let req = RegisterRequest {
        name: cfg.name.clone(),
        native_arches: cfg.native_arches.clone(),
        emulated_arches: cfg.emulated_arches.clone(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        csr_pem,
        enrollment_token: cfg.enrollment_token.clone(),
    };

    let mut status = enroll_client.register(&req).await.context("registering")?;

    // 3. Poll until approved.
    loop {
        if let Some(client) = try_finish_enrollment(cfg, identity, &status)? {
            tracing::info!("Worker approved and enrolled");
            return Ok(client);
        }
        anyhow::ensure!(
            status.status != WorkerStatus::REVOKED,
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

/// If the status carries a signed certificate, persist it and build the
/// authenticated client. Returns `None` while still pending.
fn try_finish_enrollment(
    cfg: &Config,
    identity: &Identity,
    status: &RegisterStatus,
) -> Result<Option<WorkerClient>> {
    let (Some(cert), Some(ca)) = (&status.signed_cert, &status.ca_cert) else {
        return Ok(None);
    };
    identity.store_signed(cert, ca)?;
    let client = WorkerClient::authenticated(&cfg.aurcache_url, ca, cert, &identity.key_pem())?;
    Ok(Some(client))
}
