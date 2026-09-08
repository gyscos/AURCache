//! Protocol-level worker configuration, assembled from environment variables
//! with zero-config defaults so the bundled single-host topology needs no
//! tuning.
//!
//! Only settings the *protocol* needs live here — identity, enrollment,
//! scheduling and liveness. How a job is actually turned into a package is an
//! executor concern, so each executor crate parses its own settings alongside
//! this (see `aurcache_worker::config::Config`).

use std::path::PathBuf;

/// Worker configuration shared by every executor.
#[derive(Clone, Debug)]
pub struct CoreConfig {
    /// Backend API base URL, e.g. `https://aurcache:8080`.
    pub aurcache_url: String,
    /// Optional pinned CA/server fingerprint (SHA-256 hex). When unset the
    /// worker trusts-on-first-use the CA it downloads during enrollment.
    pub server_ca_fingerprint: Option<String>,
    /// Shared enrollment volume (bundled topology drops the CSR here).
    pub enrollment_dir: Option<PathBuf>,
    /// Shared-secret enrollment token (fallback path).
    pub enrollment_token: Option<String>,
    /// Architectures this worker builds natively.
    pub native_arches: Vec<String>,
    /// Architectures this worker can build via emulation.
    pub emulated_arches: Vec<String>,
    /// Exact pkgbase names this worker is specially provisioned for
    /// (credentials, licensed toolchain, scratch space). Packages named here
    /// are reserved to workers that name them.
    pub packages: Vec<String>,
    /// Scheduling preference; higher wins. Lower-priority workers hold back
    /// while a higher-priority one has capacity.
    pub priority: i32,
    /// Maximum concurrent builds.
    pub concurrency: usize,
    /// Human-friendly worker name (defaults to hostname).
    pub name: String,
    /// Directory where identity (key + cert) is persisted.
    pub data_dir: PathBuf,
    /// Heartbeat cadence in seconds.
    pub heartbeat_interval: u64,
    /// Lease TTL in seconds; the worker self-aborts a build it cannot report
    /// for longer than this (the server will have requeued it).
    pub lease_ttl: u64,
    /// Poll interval when no job is available, in seconds.
    pub poll_interval: u64,
    /// Per-build timeout in seconds (`0` disables the worker-side timeout;
    /// the build is killed and reported as a timeout when exceeded).
    pub build_timeout: u64,
    /// Bytes to keep free on the persistent build-tree filesystem.
    pub builddir_min_free: u64,
    /// Overrides the host used in the `[repo]` section, for deployments where
    /// the address used to reach the worker protocol is not the address that
    /// serves the package repository.
    pub repo_host: Option<String>,
    /// Overrides the whole repository base URL, not just its host.
    ///
    /// The server renders one template for every worker, so scheme, port and
    /// path are the same for all of them and only the host varies. That holds
    /// while every worker reaches the repository the same way, and stops
    /// holding as soon as one does not: a worker inside the compose network
    /// uses `http://aurcache:8081` while one across the internet comes in
    /// through a reverse proxy at `https://aur.example.com/repo`. Differing in
    /// host alone cannot express that.
    ///
    /// Takes precedence over [`Self::repo_host`], which is the narrower case of
    /// the same thing.
    pub repo_url: Option<String>,
    /// A mirrorlist this worker uses in place of anything the server sends.
    ///
    /// From `WORKER_MIRRORLIST_SERVERS` (a `;`-separated server list, rendered
    /// into `Server =` lines) or `WORKER_MIRRORLIST_FILE` (a path to a ready
    /// mirrorlist). Set for a worker whose local mirrors beat the server's --
    /// a worker on other hardware, or on the far side of a slow link from the
    /// mirror the server happens to prefer.
    ///
    /// When this is set the server is told not to send one at all, rather than
    /// sending bytes the worker would discard.
    pub mirrorlist: Option<String>,
}

/// Read an environment variable, treating blank values as unset.
#[must_use]
pub fn env_opt(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// Parse a comma/space separated arch list into a normalized vector.
#[must_use]
pub fn parse_arches(raw: &str) -> Vec<String> {
    raw.split([',', ' '])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Detect the host architecture via `uname -m`, defaulting to `x86_64`.
#[must_use]
pub fn detect_arch() -> String {
    std::process::Command::new("uname")
        .arg("-m")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "x86_64".to_string())
}

fn detect_hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "aurcache-worker".to_string())
}

impl CoreConfig {
    /// Build a [`CoreConfig`] from the process environment, applying
    /// zero-config defaults for everything not explicitly set.
    #[must_use]
    pub fn from_env() -> Self {
        let native_arches = env_opt("WORKER_ARCHES")
            .map(|s| parse_arches(&s))
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| vec![detect_arch()]);

        let emulated_arches = env_opt("WORKER_EMULATED_ARCHES")
            .map(|s| parse_arches(&s))
            .unwrap_or_default();

        // Reuses `parse_arches`: both are comma/space separated token lists.
        let packages = env_opt("WORKER_PACKAGES")
            .map(|s| parse_arches(&s))
            .unwrap_or_default();

        // Default to one build at a time, not to the core count.
        //
        // Each build is *already* parallel: the server renders
        // `MAKEFLAGS=-j$(nproc)` into every job's makepkg.conf. Defaulting
        // concurrency to nproc therefore multiplies out to nproc x nproc
        // compiler processes — 576 on a 24-core machine — while each build also
        // holds its own chroot copy and its own package cache. The result is
        // memory exhaustion and disk pressure, not throughput.
        //
        // One is the honest default: predictable, and an operator who has the
        // headroom opts in with WORKER_CONCURRENCY.
        let concurrency = env_opt("WORKER_CONCURRENCY")
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(1);

        Self {
            aurcache_url: env_opt("AURCACHE_URL")
                .unwrap_or_else(|| "https://localhost:8080".to_string())
                .trim_end_matches('/')
                .to_string(),
            server_ca_fingerprint: env_opt("AURCACHE_SERVER_CA_FINGERPRINT")
                .map(|s| s.to_lowercase().replace([':', ' '], "")),
            enrollment_dir: env_opt("AURCACHE_ENROLLMENT_DIR")
                .map(PathBuf::from)
                .or_else(|| Some(PathBuf::from("/enroll"))),
            enrollment_token: env_opt("AURCACHE_ENROLLMENT_TOKEN"),
            native_arches,
            emulated_arches,
            packages,
            priority: env_opt("WORKER_PRIORITY")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
            concurrency,
            name: env_opt("WORKER_NAME").unwrap_or_else(detect_hostname),
            data_dir: env_opt("WORKER_DATA_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/var/lib/aurcache-worker")),
            heartbeat_interval: env_opt("WORKER_HEARTBEAT_INTERVAL")
                .and_then(|s| s.parse().ok())
                .unwrap_or(15),
            lease_ttl: env_opt("LEASE_TTL")
                .and_then(|s| s.parse().ok())
                .unwrap_or(60),
            poll_interval: env_opt("WORKER_POLL_INTERVAL")
                .and_then(|s| s.parse().ok())
                .unwrap_or(10),
            // Free space to keep spare on the build-tree filesystem. A
            // worker setting, not a server one: it is this machine's disk.
            // Default 50 GiB, which is generous for ordinary packages and
            // deliberately not enough to make one opt-in fill a disk unnoticed.
            builddir_min_free: env_opt("WORKER_BUILDDIR_MIN_FREE")
                .and_then(|v| v.parse().ok())
                .unwrap_or(50 * 1024 * 1024 * 1024),
            build_timeout: env_opt("WORKER_BUILD_TIMEOUT")
                .and_then(|s| s.parse().ok())
                .unwrap_or(3 * 60 * 60),
            repo_host: env_opt("AURCACHE_REPO_HOST"),
            repo_url: env_opt("AURCACHE_REPO_URL"),
            mirrorlist: local_mirrorlist(),
        }
    }
}

/// The worker's own mirrorlist, if it has been given one.
///
/// `WORKER_MIRRORLIST_SERVERS` wins over `WORKER_MIRRORLIST_FILE`: it is the
/// more specific statement of intent, and an unreadable file should not
/// silently override an explicit list. A file that cannot be read is a warning
/// and not a failure -- the worker then takes the server's mirrorlist, which is
/// what it would have done with neither variable set.
fn local_mirrorlist() -> Option<String> {
    if let Some(servers) = env_opt("WORKER_MIRRORLIST_SERVERS") {
        let rendered: String = servers
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| format!("Server = {s}\n"))
            .collect();
        if !rendered.is_empty() {
            return Some(rendered);
        }
    }
    let path = env_opt("WORKER_MIRRORLIST_FILE")?;
    match std::fs::read_to_string(&path) {
        Ok(content) if !content.trim().is_empty() => Some(content),
        Ok(_) => {
            tracing::warn!("WORKER_MIRRORLIST_FILE {path} is empty; using the server's mirrorlist");
            None
        }
        Err(e) => {
            tracing::warn!("WORKER_MIRRORLIST_FILE {path} unreadable ({e}); using the server's");
            None
        }
    }
}

/// Parse a human size like `20G`, `500M`, `1024` (bytes) into a byte count.
#[must_use]
pub fn parse_size(raw: impl AsRef<str>) -> Option<u64> {
    let s = raw.as_ref().trim();
    if s.is_empty() {
        return None;
    }
    // Split off the trailing unit by chars, so a multi-byte suffix can never
    // slice mid-character.
    let mut chars = s.chars();
    let unit = chars.next_back()?.to_ascii_uppercase();
    let (num, mult) = match unit {
        'K' => (chars.as_str(), 1024u64),
        'M' => (chars.as_str(), 1024 * 1024),
        'G' => (chars.as_str(), 1024 * 1024 * 1024),
        'T' => (chars.as_str(), 1024u64 * 1024 * 1024 * 1024),
        _ => (s, 1),
    };
    num.trim().parse::<u64>().ok()?.checked_mul(mult)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_arch_lists() {
        assert_eq!(parse_arches("x86_64"), vec!["x86_64"]);
        assert_eq!(parse_arches("aarch64, armv7h"), vec!["aarch64", "armv7h"]);
        assert_eq!(parse_arches("a b,c  d"), vec!["a", "b", "c", "d"]);
        assert!(parse_arches("  ,  ").is_empty());
    }

    #[test]
    fn parses_sizes() {
        assert_eq!(parse_size("1024"), Some(1024));
        assert_eq!(parse_size("20G"), Some(20 * 1024 * 1024 * 1024));
        assert_eq!(parse_size("500m"), Some(500 * 1024 * 1024));
        assert_eq!(parse_size("2T"), Some(2u64 * 1024 * 1024 * 1024 * 1024));
        assert_eq!(parse_size(""), None);
        assert_eq!(parse_size("abc"), None);
    }
}
