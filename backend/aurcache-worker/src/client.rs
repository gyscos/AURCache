//! HTTPS/mTLS client for the worker protocol.
//!
//! Two construction paths:
//! * [`WorkerClient::enrollment`] — trusts the server via a pinned CA
//!   fingerprint (or TOFU on first contact) and carries no client certificate;
//!   used for `register` / status polling / CA fetch.
//! * [`WorkerClient::authenticated`] — presents the CA-signed client
//!   certificate (mTLS) and validates the server against the pinned CA; used
//!   for all job endpoints.

use anyhow::{Context, Result, bail};
use aurcache_types::worker::{
    ClaimRequest, CompleteReport, Heartbeat, JobDescriptor, JobStatus, RegisterRequest,
    RegisterStatus,
};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use reqwest::{Certificate, Client, Identity, StatusCode};
use std::time::Duration;

use crate::identity::spki_fingerprint;

/// How long to wait for a TCP+TLS connection to establish before giving up.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Inactivity timeout while reading a response body. A silently stalled
/// connection trips this well before the lease TTL (default 60s), so the
/// worker's lease self-abort watchdog is never defeated by a hung socket.
/// This is a per-read idle timeout, not a total-request deadline, so it does
/// not break legitimately slow large source downloads / artifact uploads.
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// A configured protocol client bound to a base URL.
pub struct WorkerClient {
    http: Client,
    base: String,
}

/// Fetch the server CA (PEM + fingerprint) using a deliberately
/// unauthenticated transport, verifying the fingerprint against `pin` when
/// provided (trust-on-first-use otherwise). Returns the CA PEM.
pub async fn fetch_and_pin_ca(base: &str, pin: Option<&str>) -> Result<String> {
    let insecure = Client::builder()
        .danger_accept_invalid_certs(true)
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        .build()
        .context("building bootstrap client")?;

    let ca_pem = insecure
        .get(format!("{base}/api/worker/ca"))
        .send()
        .await
        .context("fetching CA certificate")?
        .error_for_status()
        .context("CA endpoint returned error")?
        .text()
        .await
        .context("reading CA certificate")?;

    let actual = ca_fingerprint(&ca_pem)?;
    if let Some(expected) = pin {
        let expected = expected.to_lowercase().replace([':', ' '], "");
        if actual != expected {
            bail!(
                "server CA fingerprint mismatch: expected {expected}, got {actual} \
                 (possible man-in-the-middle)"
            );
        }
        tracing::info!("Verified server CA fingerprint {actual}");
    } else {
        tracing::warn!(
            "Trusting server CA on first use (fingerprint {actual}); \
             set AURCACHE_SERVER_CA_FINGERPRINT to pin it"
        );
    }
    Ok(ca_pem)
}

/// SHA-256 fingerprint of a PEM certificate's DER body.
fn ca_fingerprint(pem: &str) -> Result<String> {
    let der = pem_to_der(pem).context("decoding CA PEM")?;
    Ok(spki_fingerprint(&der))
}

/// Minimal PEM → DER decoder for a single CERTIFICATE block.
fn pem_to_der(pem: &str) -> Result<Vec<u8>> {
    let mut b64 = String::new();
    let mut in_block = false;
    for line in pem.lines() {
        let line = line.trim();
        if line.starts_with("-----BEGIN") {
            in_block = true;
        } else if line.starts_with("-----END") {
            break;
        } else if in_block {
            b64.push_str(line);
        }
    }
    if b64.is_empty() {
        bail!("no PEM block found");
    }
    base64_decode(&b64)
}

/// The CA fingerprint is computed over the whole certificate DER (matching the
/// server's `ca_cert_fingerprint`, which hashes the full DER — not the SPKI).
/// Note: [`spki_fingerprint`] simply hashes the bytes it is given, so passing
/// the full cert DER yields the certificate fingerprint.
fn base64_decode(s: &str) -> Result<Vec<u8>> {
    let cleaned: String = s.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    STANDARD
        .decode(cleaned.as_bytes())
        .context("invalid base64 in PEM body")
}

impl WorkerClient {
    /// Build an enrollment client that trusts only the pinned CA and carries no
    /// client identity (job endpoints will 401/403 until authenticated).
    pub fn enrollment(base: &str, ca_pem: &str) -> Result<Self> {
        let ca = Certificate::from_pem(ca_pem.as_bytes()).context("parsing CA cert")?;
        let http = Client::builder()
            .use_rustls_tls()
            .add_root_certificate(ca)
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .build()
            .context("building enrollment client")?;
        Ok(Self {
            http,
            base: base.to_string(),
        })
    }

    /// Build an authenticated mTLS client presenting the worker certificate.
    pub fn authenticated(base: &str, ca_pem: &str, cert_pem: &str, key_pem: &str) -> Result<Self> {
        let ca = Certificate::from_pem(ca_pem.as_bytes()).context("parsing CA cert")?;
        let mut identity_pem = cert_pem.as_bytes().to_vec();
        identity_pem.extend_from_slice(key_pem.as_bytes());
        let identity =
            Identity::from_pem(&identity_pem).context("building client identity from PEM")?;
        let http = Client::builder()
            .use_rustls_tls()
            .add_root_certificate(ca)
            .identity(identity)
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .build()
            .context("building authenticated client")?;
        Ok(Self {
            http,
            base: base.to_string(),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}/api/worker{path}", self.base)
    }

    /// Register (or re-register) this worker, returning the current status.
    pub async fn register(&self, req: &RegisterRequest) -> Result<RegisterStatus> {
        let resp = self
            .http
            .post(self.url("/register"))
            .json(req)
            .send()
            .await
            .context("register request")?
            .error_for_status()
            .context("register rejected")?;
        resp.json().await.context("decoding register status")
    }

    /// Poll enrollment status by fingerprint.
    pub async fn register_status(&self, fingerprint: &str) -> Result<RegisterStatus> {
        let resp = self
            .http
            .get(self.url(&format!("/register/{fingerprint}/status")))
            .send()
            .await
            .context("status request")?
            .error_for_status()
            .context("status rejected")?;
        resp.json().await.context("decoding register status")
    }

    /// Claim the next job for the given arches, or `None` if the queue is empty.
    pub async fn claim(&self, req: &ClaimRequest) -> Result<Option<JobDescriptor>> {
        let resp = self
            .http
            .post(self.url("/jobs/claim"))
            .json(req)
            .send()
            .await
            .context("claim request")?;
        match resp.status() {
            StatusCode::OK => Ok(Some(resp.json().await.context("decoding job")?)),
            StatusCode::NOT_FOUND | StatusCode::NO_CONTENT => Ok(None),
            other => {
                let body = resp.text().await.unwrap_or_default();
                bail!("claim failed ({other}): {body}");
            }
        }
    }

    /// Download the (server-patched) source tarball bytes for a build.
    pub async fn source(&self, build_id: i32) -> Result<Vec<u8>> {
        let resp = self
            .http
            .get(self.url(&format!("/jobs/{build_id}/source")))
            .send()
            .await
            .context("source request")?
            .error_for_status()
            .context("source rejected")?;
        Ok(resp.bytes().await.context("reading source")?.to_vec())
    }

    /// Append a chunk of build log output.
    pub async fn append_log(&self, build_id: i32, text: &str) -> Result<()> {
        self.http
            .post(self.url(&format!("/jobs/{build_id}/logs")))
            .body(text.to_string())
            .send()
            .await
            .context("log request")?
            .error_for_status()
            .context("log rejected")?;
        Ok(())
    }

    /// Upload a single artifact file.
    pub async fn upload_artifact(&self, build_id: i32, filename: &str, bytes: Vec<u8>) -> Result<()> {
        self.http
            .post(self.url(&format!("/jobs/{build_id}/artifacts/{filename}")))
            .body(bytes)
            .send()
            .await
            .context("artifact request")?
            .error_for_status()
            .context("artifact rejected")?;
        Ok(())
    }

    /// Report a build's terminal result.
    pub async fn complete(&self, build_id: i32, report: &CompleteReport) -> Result<()> {
        self.http
            .post(self.url(&format!("/jobs/{build_id}/complete")))
            .json(report)
            .send()
            .await
            .context("complete request")?
            .error_for_status()
            .context("complete rejected")?;
        Ok(())
    }

    /// Send a liveness heartbeat listing the builds still running.
    pub async fn heartbeat(&self, hb: &Heartbeat) -> Result<()> {
        self.http
            .post(self.url("/heartbeat"))
            .json(hb)
            .send()
            .await
            .context("heartbeat request")?
            .error_for_status()
            .context("heartbeat rejected")?;
        Ok(())
    }

    /// Poll whether a build has been asked to cancel.
    pub async fn job_status(&self, build_id: i32) -> Result<JobStatus> {
        let resp = self
            .http
            .get(self.url(&format!("/jobs/{build_id}/status")))
            .send()
            .await
            .context("job status request")?
            .error_for_status()
            .context("job status rejected")?;
        resp.json().await.context("decoding job status")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_roundtrip_known_vectors() {
        assert_eq!(base64_decode("").unwrap(), b"");
        assert_eq!(base64_decode("Zg==").unwrap(), b"f");
        assert_eq!(base64_decode("Zm8=").unwrap(), b"fo");
        assert_eq!(base64_decode("Zm9v").unwrap(), b"foo");
        assert_eq!(base64_decode("Zm9vYg==").unwrap(), b"foob");
        assert_eq!(base64_decode("Zm9vYmE=").unwrap(), b"fooba");
        assert_eq!(base64_decode("Zm9vYmFy").unwrap(), b"foobar");
    }

    #[test]
    fn pem_decodes_single_block() {
        // "foobar" base64 wrapped as a fake cert block.
        let pem = "-----BEGIN CERTIFICATE-----\nZm9vYmFy\n-----END CERTIFICATE-----\n";
        assert_eq!(pem_to_der(pem).unwrap(), b"foobar");
    }

    #[test]
    fn pem_without_block_errors() {
        assert!(pem_to_der("not a pem").is_err());
    }
}
