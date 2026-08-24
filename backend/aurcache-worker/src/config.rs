//! Worker runtime configuration, assembled from environment variables with
//! zero-config defaults so the bundled single-host topology needs no tuning.

use std::path::PathBuf;

/// Fully-resolved worker configuration.
#[derive(Clone, Debug)]
pub struct Config {
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
    /// Explicit SSH key for authenticated sources. When set, no key is
    /// generated — see `credentials.rs`.
    pub git_ssh_key: Option<PathBuf>,
    /// Optional `known_hosts` to trust inside the build chroot.
    pub ssh_known_hosts: Option<PathBuf>,
    /// Extra `host:chroot` bind mounts exposed to every build, for credentials
    /// that are not SSH (a `.netrc`, an API token, a licence file).
    pub bind_mounts: Vec<(PathBuf, PathBuf)>,
    /// Maximum concurrent builds.
    pub concurrency: usize,
    /// Human-friendly worker name (defaults to hostname).
    pub name: String,
    /// Directory where identity (key + cert) is persisted.
    pub data_dir: PathBuf,
    /// Directory holding the shared base chroot + per-job copies.
    pub chroot_dir: PathBuf,
    /// Directory holding worker-local caches (srcdest, gnupg, pacman pkg).
    pub cache_dir: PathBuf,
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
    /// Keyserver for `gpg --recv-keys`.
    pub keyserver: String,
    /// Source (`SRCDEST`) cache budget in bytes (`0` disables size-based
    /// eviction).
    pub cache_max_size: u64,
    /// Cache entry TTL in seconds (`0` disables age-based eviction).
    pub cache_ttl: u64,
    /// Shared pacman package cache budget in bytes (`0` disables size-based
    /// eviction). Kept separate from the source budget: the two pools have
    /// very different sizes and refill costs, and a shared budget would let
    /// large VCS checkouts starve the package cache (or vice versa).
    pub pkgcache_max_size: u64,
    /// Package cache TTL in seconds. Defaults to `0` (disabled) because a
    /// cached package's mtime is its *download* time — pacman does not touch
    /// it on a cache hit — so age-evicting would discard a package used daily
    /// simply for being old. Size pressure is the honest bound for this pool.
    pub pkgcache_ttl: u64,
}

fn env_opt(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// Parse a comma/space separated arch list into a normalized vector.
pub fn parse_arches(raw: &str) -> Vec<String> {
    raw.split([',', ' '])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Detect the host architecture via `uname -m`, defaulting to `x86_64`.
fn detect_arch() -> String {
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

fn detect_nproc() -> usize {
    std::thread::available_parallelism()
        .map(std::num::NonZero::get)
        .unwrap_or(1)
}

impl Config {
    /// Build a [`Config`] from the process environment, applying zero-config
    /// defaults for everything not explicitly set.
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

        let concurrency = env_opt("WORKER_CONCURRENCY")
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|n| *n > 0)
            .unwrap_or_else(detect_nproc);

        let name = env_opt("WORKER_NAME").unwrap_or_else(detect_hostname);

        let (src_budget, pkg_budget) = split_cache_budgets(
            env_opt("WORKER_CACHE_MAX_SIZE").and_then(parse_size),
            env_opt("WORKER_SRCCACHE_MAX_SIZE").and_then(parse_size),
            env_opt("WORKER_PKGCACHE_MAX_SIZE").and_then(parse_size),
        );

        let data_dir = env_opt("WORKER_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/var/lib/aurcache-worker"));
        let chroot_dir = env_opt("WORKER_CHROOT_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/var/lib/aurcache-worker/chroot"));
        let cache_dir = env_opt("WORKER_CACHE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/var/cache/aurcache-worker"));

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
            git_ssh_key: env_opt("WORKER_GIT_SSH_KEY").map(PathBuf::from),
            ssh_known_hosts: env_opt("WORKER_SSH_KNOWN_HOSTS").map(PathBuf::from),
            bind_mounts: env_opt("WORKER_BIND_MOUNTS")
                .map(|raw| crate::credentials::parse_bind_mounts(&raw))
                .unwrap_or_default(),
            concurrency,
            name,
            data_dir,
            chroot_dir,
            cache_dir,
            heartbeat_interval: env_opt("WORKER_HEARTBEAT_INTERVAL")
                .and_then(|s| s.parse().ok())
                .unwrap_or(15),
            lease_ttl: env_opt("LEASE_TTL")
                .and_then(|s| s.parse().ok())
                .unwrap_or(60),
            poll_interval: env_opt("WORKER_POLL_INTERVAL")
                .and_then(|s| s.parse().ok())
                .unwrap_or(10),
            build_timeout: env_opt("WORKER_BUILD_TIMEOUT")
                .and_then(|s| s.parse().ok())
                .unwrap_or(3 * 60 * 60),
            keyserver: env_opt("WORKER_KEYSERVER")
                .unwrap_or_else(|| "hkps://keyserver.ubuntu.com".to_string()),
            cache_max_size: src_budget,
            cache_ttl: env_opt("WORKER_CACHE_TTL")
                .and_then(|s| s.parse().ok())
                .unwrap_or(30 * 24 * 60 * 60),
            pkgcache_max_size: pkg_budget,
            pkgcache_ttl: env_opt("WORKER_PKGCACHE_TTL")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
        }
    }
}

/// Total worker cache budget when nothing is configured.
const DEFAULT_TOTAL_CACHE_SIZE: u64 = 20 * 1024 * 1024 * 1024;

/// Resolve the source and package cache budgets.
///
/// `WORKER_CACHE_MAX_SIZE` is the **total** disk the worker's caches may use,
/// split evenly between sources and packages — one number is what most
/// deployments actually want to think about, and the name reads as a total.
///
/// Either pool can be pinned with `WORKER_SRCCACHE_MAX_SIZE` /
/// `WORKER_PKGCACHE_MAX_SIZE`, which also makes the split ratio configurable
/// without a separate knob for it. A pinned pool wins outright: the total is a
/// default for whatever is left unset, not a cap enforced over explicit values,
/// because silently shrinking a number the operator wrote down is worse than
/// exceeding one they did not.
#[must_use]
pub fn split_cache_budgets(total: Option<u64>, src: Option<u64>, pkg: Option<u64>) -> (u64, u64) {
    let half = total.unwrap_or(DEFAULT_TOTAL_CACHE_SIZE) / 2;
    (src.unwrap_or(half), pkg.unwrap_or(half))
}

/// Parse a human size like `20G`, `500M`, `1024` (bytes) into a byte count.
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
    fn total_budget_is_split_evenly() {
        assert_eq!(split_cache_budgets(Some(1000), None, None), (500, 500));
    }

    /// Pinning one pool must not silently redistribute the other: setting the
    /// package budget should not change how much the source cache may use.
    #[test]
    fn a_pinned_pool_wins_and_leaves_the_other_alone() {
        assert_eq!(split_cache_budgets(Some(1000), None, Some(900)), (500, 900));
        assert_eq!(split_cache_budgets(Some(1000), Some(900), None), (900, 500));
        assert_eq!(split_cache_budgets(Some(10), Some(1), Some(2)), (1, 2));
    }

    #[test]
    fn defaults_split_the_default_total() {
        let (src, pkg) = split_cache_budgets(None, None, None);
        assert_eq!(src, pkg);
        assert_eq!(src + pkg, DEFAULT_TOTAL_CACHE_SIZE);
    }

    /// `0` disables a budget, and must survive as `0` rather than being
    /// mistaken for "unset" and replaced by a default.
    #[test]
    fn zero_disables_rather_than_defaulting() {
        assert_eq!(split_cache_budgets(Some(1000), Some(0), None), (0, 500));
        assert_eq!(split_cache_budgets(Some(0), None, None), (0, 0));
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
