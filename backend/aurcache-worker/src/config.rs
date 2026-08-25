//! Configuration specific to the `devtools` chroot executor.
//!
//! Protocol settings (identity, enrollment, scheduling, liveness) live in
//! [`CoreConfig`]; this adds only what building in a chroot needs. Both are
//! read from the same environment, so a worker is still configured as one flat
//! set of variables.

use aurcache_worker_core::config::{CoreConfig, env_opt, parse_size};
use std::path::PathBuf;

/// Fully-resolved configuration for the chroot worker.
#[derive(Clone, Debug)]
pub struct Config {
    /// Settings shared with every executor.
    pub core: CoreConfig,
    /// Explicit SSH key for authenticated sources. When set, no key is
    /// generated — see `credentials.rs`.
    pub git_ssh_key: Option<PathBuf>,
    /// Optional `known_hosts` to trust inside the build chroot.
    pub ssh_known_hosts: Option<PathBuf>,
    /// Extra `host:chroot` bind mounts exposed to every build, for credentials
    /// that are not SSH (a `.netrc`, an API token, a licence file).
    pub bind_mounts: Vec<(PathBuf, PathBuf)>,
    /// Directory holding the shared base chroot + per-job copies.
    pub chroot_dir: PathBuf,
    /// Directory holding worker-local caches (srcdest, gnupg, pacman pkg).
    pub cache_dir: PathBuf,
    /// Keyserver for `gpg --recv-keys`.
    pub keyserver: String,
    /// Unix user each build runs as.
    ///
    /// Must differ from the user the worker itself runs as: the worker's mTLS
    /// identity and build credentials are protected from PKGBUILD code by file
    /// ownership, which only separates them if the two users differ.
    pub build_user: String,
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

impl Config {
    /// Build a [`Config`] from the process environment, applying zero-config
    /// defaults for everything not explicitly set.
    #[must_use]
    pub fn from_env() -> Self {
        let core = CoreConfig::from_env();

        let (src_budget, pkg_budget) = split_cache_budgets(
            env_opt("WORKER_CACHE_MAX_SIZE").and_then(parse_size),
            env_opt("WORKER_SRCCACHE_MAX_SIZE").and_then(parse_size),
            env_opt("WORKER_PKGCACHE_MAX_SIZE").and_then(parse_size),
        );

        // The chroot lives under the data dir by default so that persisting one
        // volume persists the identity *and* the expensive base chroot; a
        // worker whose data dir moved to a bigger disk should not silently
        // leave its largest directory behind on the old one.
        let chroot_dir = env_opt("WORKER_CHROOT_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| core.data_dir.join("chroot"));

        Self {
            git_ssh_key: env_opt("WORKER_GIT_SSH_KEY").map(PathBuf::from),
            ssh_known_hosts: env_opt("WORKER_SSH_KNOWN_HOSTS").map(PathBuf::from),
            bind_mounts: env_opt("WORKER_BIND_MOUNTS")
                .map(|raw| crate::credentials::parse_bind_mounts(&raw))
                .unwrap_or_default(),
            chroot_dir,
            cache_dir: env_opt("WORKER_CACHE_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/var/cache/aurcache-worker")),
            keyserver: env_opt("WORKER_KEYSERVER")
                .unwrap_or_else(|| "hkps://keyserver.ubuntu.com".to_string()),
            build_user: env_opt("WORKER_BUILD_USER").unwrap_or_else(|| "builder".to_string()),
            cache_max_size: src_budget,
            cache_ttl: env_opt("WORKER_CACHE_TTL")
                .and_then(|s| s.parse().ok())
                .unwrap_or(30 * 24 * 60 * 60),
            pkgcache_max_size: pkg_budget,
            pkgcache_ttl: env_opt("WORKER_PKGCACHE_TTL")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
            core,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn total_budget_is_split_evenly() {
        assert_eq!(split_cache_budgets(Some(1000), None, None), (500, 500));
    }

    /// A pinned pool is honoured exactly; the total only fills in the pool the
    /// operator left unset.
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
}
