//! Configuration specific to the `devtools` chroot executor.
//!
//! Protocol settings (identity, enrollment, scheduling, liveness) live in
//! [`CoreConfig`]; this adds only what building in a chroot needs. Both are
//! read from the same environment, so a worker is still configured as one flat
//! set of variables.

use crate::settings::keys;
use aurcache_worker_core::config::{CoreConfig, env_opt};
use aurcache_worker_core::settings::WorkerSettings;
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
    /// How long a freshly `-Syu`'d base chroot counts as current, in seconds
    /// (`0` refreshes before every build, which is what this used to do).
    ///
    /// The refresh costs ~13s and, measured over a day on the reference
    /// worker, 23 of 26 of them upgraded nothing at all: Arch's repositories
    /// move a few times a day, not a few times an hour. Every build paid for
    /// that, serially, before it could start.
    ///
    /// This bounds how stale the *shared* base may be, not what a build sees:
    /// every build syncs its own chroot copy before it starts
    /// (`makechrootpkg -u`, see [`crate::build::build_command`]), because
    /// AURCache's own repository moves whenever a build finishes rather than a
    /// few times a day. Keeping the base close to current is what leaves that
    /// per-build sync with nothing to download.
    pub chroot_refresh_interval: u64,
    /// Whether each build's chroot is an overlay on the base or a copy of it.
    ///
    /// Defaults to deciding at startup, because the right answer is a property
    /// of the machine: on btrfs a copy is a snapshot and already free, while
    /// anywhere else it is an rsync of the whole chroot. See
    /// [`crate::chroots::ChrootMode`].
    pub chroot_mode: crate::chroots::ChrootMode,
    /// Package cache TTL in seconds. Defaults to `0` (disabled) because a
    /// cached package's mtime is its *download* time — pacman does not touch
    /// it on a cache hit — so age-evicting would discard a package used daily
    /// simply for being old. Size pressure is the honest bound for this pool.
    pub pkgcache_ttl: u64,
    /// Memory, swap and CPU each build may use (`WORKER_BUILD_MEMORY_MAX`,
    /// `WORKER_BUILD_SWAP_MAX`, `WORKER_BUILD_CPUS`). Unset means unlimited, as
    /// it always was.
    pub build_limits: crate::cgroup::BuildLimits,
    /// Memory, swap and CPU all builds may use between them
    /// (`WORKER_TOTAL_BUILD_MEMORY_MAX`, `WORKER_TOTAL_BUILD_SWAP_MAX`,
    /// `WORKER_TOTAL_BUILD_CPUS`). Unset means unlimited.
    pub total_build_limits: crate::cgroup::BuildLimits,
}

impl Config {
    /// Build a [`Config`] from the process environment, applying zero-config
    /// defaults for everything not explicitly set.
    #[must_use]
    pub fn from_env() -> Self {
        let mut core = CoreConfig::from_env();
        // The executor's settings are declared beside the protocol ones, so
        // registration reports one table and the fields below read from the
        // same resolved values the server is shown.
        core.settings =
            std::mem::take(&mut core.settings).extended(crate::settings::chroot_settings());
        let settings = &core.settings;

        let (src_budget, pkg_budget) = split_cache_budgets(
            settings.size(keys::CACHE_MAX_SIZE),
            settings.size(keys::SRCCACHE_MAX_SIZE),
            settings.size(keys::PKGCACHE_MAX_SIZE),
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
            keyserver: settings
                .raw(keys::KEYSERVER)
                .unwrap_or(crate::settings::DEFAULT_KEYSERVER)
                .to_string(),
            build_user: env_opt("WORKER_BUILD_USER").unwrap_or_else(|| "builder".to_string()),
            cache_max_size: src_budget,
            cache_ttl: settings
                .duration(keys::CACHE_TTL)
                .unwrap_or(crate::settings::DEFAULT_CACHE_TTL),
            pkgcache_max_size: pkg_budget,
            pkgcache_ttl: settings.duration(keys::PKGCACHE_TTL).unwrap_or(0),
            chroot_refresh_interval: settings
                .duration(keys::CHROOT_REFRESH_INTERVAL)
                .unwrap_or(crate::settings::DEFAULT_CHROOT_REFRESH_INTERVAL),
            chroot_mode: crate::chroots::ChrootMode::parse(
                env_opt("WORKER_CHROOT_OVERLAY").as_deref(),
            ),
            build_limits: limits_from_settings(
                settings,
                keys::BUILD_MEMORY_MAX,
                keys::BUILD_SWAP_MAX,
                keys::BUILD_CPUS,
            ),
            total_build_limits: limits_from_settings(
                settings,
                keys::TOTAL_BUILD_MEMORY_MAX,
                keys::TOTAL_BUILD_SWAP_MAX,
                keys::TOTAL_BUILD_CPUS,
            ),
            core,
        }
    }
}

/// The same three limits per build and for all builds together, read the same
/// way from whichever pair of keys names them.
fn limits_from_settings(
    settings: &WorkerSettings,
    memory_key: &str,
    swap_key: &str,
    cpus_key: &str,
) -> crate::cgroup::BuildLimits {
    // `0` is the unlimited that leaving it unset already is, and writing it
    // would give the builds no memory at all.
    let memory_max = settings.size(memory_key).filter(|&bytes| bytes > 0);
    crate::cgroup::BuildLimits {
        memory_max,
        // A memory limit means no swap beyond it unless swap is asked for; see
        // `BuildLimits::swap_max` for why it cannot simply be left alone.
        swap_max: settings.size(swap_key).or_else(|| memory_max.map(|_| 0)),
        // A non-positive count is the unlimited that unset is: zero CPUs is not
        // a limit anyone means, and the declared minimum already turns a
        // negative one into a reported rejection.
        cpus: settings
            .float(cpus_key)
            .filter(|&cpus| cpus.is_finite() && cpus > 0.0),
    }
}

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
    let half = total.unwrap_or(crate::settings::DEFAULT_TOTAL_CACHE_SIZE) / 2;
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
        assert_eq!(src + pkg, crate::settings::DEFAULT_TOTAL_CACHE_SIZE);
    }

    /// The values every field had before they were resolved through the
    /// declared settings table, so a key wired to the wrong field is caught
    /// here rather than on a worker.
    ///
    /// Reads the process environment, as the worker does, so it asserts nothing
    /// on a machine that has configured any of these -- which is the same
    /// reason it is worth having: those variables are exactly what this
    /// resolves.
    #[test]
    fn a_clean_environment_still_produces_the_documented_defaults() {
        let configured: Vec<_> = crate::settings::chroot_settings()
            .into_iter()
            .chain(aurcache_worker_core::settings::protocol_settings())
            .map(|spec| spec.env_var)
            .filter(|var| {
                std::env::var_os(var).is_some()
                    || std::env::var_os(format!("{var}_DEFAULT")).is_some()
            })
            .collect();
        if !configured.is_empty() {
            eprintln!("skipped: {configured:?} set in this environment");
            return;
        }

        let cfg = Config::from_env();
        assert_eq!(cfg.core.concurrency, 1);
        assert_eq!(cfg.core.priority, 0);
        assert!(cfg.core.packages.is_empty());
        assert_eq!(cfg.core.poll_interval, 10);
        assert_eq!(cfg.core.build_timeout, 3 * 60 * 60);
        assert_eq!(cfg.core.builddir_max_bytes, 200 * 1024 * 1024 * 1024);
        assert_eq!(cfg.core.builddir_min_free, 50 * 1024 * 1024 * 1024);
        assert_eq!(cfg.keyserver, "hkps://keyserver.ubuntu.com");
        assert_eq!(cfg.cache_max_size, 10 * 1024 * 1024 * 1024);
        assert_eq!(cfg.pkgcache_max_size, 10 * 1024 * 1024 * 1024);
        assert_eq!(cfg.cache_ttl, 30 * 24 * 60 * 60);
        assert_eq!(cfg.pkgcache_ttl, 0);
        assert_eq!(cfg.chroot_refresh_interval, 15 * 60);
        assert_eq!(cfg.build_limits.memory_max, None);
        assert_eq!(cfg.build_limits.swap_max, None);
        assert_eq!(cfg.build_limits.cpus, None);
        assert_eq!(cfg.total_build_limits.memory_max, None);
    }

    /// `0` disables a budget, and must survive as `0` rather than being
    /// mistaken for "unset" and replaced by a default.
    #[test]
    fn zero_disables_rather_than_defaulting() {
        assert_eq!(split_cache_budgets(Some(1000), Some(0), None), (0, 500));
        assert_eq!(split_cache_budgets(Some(0), None, None), (0, 0));
    }
}
