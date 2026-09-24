//! Where a build gets its chroot, and how it gives it back.
//!
//! Every chroot lives in the worker's storage pool (`aurcache-chroot`): a
//! btrfs filesystem with simple quotas, backed by an image, a device or a
//! dedicated mount. The base chroot is a subvolume there, and a build's chroot
//! is a snapshot of it, taken by the worker before `makechrootpkg` runs, with
//! a second subvolume beside it for the build's workdir. Both sit in one quota
//! group limited to `WORKER_BUILD_DISK_MAX`, under the pool's total
//! (`WORKER_DISK_MAX`), so a build that sets out to fill the disk fails its own
//! build instead. See `design/in-progress/build-disk-quota.md`.
//!
//! A snapshot is point-in-time, so the base is refreshed in place while builds
//! run: its lock is held shared only for the instant of taking a snapshot. The
//! overlay chroots, update layers and flattening this replaced existed because
//! an overlay's lower layer must not change for as long as a build reads it.

use crate::chroot;
use anyhow::{Context, Result, bail};
use aurcache_chroot::{BuildVolumes, Pool, PoolConfig, Usage};
use aurcache_worker_core::client::WorkerClient;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, RwLock};

/// How long after a failed attempt to open the pool the next one is made.
/// Opening runs a handful of commands; asking before every claim would repeat
/// them every few seconds for as long as the host stays broken.
const REOPEN_AFTER: Duration = Duration::from_secs(60);

/// The build id a local one-shot build (`build-once`) uses in the pool. A
/// served build never has it, so the two cannot collide even when a one-shot
/// build shares a pool with a running worker.
pub const ONE_SHOT_BUILD_ID: i32 = i32::MAX;

/// The chroots on this worker: one base, and a snapshot per build.
pub struct Chroots {
    config: std::sync::Mutex<PoolConfig>,
    pool: RwLock<Option<Pool>>,
    /// When opening the pool last failed, so retries are spaced out.
    last_failure: std::sync::Mutex<Option<Instant>>,
    /// When the base chroot was last brought up to date, and the lock that
    /// serialises doing so. One lock for both, because "is it due" and "make
    /// it current" have to be one decision or two builds starting together
    /// both find it due.
    last_refresh: Mutex<Option<Instant>>,
    /// How long a refresh counts as current, in seconds. Atomic because it is
    /// a setting the server may change while builds run.
    interval: AtomicU64,
    /// Whether the last readiness check found the host short of room, so the
    /// change is logged once rather than at every poll.
    short_of_room: AtomicBool,
}

impl Chroots {
    /// Chroots in the pool `config` describes. Nothing is mounted until
    /// [`Self::open`], so a pool that cannot be opened yet costs a worker its
    /// builds, not its ability to start and say why.
    #[must_use]
    pub fn new(config: PoolConfig, interval: Duration) -> Self {
        Self {
            config: std::sync::Mutex::new(config),
            pool: RwLock::new(None),
            last_failure: std::sync::Mutex::new(None),
            last_refresh: Mutex::new(None),
            interval: AtomicU64::new(interval.as_secs()),
            short_of_room: AtomicBool::new(false),
        }
    }

    /// Open the pool if it is not open yet. Returns whether it is.
    ///
    /// A failure is logged and retried no sooner than [`REOPEN_AFTER`]: a host
    /// without btrfs support or a free loop device does not start having one
    /// between two claims.
    pub async fn open(&self) -> bool {
        if self.pool.read().await.is_some() {
            return true;
        }
        let mut pool = self.pool.write().await;
        if pool.is_some() {
            return true;
        }
        {
            let last = self.last_failure.lock().expect("not poisoned");
            if last.is_some_and(|at| at.elapsed() < REOPEN_AFTER) {
                return false;
            }
        }
        let config = self.config.lock().expect("not poisoned").clone();
        match Pool::open(config).await {
            Ok(opened) => {
                tracing::info!(
                    "storage pool mounted at {}{}",
                    opened.path().display(),
                    opened.total_usage().map_or_else(String::new, |u| format!(
                        ", {} of {} used",
                        gib(u.used),
                        u.limit.map_or_else(|| "unlimited".to_string(), gib)
                    ))
                );
                *pool = Some(opened);
                true
            }
            Err(e) => {
                tracing::error!(
                    "the storage pool could not be opened ({e:#}); this worker takes no \
                     builds until it can, because a build without its disk quota could \
                     fill the host"
                );
                *self.last_failure.lock().expect("not poisoned") = Some(Instant::now());
                false
            }
        }
    }

    /// Whether a build limited to `build_limit` can start: once the pool is
    /// open, and -- for a sparse image -- while the host has room for the
    /// build to use its whole quota.
    ///
    /// Asked before claiming, not after: a claimed job holds a lease the
    /// server expects progress on, while one left queued waits where everyone
    /// can see it. The check is a guard, not a guarantee -- the host's space is
    /// shared with whatever else runs there -- and a host that fills up anyway
    /// fails the builds writing at that moment, not the pool.
    pub async fn ready_for_work(&self, build_limit: u64) -> bool {
        if !self.open().await {
            return false;
        }
        let free = self.pool.read().await.as_ref().and_then(Pool::host_free);
        let short = free.is_some_and(|free| free < build_limit);
        let was_short = self.short_of_room.swap(short, Ordering::Relaxed);
        if short && !was_short {
            tracing::warn!(
                "not taking builds: the host has {} free for the storage pool's image, less than \
                 one build's quota of {} (WORKER_BUILD_DISK_MAX). Free space there, or reserve \
                 the image with WORKER_DISK_RESERVE",
                gib(free.unwrap_or(0)),
                gib(build_limit)
            );
        } else if !short && was_short {
            tracing::info!("the host has room for a build again; taking builds");
        }
        !short
    }

    /// Change how long a refresh counts as current, from the next decision on.
    pub fn set_interval(&self, interval: Duration) {
        self.interval.store(interval.as_secs(), Ordering::Relaxed);
    }

    fn interval(&self) -> Duration {
        Duration::from_secs(self.interval.load(Ordering::Relaxed))
    }

    /// Set the total everything in the pool may use. Applies at once, to
    /// builds already running.
    pub async fn set_total(&self, total: u64) -> Result<()> {
        self.config.lock().expect("not poisoned").total = total;
        match self.pool.read().await.as_ref() {
            Some(pool) => pool.set_total(total).await,
            // Opened with it, whenever it is.
            None => Ok(()),
        }
    }

    /// Remove what builds left in the pool, except those in `keep`. Returns
    /// how many were cleared. Nothing to do on a pool not open yet.
    pub async fn sweep(&self, keep: &HashSet<i32>) -> usize {
        match self.pool.read().await.as_ref() {
            Some(pool) => pool.sweep(keep).await,
            None => 0,
        }
    }

    /// Bring the base chroot up to date, making it if it does not exist.
    ///
    /// Refreshing costs ~13s, and Arch's repositories move a few times a day,
    /// so it happens at most once per `WORKER_CHROOT_REFRESH_INTERVAL` rather
    /// than per build. Every build still syncs its own snapshot before it
    /// starts (`makechrootpkg -u`); this bounds how much that has to fetch.
    ///
    /// `report_to` names who to tell about a problem preparing the chroot, and
    /// the build to file it under -- `None` for `build-once`.
    pub async fn refresh(
        &self,
        pacman_conf: &Path,
        report_to: Option<(&WorkerClient, i32)>,
    ) -> Result<PathBuf> {
        if !self.open().await {
            bail!("the storage pool is not available; see the worker's log");
        }
        let guard = self.pool.read().await;
        let pool = guard.as_ref().expect("opened above");
        let root = pool.root();
        let mut last = self.last_refresh.lock().await;
        if root.exists() && !refresh_due(last.map(|at| at.elapsed()), self.interval()) {
            tracing::debug!("base chroot is current; not refreshing");
            return Ok(root);
        }
        // Only while no snapshot is being taken of it. That takes an instant,
        // so a busy base is rare, and it means "later", never "queue up": this
        // holds the lock every job start needs. `last` is deliberately not
        // stamped, so the next build tries again.
        let existed = root.exists();
        if existed {
            match chroot::try_lock_base(&root).await {
                chroot::BaseLock::Held(lock) => {
                    let root =
                        chroot::ensure_base_chroot(pool.path(), pacman_conf, report_to).await?;
                    drop(lock);
                    *last = Some(Instant::now());
                    return Ok(root);
                }
                chroot::BaseLock::Busy => {
                    tracing::info!("a build is snapshotting the base chroot; refreshing it later");
                    return Ok(root);
                }
                chroot::BaseLock::Unavailable => {}
            }
        }
        let root = chroot::ensure_base_chroot(pool.path(), pacman_conf, report_to).await?;
        if !existed {
            // `mkarchroot` made the base a subvolume of its own; until it is
            // counted under the pool's total, the base's size is not.
            pool.charge_to_total(&root)
                .await
                .context("counting the new base chroot against the pool's total")?;
        }
        *last = Some(Instant::now());
        Ok(root)
    }

    /// Take a chroot for one build, limited to `limit` bytes of disk. Give it
    /// back with [`Self::release`], or use [`Self::with_lease`].
    pub async fn acquire(&self, build_id: i32, limit: Option<u64>) -> Result<Lease> {
        if !self.open().await {
            bail!("the storage pool is not available; see the worker's log");
        }
        let guard = self.pool.read().await;
        let pool = guard.as_ref().expect("opened above");
        // Shared, for the instant of the snapshot: a refresh must not be
        // halfway through the base when it is taken.
        let lock = chroot::share_base_chroot(&pool.root()).await;
        let volumes = pool.lease(build_id, limit).await;
        drop(lock);
        Ok(Lease {
            dir: pool.path().to_path_buf(),
            volumes: volumes?,
            limit,
        })
    }

    /// Give a build's chroot back: both its subvolumes go, and the space they
    /// held is freed in the background by btrfs.
    ///
    /// Awaited rather than left to `Drop`: it has to happen *after* the
    /// build's child has been reaped, and drop order is a poor place to state
    /// that dependency.
    pub async fn release(&self, lease: Lease) {
        match self.pool.read().await.as_ref() {
            Some(pool) => pool.release(lease.volumes).await,
            // Not reachable: a lease comes from an open pool, and a pool is
            // never closed while the worker runs. The sweep would find it.
            None => drop(lease.volumes),
        }
    }

    /// Run `body` with a chroot and give it back afterwards -- on the error
    /// path too, which is the half a caller forgets.
    pub async fn with_lease<T, F>(&self, build_id: i32, limit: Option<u64>, body: F) -> Result<T>
    where
        F: AsyncFnOnce(&Lease) -> Result<T>,
    {
        let lease = self.acquire(build_id, limit).await?;
        let out = body(&lease).await;
        self.release(lease).await;
        out
    }

    /// Why a build that failed may have failed for want of disk, in the terms
    /// of the setting to change. `None` when neither its quota nor the pool's
    /// total was reached.
    pub async fn disk_reason(&self, lease: &Lease) -> Option<String> {
        let guard = self.pool.read().await;
        let pool = guard.as_ref()?;
        // A write that did not fit leaves the group just short of its limit,
        // by up to the size of that write; a megabyte covers what makepkg and
        // compilers write at once.
        const SLACK: u64 = 1 << 20;
        disk_reason(pool.usage(lease.volumes.group()), pool.total_usage(), SLACK)
    }
}

/// See [`Chroots::disk_reason`].
fn disk_reason(build: Option<Usage>, total: Option<Usage>, slack: u64) -> Option<String> {
    if let Some(usage) = build.filter(|u| u.at_limit(slack)) {
        return Some(format!(
            "out of disk: the build reached its disk quota of {} (WORKER_BUILD_DISK_MAX)",
            gib(usage.limit.expect("at_limit implies a limit"))
        ));
    }
    if let Some(usage) = total.filter(|u| u.at_limit(slack)) {
        return Some(format!(
            "out of disk: this worker's storage is full ({} of WORKER_DISK_MAX); this build \
             may have been under its own quota",
            gib(usage.limit.expect("at_limit implies a limit"))
        ));
    }
    None
}

/// Whether the base chroot is due a refresh.
fn refresh_due(since_last: Option<Duration>, interval: Duration) -> bool {
    since_last.is_none_or(|elapsed| elapsed >= interval)
}

fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / f64::from(1u32 << 30))
}

/// One build's claim on a chroot, and on the workdir beside it.
pub struct Lease {
    dir: PathBuf,
    volumes: BuildVolumes,
    limit: Option<u64>,
}

impl Lease {
    /// The `-r` argument: the directory the chroots live in.
    #[must_use]
    pub fn chroot_dir(&self) -> &Path {
        &self.dir
    }

    /// The `-l` argument: what this build's chroot is called.
    #[must_use]
    pub fn label(&self) -> &str {
        self.volumes.label()
    }

    /// The build's own writable space, counted against its quota: where its
    /// source is extracted and its packages land.
    #[must_use]
    pub fn workdir(&self) -> &Path {
        self.volumes.data()
    }

    /// The disk this build may use, `None` when unlimited.
    #[must_use]
    pub const fn limit(&self) -> Option<u64> {
        self.limit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A worker that has just started refreshes whatever the interval says --
    /// it may have been down for a week -- and an interval of zero keeps the
    /// old behaviour of refreshing before every build.
    #[test]
    fn a_refresh_is_due_when_it_has_never_happened_or_has_aged_out() {
        let interval = Duration::from_secs(900);
        assert!(refresh_due(None, interval));
        assert!(!refresh_due(Some(Duration::from_secs(60)), interval));
        assert!(refresh_due(Some(Duration::from_secs(900)), interval));
        assert!(refresh_due(Some(Duration::from_secs(60)), Duration::ZERO));
    }

    /// The reason names the setting that was reached, and prefers the build's
    /// own quota: when both are full, raising the total alone would not help.
    #[test]
    fn a_disk_failure_names_the_quota_that_was_reached() {
        let gib = 1u64 << 30;
        let at = |used, limit| {
            Some(Usage {
                used,
                limit: Some(limit),
            })
        };
        let build = disk_reason(at(50 * gib, 50 * gib), at(100 * gib, 200 * gib), 0).unwrap();
        assert!(build.contains("WORKER_BUILD_DISK_MAX"), "{build}");
        assert!(build.contains("50.0 GiB"), "{build}");

        let total = disk_reason(at(10 * gib, 50 * gib), at(200 * gib, 200 * gib), 0).unwrap();
        assert!(total.contains("WORKER_DISK_MAX"), "{total}");

        assert_eq!(
            disk_reason(at(10 * gib, 50 * gib), at(20 * gib, 200 * gib), 0),
            None
        );
        assert_eq!(disk_reason(None, None, 0), None);
    }
}
