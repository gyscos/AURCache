//! AURCache internal certificate authority.
//!
//! On first startup AURCache generates a self-signed CA (persisted in the data
//! volume). It issues its own server certificate from that CA and signs worker
//! certificate signing requests (CSRs) when a worker is approved. Workers are
//! identified by the SHA-256 fingerprint of their public key (SubjectPublicKeyInfo),
//! which is stable across the CSR, the issued certificate, and the certificate
//! presented during the mTLS handshake.

use anyhow::{Context, anyhow};
use rcgen::{
    BasicConstraints, CertificateParams, CertificateSigningRequestParams, DnType, IsCa, KeyPair,
    KeyUsagePurpose,
};
use sha2::{Digest, Sha256};
use std::path::Path;
use time::{Duration, OffsetDateTime};

const CA_CERT_FILE: &str = "ca-cert.pem";
const CA_KEY_FILE: &str = "ca-key.pem";

/// A loaded (or freshly created) internal CA: its certificate and private key.
pub struct Ca {
    cert_pem: String,
    key_pem: String,
}

/// The result of signing a worker CSR.
pub struct SignedWorkerCert {
    /// PEM of the signed leaf certificate.
    pub cert_pem: String,
    /// Epoch seconds when the certificate expires.
    pub not_after: i64,
    /// SHA-256 fingerprint of the worker's public key.
    pub fingerprint: String,
}

impl Ca {
    /// Load the CA from `dir`, creating and persisting a new one if absent.
    ///
    /// Idempotent: a second call returns the same CA. Key material is written
    /// with `0600` permissions on Unix.
    pub fn load_or_create(dir: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir).context("creating CA directory")?;
        let cert_path = dir.join(CA_CERT_FILE);
        let key_path = dir.join(CA_KEY_FILE);

        if cert_path.exists() && key_path.exists() {
            let cert_pem = std::fs::read_to_string(&cert_path).context("reading CA cert")?;
            let key_pem = std::fs::read_to_string(&key_path).context("reading CA key")?;
            // Validate that the material parses before trusting it.
            KeyPair::from_pem(&key_pem).context("parsing persisted CA key")?;
            return Ok(Self { cert_pem, key_pem });
        }

        let (cert_pem, key_pem) = Self::generate_ca()?;
        write_secret(&key_path, &key_pem)?;
        std::fs::write(&cert_path, &cert_pem).context("writing CA cert")?;
        tracing::info!("Generated new AURCache internal CA at {}", dir.display());

        Ok(Self { cert_pem, key_pem })
    }

    fn generate_ca() -> anyhow::Result<(String, String)> {
        let key = KeyPair::generate().context("generating CA key")?;
        let mut params = CertificateParams::new(Vec::<String>::new())?;
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(DnType::CommonName, "AURCache Internal CA");
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        params.not_before = OffsetDateTime::now_utc() - Duration::hours(1);
        params.not_after = OffsetDateTime::now_utc() + Duration::days(3650);
        let cert = params.self_signed(&key).context("self-signing CA")?;
        Ok((cert.pem(), key.serialize_pem()))
    }

    /// PEM of the CA certificate (served to workers so they can pin it).
    #[must_use]
    pub fn ca_cert_pem(&self) -> &str {
        &self.cert_pem
    }

    /// SHA-256 fingerprint of the CA certificate (hex, lowercase).
    pub fn ca_cert_fingerprint(&self) -> anyhow::Result<String> {
        let der = pem_to_der(&self.cert_pem)?;
        Ok(sha256_hex(&der))
    }

    /// Reconstruct an issuer usable for signing from the persisted CA material.
    fn issuer(&self) -> anyhow::Result<(rcgen::Certificate, KeyPair)> {
        let ca_key = KeyPair::from_pem(&self.key_pem).context("parsing CA key")?;
        let ca_params = CertificateParams::from_ca_cert_pem(&self.cert_pem)
            .context("parsing CA cert for signing")?;
        let ca_cert = ca_params
            .self_signed(&ca_key)
            .context("reconstructing CA issuer")?;
        Ok((ca_cert, ca_key))
    }

    /// Issue a server certificate for the given subject alt names, signed by the CA.
    pub fn issue_server_cert(&self, sans: Vec<String>) -> anyhow::Result<(String, String)> {
        let (ca_cert, ca_key) = self.issuer()?;
        let key = KeyPair::generate().context("generating server key")?;
        let mut params = CertificateParams::new(sans)?;
        params
            .distinguished_name
            .push(DnType::CommonName, "aurcache");
        params.not_before = OffsetDateTime::now_utc() - Duration::hours(1);
        params.not_after = OffsetDateTime::now_utc() + Duration::days(3650);
        let cert = params
            .signed_by(&key, &ca_cert, &ca_key)
            .context("signing server cert")?;
        Ok((cert.pem(), key.serialize_pem()))
    }

    /// Sign a worker CSR, returning the issued certificate and its metadata.
    pub fn sign_worker_csr(
        &self,
        csr_pem: &str,
        validity_days: i64,
    ) -> anyhow::Result<SignedWorkerCert> {
        let fingerprint = fingerprint_from_csr_pem(csr_pem)?;
        let (ca_cert, ca_key) = self.issuer()?;

        let mut csr =
            CertificateSigningRequestParams::from_pem(csr_pem).context("parsing worker CSR")?;
        let not_after = OffsetDateTime::now_utc() + Duration::days(validity_days);
        csr.params.not_before = OffsetDateTime::now_utc() - Duration::hours(1);
        csr.params.not_after = not_after;

        let cert = csr
            .signed_by(&ca_cert, &ca_key)
            .context("signing worker CSR")?;
        Ok(SignedWorkerCert {
            cert_pem: cert.pem(),
            not_after: not_after.unix_timestamp(),
            fingerprint,
        })
    }
}

/// SHA-256 fingerprint (hex, lowercase) of the public key in a certificate PEM.
pub fn fingerprint_from_cert_pem(cert_pem: &str) -> anyhow::Result<String> {
    let der = pem_to_der(cert_pem)?;
    let (_, cert) = x509_parser::parse_x509_certificate(&der)
        .map_err(|e| anyhow!("parsing certificate: {e}"))?;
    Ok(sha256_hex(cert.tbs_certificate.subject_pki.raw))
}

/// SHA-256 fingerprint (hex, lowercase) of the public key in a certificate's DER.
pub fn fingerprint_from_cert_der(der: &[u8]) -> anyhow::Result<String> {
    let (_, cert) = x509_parser::parse_x509_certificate(der)
        .map_err(|e| anyhow!("parsing certificate: {e}"))?;
    Ok(sha256_hex(cert.tbs_certificate.subject_pki.raw))
}

/// SHA-256 fingerprint (hex, lowercase) of the public key in a CSR PEM.
pub fn fingerprint_from_csr_pem(csr_pem: &str) -> anyhow::Result<String> {
    use x509_parser::prelude::FromDer;
    let der = pem_to_der(csr_pem)?;
    let (_, csr) = x509_parser::certification_request::X509CertificationRequest::from_der(&der)
        .map_err(|e| anyhow!("parsing CSR: {e}"))?;
    Ok(sha256_hex(csr.certification_request_info.subject_pki.raw))
}

fn pem_to_der(pem: &str) -> anyhow::Result<Vec<u8>> {
    let (_, doc) = x509_parser::pem::parse_x509_pem(pem.as_bytes())
        .map_err(|e| anyhow!("parsing PEM: {e}"))?;
    Ok(doc.contents)
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// SHA-256 fingerprint (hex, lowercase) of a raw SubjectPublicKeyInfo DER slice.
///
/// Used by the mTLS auth guard, which obtains the SPKI bytes directly from the
/// verified client certificate. Matches the fingerprint computed from the CSR
/// and issued certificate.
#[must_use]
pub fn fingerprint_from_spki_der(spki_der: &[u8]) -> String {
    sha256_hex(spki_der)
}

fn write_secret(path: &Path, contents: &str) -> anyhow::Result<()> {
    std::fs::write(path, contents).context("writing secret")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .context("setting secret permissions")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_and_reloads_stable_ca() {
        let dir = tempfile::tempdir().unwrap();
        let ca1 = Ca::load_or_create(dir.path()).unwrap();
        let ca2 = Ca::load_or_create(dir.path()).unwrap();
        assert_eq!(ca1.ca_cert_pem(), ca2.ca_cert_pem());
        assert!(ca1.ca_cert_pem().contains("BEGIN CERTIFICATE"));
    }

    #[test]
    fn issues_server_cert_chaining_to_ca() {
        let dir = tempfile::tempdir().unwrap();
        let ca = Ca::load_or_create(dir.path()).unwrap();
        let (cert_pem, key_pem) = ca.issue_server_cert(vec!["localhost".to_string()]).unwrap();
        assert!(cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(key_pem.contains("PRIVATE KEY"));
    }

    #[test]
    fn signs_worker_csr_with_stable_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let ca = Ca::load_or_create(dir.path()).unwrap();

        // Worker side: generate a keypair + CSR.
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(vec!["worker-1".to_string()]).unwrap();
        params
            .distinguished_name
            .push(DnType::CommonName, "worker-1");
        let csr_pem = params.serialize_request(&key).unwrap().pem().unwrap();

        let csr_fp = fingerprint_from_csr_pem(&csr_pem).unwrap();

        let signed = ca.sign_worker_csr(&csr_pem, 365).unwrap();
        assert!(signed.cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(signed.not_after > 0);

        // The fingerprint is stable across CSR -> issued cert.
        let cert_fp = fingerprint_from_cert_pem(&signed.cert_pem).unwrap();
        assert_eq!(csr_fp, cert_fp);
        assert_eq!(signed.fingerprint, cert_fp);
    }

    #[test]
    fn different_workers_get_different_fingerprints() {
        let dir = tempfile::tempdir().unwrap();
        let ca = Ca::load_or_create(dir.path()).unwrap();

        let make_csr = || {
            let key = KeyPair::generate().unwrap();
            let params = CertificateParams::new(vec!["w".to_string()]).unwrap();
            params.serialize_request(&key).unwrap().pem().unwrap()
        };

        let a = ca.sign_worker_csr(&make_csr(), 365).unwrap();
        let b = ca.sign_worker_csr(&make_csr(), 365).unwrap();
        assert_ne!(a.fingerprint, b.fingerprint);
        assert_ne!(a.cert_pem, b.cert_pem);
    }
}
