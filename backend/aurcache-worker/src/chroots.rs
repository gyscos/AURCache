//! Where a build gets its chroot, and how it gives it back.
//!
//! `makechrootpkg` builds in a *copy* of a base chroot, and how that copy is
//! made -- devtools copying it itself, a btrfs snapshot, a reflink, an overlay
//! mount -- settles three other things at once: what `build_command` passes,
//! who deletes the copy when the build ends, and what the startup sweep must do
//! about one a crash left behind. Answered separately in three files they drift
//! apart, so they are answered here together, behind an interface that says
//! only "give me a chroot" and "here it is back".
//!
//! Two strategies live here. `DevtoolsCopy` is what devtools has always done:
//! copy the base chroot, build in the copy, delete it. `Overlay` mounts the
//! base read-only under a per-build upper layer, which copies nothing at all --
//! 15ms and a few hundred kilobytes against 810MB of rsync on a filesystem
//! without cheap snapshots. They differ in exactly those three answers and in
//! nothing else the rest of the worker sees.

use crate::chroot;
use anyhow::{Context, Result, bail};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// How a build's chroot copy is made and destroyed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    /// `makechrootpkg` makes the copy (`-c`) and deletes it (`-T`); the worker
    /// only names it.
    DevtoolsCopy,
    /// The worker mounts the base chroot read-only under a per-build upper
    /// layer, and `makechrootpkg` finds the copy already there. Nothing is
    /// copied, and the base cannot be written through the mount.
    Overlay,
}

impl Strategy {
    /// Whether giving a chroot back has any work to do.
    ///
    /// `false` here is why a dropped lease is not a leak today: devtools has
    /// already deleted the copy by the time the lease goes out of scope.
    const fn needs_release(self) -> bool {
        match self {
            Self::DevtoolsCopy => false,
            Self::Overlay => true,
        }
    }
}

/// Where per-build overlay layers live, beside the copies rather than inside
/// one: `is_stale_copy` matches `job-*`, so a name starting with a dot is
/// never mistaken for a chroot to sweep.
const OVERLAY_DIR: &str = ".overlay";

/// Published update layers, one directory per refresh, under [`OVERLAY_DIR`].
const UPDATES_DIR: &str = "updates";

/// A layer still being written. Publication is the rename that drops this, so
/// a refresh killed halfway leaves rubbish rather than a half-written layer in
/// the stack.
const PENDING_SUFFIX: &str = ".tmp";

/// How many layers to stack before flattening them back into the base.
///
/// Lookup cost grows with the stack and `lowerdir` lists are not unbounded --
/// Docker caps its equivalent at 128. Three refreshes a day was the measured
/// rate, so this is a fortnight of them.
const MAX_LAYERS: usize = 40;

/// Where stacking stops altogether.
///
/// Flattening needs a moment with nothing mounted, and a worker that never
/// idles never gets one -- so the soft cap alone bounds nothing. `lowerdir`
/// lists are not unbounded (Docker caps its equivalent at 128), and running
/// into that limit would fail a *build*, not a refresh. Refusing to stack
/// further leaves the chroot stale, which is the better failure.
const HARD_MAX_LAYERS: usize = 96;

/// Run one privileged command, failing with its output.
///
/// The chroot directory belongs to root and this worker deliberately does not,
/// which is the same reason every other chroot operation here goes through
/// `sudo`.
async fn sudo(args: &[&OsStr]) -> Result<()> {
    let mut cmd = tokio::process::Command::new("sudo");
    cmd.args(args);
    let (log, status) = chroot::run_capture(cmd).await?;
    if !status.success() {
        bail!("sudo {args:?} failed:\n{log}");
    }
    Ok(())
}

/// The `-o` argument for one build's overlay.
///
/// `None` if a path would need escaping. overlayfs separates its options with
/// commas and its lower layers with colons, escaped with backslashes -- and a
/// chroot directory containing either is so far outside what this deployment
/// looks like that falling back to copying is a better answer than getting the
/// escaping subtly wrong.
fn overlay_options(lower: &str, upper: &Path, work: &Path) -> Option<String> {
    let upper = upper.to_str()?;
    let work = work.to_str()?;
    if [lower, upper, work]
        .iter()
        .any(|p| p.contains(',') || p.contains('\\'))
        || upper.contains(':')
        || work.contains(':')
    {
        return None;
    }
    Some(format!("lowerdir={lower},upperdir={upper},workdir={work}"))
}

/// The name of the next layer, ordered so a plain directory listing sorts
/// oldest-first for as long as anyone will run this.
fn next_layer_name(published: &[PathBuf]) -> String {
    let highest = published
        .iter()
        .filter_map(|p| p.file_name()?.to_str()?.parse::<u32>().ok())
        .max()
        .unwrap_or(0);
    format!("{:04}", highest + 1)
}

/// The `lowerdir` list for a stack: newest layer first, the base last.
///
/// overlayfs reads lower layers left to right with the leftmost winning, so an
/// updated file in the newest layer shadows the one in the base.
fn lower_stack(root: &Path, published: &[PathBuf]) -> Option<String> {
    let mut parts = Vec::with_capacity(published.len() + 1);
    for dir in published
        .iter()
        .rev()
        .chain(std::iter::once(&root.to_path_buf()))
    {
        let text = dir.to_str()?;
        if text.contains(',') || text.contains(':') || text.contains('\\') {
            return None;
        }
        parts.push(text.to_string());
    }
    Some(parts.join(":"))
}

/// Mount points under `dir`, deepest first, from the contents of
/// `/proc/self/mountinfo`.
///
/// Deepest first because a build's chroot can hold mounts of its own if a
/// previous run died between `arch-nspawn` starting and finishing, and the
/// parent will not unmount while a child is still mounted under it.
fn mounts_under(mountinfo: &str, dir: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = mountinfo
        .lines()
        .filter_map(|line| line.split_whitespace().nth(4))
        // mountinfo escapes the few characters that would break the field
        // split; only the space matters for paths we create.
        .map(|point| PathBuf::from(point.replace("\\040", " ")))
        .filter(|point| point.starts_with(dir) && point != dir)
        .collect();
    found.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    found.dedup();
    found
}

/// Whether the base chroot is due another `pacman -Syu`.
///
/// `None` -- not refreshed since this worker started -- is always due: the
/// worker may have been down for a week. An interval of zero is "before every
/// build", which is what this did before it was measured.
fn refresh_due(since_last: Option<Duration>, interval: Duration) -> bool {
    since_last.is_none_or(|elapsed| elapsed >= interval)
}

/// What the operator asked for, before the machine gets a say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChrootMode {
    /// Decide at startup: overlay where a copy would be expensive, and where
    /// the kernel and filesystem actually allow one.
    Auto,
    /// Always try to mount overlays.
    Overlay,
    /// Always let devtools copy.
    Copy,
}

impl ChrootMode {
    /// Parse the `WORKER_CHROOT_OVERLAY` setting. Anything unrecognised is
    /// `Auto`, which is also the default: a typo should not silently turn off
    /// something the operator was trying to turn on.
    #[must_use]
    pub fn parse(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            Some("1" | "true" | "yes" | "on") => Self::Overlay,
            Some("0" | "false" | "no" | "off") => Self::Copy,
            _ => Self::Auto,
        }
    }
}

/// The chroots on this worker: one base, and a copy per build.
pub struct Chroots {
    dir: PathBuf,
    /// Resolved once by [`Chroots::detect`]. Unresolved means copying, which
    /// is what a worker did before any of this and is never wrong, only
    /// sometimes slow.
    strategy: std::sync::OnceLock<Strategy>,
    mode: ChrootMode,
    /// When the base chroot was last brought up to date, and the lock that
    /// serialises doing so. One lock for both, because "is it due" and "make
    /// it current" have to be one decision or two builds starting together
    /// both find it due.
    last_refresh: Mutex<Option<Instant>>,
    /// How long a refresh counts as current.
    interval: Duration,
    /// Whether the worker is currently refusing jobs to drain for a flatten.
    /// Only so the transition is logged once rather than at every poll.
    draining: AtomicBool,
}

impl Chroots {
    #[must_use]
    pub fn new(dir: PathBuf, interval: Duration, mode: ChrootMode) -> Self {
        Self {
            dir,
            strategy: std::sync::OnceLock::new(),
            mode,
            last_refresh: Mutex::new(None),
            interval,
            draining: AtomicBool::new(false),
        }
    }

    /// Whether a build can start, or the worker should drain first.
    ///
    /// Layers can only be flattened when nothing is mounted, and a worker with
    /// a full queue never reaches that on its own -- so at the hard cap it
    /// stops taking work until the builds in flight finish. Refusing to claim
    /// is what makes that safe: a *claimed* job holds a lease the server
    /// expects progress on, so stalling one that had already been accepted
    /// would risk it being taken for a dead worker, while a job left queued
    /// simply waits where everyone can see it.
    ///
    /// Cheap enough to ask before every claim: a `read_dir` of the layer
    /// directory, and the flatten below only tries once the cap is reached.
    pub async fn ready_for_work(&self) -> bool {
        if !matches!(self.strategy(), Strategy::Overlay) {
            return true;
        }
        let root = self.dir.join("root");
        if !root.exists() || self.layers().len() < HARD_MAX_LAYERS {
            if self.draining.swap(false, Ordering::Relaxed) {
                tracing::info!("chroot layers flattened; accepting builds again");
            }
            return true;
        }
        if !self.draining.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                "{} chroot layers: not claiming builds until the ones in flight \
                 finish and the layers can be flattened",
                self.layers().len()
            );
        }
        // Every poll, because the moment worth catching is the one just after
        // the last build ends.
        self.flatten_when_deep(&root).await;
        self.layers().len() < HARD_MAX_LAYERS
    }

    /// The published update layers, oldest first.
    ///
    /// Read from the directory rather than remembered, because a worker that
    /// restarts mid-life inherits whatever the last one published.
    fn layers(&self) -> Vec<PathBuf> {
        let dir = self.dir.join(OVERLAY_DIR).join(UPDATES_DIR);
        let mut found: Vec<PathBuf> = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.parse::<u32>().is_ok())
            })
            .collect();
        found.sort();
        found
    }

    /// Layers a refresh was killed partway through writing.
    fn layers_pending(&self) -> Vec<PathBuf> {
        let dir = self.dir.join(OVERLAY_DIR).join(UPDATES_DIR);
        std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.ends_with(PENDING_SUFFIX))
            })
            .collect()
    }

    /// How chroots will be made. Copying until [`Chroots::detect`] says
    /// otherwise.
    fn strategy(&self) -> Strategy {
        self.strategy
            .get()
            .copied()
            .unwrap_or(Strategy::DevtoolsCopy)
    }

    /// Decide once, at startup, how this worker makes chroots.
    ///
    /// Asking the machine rather than the operator, because the answer is a
    /// property of the machine: btrfs already copies a chroot for free by
    /// snapshotting it, and there an overlay would buy nothing while making
    /// refreshes wait for builds. Everywhere else a copy is a full `rsync` of
    /// the base -- 810MB per build on the ZFS worker this was measured on --
    /// and an overlay is worth having *if the kernel will mount one*, which
    /// only a real mount can answer: ZFS below 2.2 has no whiteouts, NFS never
    /// will, and a container may not be allowed to mount at all.
    pub async fn detect(&self) {
        let chosen = match self.mode {
            ChrootMode::Copy => Strategy::DevtoolsCopy,
            ChrootMode::Overlay => Strategy::Overlay,
            ChrootMode::Auto => {
                if chroot::is_btrfs(&self.dir) {
                    tracing::info!(
                        "chroot copies are btrfs snapshots on {}; not mounting overlays",
                        self.dir.display()
                    );
                    Strategy::DevtoolsCopy
                } else if let Err(e) = self.probe_overlay().await {
                    tracing::info!("copying chroots: an overlay could not be mounted ({e:#})");
                    Strategy::DevtoolsCopy
                } else {
                    tracing::info!(
                        "mounting overlay chroots: copies on {} are not cheap",
                        self.dir.display()
                    );
                    Strategy::Overlay
                }
            }
        };
        let _ = self.strategy.set(chosen);

        // Startup is the one moment a worker is reliably idle -- the sweep has
        // just unmounted anything a previous run left -- so it is the most
        // likely chance a busy worker ever gets to flatten. Restarts come with
        // upgrades, which makes this the recovery path for a worker that
        // stacked its way to the cap.
        if matches!(chosen, Strategy::Overlay) {
            let root = self.dir.join("root");
            if root.exists() {
                self.flatten_when_deep(&root).await;
            }
        }
    }

    /// Mount an overlay and take it straight down again, to find out whether
    /// this filesystem and this kernel will have one.
    /// With a lower layer of its own rather than the base chroot, which need
    /// not exist yet: a worker's first start has no chroot at all, and probing
    /// against a missing lower answers "no overlays here" for a reason that
    /// has nothing to do with the filesystem -- once, for the life of the
    /// process, on every fresh worker there is.
    async fn probe_overlay(&self) -> Result<()> {
        let layers = self.dir.join(OVERLAY_DIR).join(".probe");
        let lower = layers.join("lower");
        let upper = layers.join("upper");
        let work = layers.join("work");
        let merged = layers.join("merged");
        let options = lower
            .to_str()
            .and_then(|lower| overlay_options(lower, &upper, &work))
            .context("chroot paths cannot be expressed as overlay options")?;

        let outcome = async {
            sudo(&[
                "mkdir".as_ref(),
                "-p".as_ref(),
                lower.as_os_str(),
                upper.as_os_str(),
                work.as_os_str(),
                merged.as_os_str(),
            ])
            .await
            .context("preparing the probe directories")?;
            sudo(&[
                "mount".as_ref(),
                "-t".as_ref(),
                "overlay".as_ref(),
                "overlay".as_ref(),
                "-o".as_ref(),
                options.as_ref(),
                merged.as_os_str(),
            ])
            .await
            .context("mounting a probe overlay")
        }
        .await;

        if outcome.is_ok() {
            let _ = sudo(&["umount".as_ref(), merged.as_os_str()]).await;
        }
        let _ = sudo(&["rm".as_ref(), "-rf".as_ref(), layers.as_os_str()]).await;
        outcome
    }

    /// Ensure the base chroot exists and is reasonably fresh.
    ///
    /// Reasonably, not perfectly: the refresh costs ~13s and upgrades nothing
    /// on the large majority of builds, because Arch's repositories move a few
    /// times a day rather than a few times an hour. Paying it per build also
    /// serialised build starts behind each other, since only one may hold the
    /// chroot at a time.
    ///
    /// A chroot that does not exist yet is always made, whatever the interval
    /// says -- there is nothing to be stale.
    pub async fn refresh(&self, pacman_conf: &Path) -> Result<PathBuf> {
        if matches!(self.strategy(), Strategy::Overlay) {
            return self.refresh_by_layer(pacman_conf).await;
        }
        let root = self.dir.join("root");
        let mut last = self.last_refresh.lock().await;
        if root.exists() && !refresh_due(last.map(|at| at.elapsed()), self.interval) {
            tracing::debug!("base chroot is current; not refreshing");
            return Ok(root);
        }
        // Only while nothing else is using the base. A copy being taken must
        // not see it half-updated, and an overlay's lower layer must not change
        // at all while it is mounted -- and neither is worth waiting for here,
        // because this holds the lock every job start needs, and an overlay is
        // held for the length of a build. So a busy base means "later", not
        // "queue up behind it". `last` is deliberately not stamped, so the next
        // build tries again rather than waiting out the whole interval.
        if root.exists() {
            match chroot::try_lock_base(&root).await {
                chroot::BaseLock::Held(lock) => {
                    let root = chroot::ensure_base_chroot(&self.dir, pacman_conf).await?;
                    drop(lock);
                    *last = Some(Instant::now());
                    return Ok(root);
                }
                chroot::BaseLock::Busy => {
                    tracing::info!("base chroot is in use by a build; refreshing it later");
                    return Ok(root);
                }
                // No lock to be had. Refresh anyway: unlocked is how every
                // worker did this until recently.
                chroot::BaseLock::Unavailable => {}
            }
        }
        let root = chroot::ensure_base_chroot(&self.dir, pacman_conf).await?;
        *last = Some(Instant::now());
        Ok(root)
    }

    /// Take a chroot for one build. Give it back with [`Lease::release`].
    ///
    /// An overlay that cannot be mounted is not an error: the build falls back
    /// to devtools copying it, which is what every worker did until now. The
    /// warning repeats per build deliberately -- a filesystem that cannot carry
    /// an upper layer will not start being able to, and one line per build is
    /// how an operator notices they are paying for copies they did not expect.
    pub async fn acquire(&self, label: &str) -> Result<Lease> {
        if matches!(self.strategy(), Strategy::Overlay) {
            match self.mount_overlay(label).await {
                Ok(lock) => {
                    return Ok(Lease {
                        dir: self.dir.clone(),
                        label: label.to_string(),
                        strategy: Strategy::Overlay,
                        released: false,
                        lock,
                    });
                }
                Err(e) => tracing::warn!("no overlay chroot, copying instead: {e:#}"),
            }
        }
        Ok(Lease {
            dir: self.dir.clone(),
            label: label.to_string(),
            strategy: Strategy::DevtoolsCopy,
            released: false,
            lock: None,
        })
    }

    /// Mount one build's chroot: the base as a read-only lower layer, a
    /// directory of this build's own as the upper.
    ///
    /// Returns the *shared* lock on the base chroot, which the lease holds for
    /// as long as the mount exists. That is not the same promise the copying
    /// strategy makes: a copy is a point-in-time tree and the base may change
    /// underneath it freely, while a live overlay's lower must not change at
    /// all -- the kernel caches its dentries and inodes, and mutating it is
    /// undefined rather than merely visible. Holding the lock devtools already
    /// uses means a refresh waits for the builds using this base to finish.
    /// The cost is that a worker which never goes idle never refreshes; the
    /// answer to that is a new base per refresh, which is a larger change than
    /// this one.
    async fn mount_overlay(&self, label: &str) -> Result<Option<std::fs::File>> {
        let root = self.dir.join("root");
        let merged = self.dir.join(label);
        let layers = self.dir.join(OVERLAY_DIR).join(label);
        let upper = layers.join("upper");
        let work = layers.join("work");
        // The stack as it stands *now*: a refresh that publishes a layer after
        // this point belongs to the next build, not to one already mounted.
        let lower = lower_stack(&root, &self.layers())
            .context("chroot paths cannot be expressed as overlay options")?;
        let options = overlay_options(&lower, &upper, &work)
            .context("chroot paths cannot be expressed as overlay options")?;

        let lock = chroot::share_base_chroot(&root).await;
        sudo(&[
            "mkdir".as_ref(),
            "-p".as_ref(),
            upper.as_os_str(),
            work.as_os_str(),
            merged.as_os_str(),
        ])
        .await
        .context("preparing the overlay directories")?;
        sudo(&[
            "mount".as_ref(),
            "-t".as_ref(),
            "overlay".as_ref(),
            "overlay".as_ref(),
            "-o".as_ref(),
            options.as_ref(),
            merged.as_os_str(),
        ])
        .await
        .context("mounting the overlay")?;
        Ok(lock)
    }

    /// Run `body` with a chroot and give it back afterwards -- on the error
    /// path too, which is the half a caller forgets.
    pub async fn with_lease<T, F>(&self, label: &str, body: F) -> Result<T>
    where
        F: AsyncFnOnce(&Lease) -> Result<T>,
    {
        let lease = self.acquire(label).await?;
        let out = body(&lease).await;
        lease.release().await;
        out
    }

    /// Bring the chroot up to date by publishing a layer, not by rewriting the
    /// base.
    ///
    /// Nothing a running build is reading is touched: its stack was fixed when
    /// it mounted, and this only ever adds to the end. So unlike the in-place
    /// refresh there is no lock to take and nothing to wait for -- a busy
    /// worker refreshes on schedule rather than never. See
    /// `design/overlay-chroot.md`.
    async fn refresh_by_layer(&self, pacman_conf: &Path) -> Result<PathBuf> {
        let root = chroot::create_base_chroot(&self.dir, pacman_conf).await?;
        // Before anything else, and whether or not a refresh is due. A worker
        // at the hard cap has stopped stacking and needs this more than it
        // needs anything else -- doing it after the early returns below meant a
        // worker that reached the cap never flattened again, even once it went
        // idle, and stayed stale for good. Costs a `read_dir` when there is
        // nothing to do.
        self.flatten_when_deep(&root).await;

        let mut last = self.last_refresh.lock().await;
        if !refresh_due(last.map(|at| at.elapsed()), self.interval) {
            tracing::debug!("base chroot is current; not adding a layer");
            return Ok(root);
        }
        let published = self.layers().len();
        if published >= HARD_MAX_LAYERS {
            tracing::warn!(
                "{published} chroot layers and no idle moment to flatten them; \
                 not updating the chroot until there is one"
            );
            return Ok(root);
        }
        match self.add_update_layer(&root).await {
            // Stamped only on success, so a failure is retried by the next
            // build rather than waiting out the interval.
            Ok(()) => *last = Some(Instant::now()),
            Err(e) => tracing::warn!("could not update the chroot: {e:#}"),
        }
        drop(last);
        self.flatten_when_deep(&root).await;
        Ok(root)
    }

    /// Run `pacman -Syu` into a new layer and publish it.
    async fn add_update_layer(&self, root: &Path) -> Result<()> {
        let published = self.layers();
        let name = next_layer_name(&published);
        let updates = self.dir.join(OVERLAY_DIR).join(UPDATES_DIR);
        let pending = updates.join(format!("{name}{PENDING_SUFFIX}"));
        let work = updates.join(format!(".work-{name}"));
        let merged = updates.join(format!(".merged-{name}"));
        let lower = lower_stack(root, &published).context("chroot paths cannot be stacked")?;
        let options = overlay_options(&lower, &pending, &work)
            .context("chroot paths cannot be expressed as overlay options")?;

        sudo(&[
            "mkdir".as_ref(),
            "-p".as_ref(),
            pending.as_os_str(),
            work.as_os_str(),
            merged.as_os_str(),
        ])
        .await?;
        sudo(&[
            "mount".as_ref(),
            "-t".as_ref(),
            "overlay".as_ref(),
            "overlay".as_ref(),
            "-o".as_ref(),
            options.as_ref(),
            merged.as_os_str(),
        ])
        .await?;

        let mut cmd = chroot::devtools("arch-nspawn");
        cmd.arg(&merged).args(["pacman", "-Syu", "--noconfirm"]);
        let updated = chroot::run_capture(cmd).await;
        let _ = sudo(&["umount".as_ref(), merged.as_os_str()]).await;
        let _ = sudo(&[
            "rm".as_ref(),
            "-rf".as_ref(),
            work.as_os_str(),
            merged.as_os_str(),
        ])
        .await;

        match updated {
            Ok((_, status)) if status.success() => {}
            Ok((log, _)) => {
                let _ = sudo(&["rm".as_ref(), "-rf".as_ref(), pending.as_os_str()]).await;
                bail!("updating the chroot returned non-zero:\n{log}");
            }
            Err(e) => {
                let _ = sudo(&["rm".as_ref(), "-rf".as_ref(), pending.as_os_str()]).await;
                return Err(e);
            }
        }

        // Publication is the rename: until it lands, `layers` does not see a
        // layer that is still being written.
        let layer = updates.join(&name);
        sudo(&["mv".as_ref(), pending.as_os_str(), layer.as_os_str()]).await?;
        tracing::info!("published chroot update layer {name}");
        Ok(())
    }

    /// [`Chroots::flatten_if_deep`], with its failure reported rather than
    /// returned: flattening is maintenance, and a worker that cannot do it
    /// should carry on building with a deeper stack.
    async fn flatten_when_deep(&self, root: &Path) {
        if let Err(e) = self.flatten_if_deep(root).await {
            tracing::warn!("could not flatten the chroot layers: {e:#}");
        }
    }

    /// Merge the layers back into the base once there are enough of them.
    ///
    /// Only while nothing is mounted, and that is not a nicety: the layers
    /// being merged are the lower layers of any live build, and removing them
    /// under one is the undefined behaviour this whole design exists to avoid.
    /// A worker that is never idle keeps stacking, which is a warning rather
    /// than a failure.
    async fn flatten_if_deep(&self, root: &Path) -> Result<()> {
        let published = self.layers();
        if published.len() < MAX_LAYERS {
            return Ok(());
        }
        // Take the base exclusively for the whole swap. Checking that nothing
        // is mounted is not enough on its own: a build starting a moment later
        // would mount the stack this is about to delete, and the copy alone
        // takes long enough for that to be likely rather than theoretical.
        // `mount_overlay` takes the same lock shared, so a build that starts
        // here waits, and one already running keeps this waiting instead.
        let root_lock = match chroot::try_lock_base(root).await {
            chroot::BaseLock::Held(lock) => lock,
            chroot::BaseLock::Busy => {
                tracing::warn!(
                    "{} chroot layers and builds in flight; flattening when the worker is idle",
                    published.len()
                );
                return Ok(());
            }
            // Without a lock there is no way to keep a build out of the swap,
            // and stale layers are better than a build reading half of one.
            chroot::BaseLock::Unavailable => {
                tracing::warn!("cannot lock the base chroot; not flattening its layers");
                return Ok(());
            }
        };
        // Belt and braces: a leftover mount from a killed worker holds no lock.
        if !self.mounted_copies().is_empty() {
            tracing::warn!("chroot copies are still mounted; flattening later");
            return Ok(());
        }

        let staging = self.dir.join(OVERLAY_DIR).join(".flatten");
        let merged = staging.join("merged");
        let work = staging.join("work");
        let empty = staging.join("empty");
        let fresh = self.dir.join("root.new");
        let lower = lower_stack(root, &published).context("chroot paths cannot be stacked")?;
        let options =
            overlay_options(&lower, &empty, &work).context("cannot express the merged view")?;

        let _ = sudo(&["rm".as_ref(), "-rf".as_ref(), fresh.as_os_str()]).await;
        sudo(&[
            "mkdir".as_ref(),
            "-p".as_ref(),
            merged.as_os_str(),
            work.as_os_str(),
            empty.as_os_str(),
        ])
        .await?;
        sudo(&[
            "mount".as_ref(),
            "-t".as_ref(),
            "overlay".as_ref(),
            "overlay".as_ref(),
            "-o".as_ref(),
            options.as_ref(),
            merged.as_os_str(),
        ])
        .await?;

        let built = self.build_flattened(root, &merged, &fresh).await;
        let _ = sudo(&["umount".as_ref(), merged.as_os_str()]).await;
        let _ = sudo(&["rm".as_ref(), "-rf".as_ref(), staging.as_os_str()]).await;
        built?;

        // Two renames, and a crash between them leaves no `root` -- which the
        // next start treats as "no chroot yet" and rebuilds, slow but correct.
        let previous = self.dir.join("root.old");
        sudo(&["mv".as_ref(), root.as_os_str(), previous.as_os_str()]).await?;
        sudo(&["mv".as_ref(), fresh.as_os_str(), root.as_os_str()]).await?;
        let updates = self.dir.join(OVERLAY_DIR).join(UPDATES_DIR);
        let _ = sudo(&[
            "rm".as_ref(),
            "-rf".as_ref(),
            previous.as_os_str(),
            updates.as_os_str(),
        ])
        .await;
        drop(root_lock);
        tracing::info!("flattened {} chroot layers into the base", published.len());
        Ok(())
    }

    /// Materialise the merged view as a plain directory, writing only what
    /// differs from the base.
    ///
    /// Seeded with hardlinks, then rsynced from the merged view: unchanged
    /// files -- nearly all of them -- cost an inode and no data, and only the
    /// files the layers actually changed are written. A straight copy of the
    /// merged view costs the whole chroot instead, which on a filesystem
    /// without reflinks is 1.3 GB of writes to reproduce something almost
    /// identical to what is already there.
    ///
    /// Hardlinks are safe here for the reason they are unsafe for a chroot
    /// copy: rsync replaces a file by renaming a temporary over it, so a
    /// changed file breaks its link rather than being written through into the
    /// base. A *build* modifies files in place, which is why builds never get
    /// hardlinked chroots.
    ///
    /// Writing into `root` directly would be simpler still and is not allowed:
    /// it is a lower layer of the mount being read, and mutating a live lower
    /// is undefined however sound the bookkeeping looks.
    async fn build_flattened(&self, root: &Path, merged: &Path, fresh: &Path) -> Result<()> {
        let seeded = sudo(&[
            "cp".as_ref(),
            "-al".as_ref(),
            root.as_os_str(),
            fresh.as_os_str(),
        ])
        .await
        .is_ok();
        if !seeded {
            // btrfs refuses to link across subvolumes, and a base chroot is
            // one. Overlays are not chosen on btrfs, so this is a path for the
            // operator who set the mode by hand -- copy the whole view.
            tracing::info!("cannot hardlink the base chroot; copying it whole instead");
            let _ = sudo(&["rm".as_ref(), "-rf".as_ref(), fresh.as_os_str()]).await;
            sudo(&["mkdir".as_ref(), "-p".as_ref(), fresh.as_os_str()]).await?;
            return sudo(&[
                "cp".as_ref(),
                "-a".as_ref(),
                "--reflink=auto".as_ref(),
                format!("{}/.", merged.display()).as_ref(),
                fresh.as_os_str(),
            ])
            .await;
        }
        // `-X` because a chroot's binaries carry file capabilities, and a
        // flatten that dropped them would leave a subtly different chroot.
        sudo(&[
            "rsync".as_ref(),
            "-aX".as_ref(),
            "--delete".as_ref(),
            format!("{}/", merged.display()).as_ref(),
            format!("{}/", fresh.display()).as_ref(),
        ])
        .await
    }

    /// Chroot copies currently mounted, from `/proc/self/mountinfo`.
    fn mounted_copies(&self) -> Vec<PathBuf> {
        std::fs::read_to_string("/proc/self/mountinfo")
            .map(|info| mounts_under(&info, &self.dir))
            .unwrap_or_default()
    }

    /// Reclaim copies a previous run left behind. This is the only real
    /// guarantee: `release` and `Drop` both lose to `SIGKILL`.
    pub async fn sweep(&self) -> u64 {
        // Unmount before removing: a copy left mounted by a killed worker is a
        // directory `rm` cannot empty, and `--one-file-system` stops it from
        // trying -- so the copy would survive every sweep until a reboot.
        if let Ok(mountinfo) = std::fs::read_to_string("/proc/self/mountinfo") {
            for point in mounts_under(&mountinfo, &self.dir) {
                match sudo(&["umount".as_ref(), point.as_os_str()]).await {
                    Ok(()) => tracing::info!("unmounted stale {}", point.display()),
                    Err(e) => tracing::warn!("could not unmount {}: {e:#}", point.display()),
                }
            }
        }
        // Per-build layers are rubbish now; published update layers are not,
        // and neither is a base a flatten was halfway through swapping in.
        let overlay = self.dir.join(OVERLAY_DIR);
        if let Ok(entries) = std::fs::read_dir(&overlay) {
            for path in entries.flatten().map(|e| e.path()) {
                if path.file_name().is_some_and(|n| n == UPDATES_DIR) {
                    continue;
                }
                let _ = sudo(&["rm".as_ref(), "-rf".as_ref(), path.as_os_str()]).await;
            }
        }
        for path in [
            self.dir.join("root.new"),
            self.dir.join(OVERLAY_DIR).join(".flatten"),
        ] {
            if path.exists() {
                let _ = sudo(&["rm".as_ref(), "-rf".as_ref(), path.as_os_str()]).await;
            }
        }
        for pending in self.layers_pending() {
            let _ = sudo(&["rm".as_ref(), "-rf".as_ref(), pending.as_os_str()]).await;
        }
        chroot::remove_stale_copies(&self.dir).await
    }
}

/// One build's claim on a chroot.
pub struct Lease {
    dir: PathBuf,
    label: String,
    strategy: Strategy,
    released: bool,
    /// Shared lock on the base chroot, held for as long as an overlay uses it
    /// as a lower layer. `None` for a copy, which needs no such promise.
    lock: Option<std::fs::File>,
}

impl Lease {
    /// The `-r` argument: the directory the copy lives in.
    #[must_use]
    pub fn chroot_dir(&self) -> &Path {
        &self.dir
    }

    /// The `-l` argument: what this build's copy is called.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Whether `makechrootpkg` makes and deletes the copy itself.
    #[must_use]
    pub fn devtools_owns_copy(&self) -> bool {
        matches!(self.strategy, Strategy::DevtoolsCopy)
    }

    /// Give the chroot back.
    ///
    /// Awaited rather than left to `Drop`, for two reasons that outlive the
    /// current strategy: teardown can fail and the failure is worth reporting,
    /// and it has to happen *after* the build's child has been reaped --
    /// unmounting under a live process fails with `EBUSY`, and drop order is a
    /// poor place to state that dependency.
    pub async fn release(mut self) {
        self.released = true;
        if !matches!(self.strategy, Strategy::Overlay) {
            return;
        }
        let merged = self.dir.join(&self.label);
        // The child has been waited on by now -- `with_lease` sees to that --
        // so an `EBUSY` here means something outlived the build rather than
        // something still finishing, and retrying would not help.
        match sudo(&["umount".as_ref(), merged.as_os_str()]).await {
            Ok(()) => {
                let layers = self.dir.join(OVERLAY_DIR).join(&self.label);
                let lock = self.dir.join(format!("{}.lock", self.label));
                // `makechrootpkg` takes a lock beside the copy and, without
                // `-T`, leaves both behind for whoever made them.
                let _ = sudo(&[
                    "rm".as_ref(),
                    "-rf".as_ref(),
                    layers.as_os_str(),
                    lock.as_os_str(),
                ])
                .await;
                let _ = sudo(&["rmdir".as_ref(), merged.as_os_str()]).await;
            }
            Err(e) => tracing::warn!(
                "could not unmount {}: {e:#}; the next sweep reclaims it",
                merged.display()
            ),
        }
        // Only now: the lower must stay unchanged while anything is mounted on
        // it, and until the unmount above that included this build.
        drop(self.lock.take());
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        // A backstop for panics and early returns, in the shape `BuildCgroup`
        // and `BuildAgent` already use: best effort, never fatal, and never the
        // path anything relies on. Silent today, because devtools has already
        // deleted the copy.
        if !self.released && self.strategy.needs_release() {
            tracing::warn!(
                "chroot lease {} dropped without release; the next sweep reclaims it",
                self.label
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lease() -> Lease {
        Lease {
            dir: PathBuf::from("/var/lib/aurcache-chroot"),
            label: "job-42".to_string(),
            strategy: Strategy::DevtoolsCopy,
            released: false,
            lock: None,
        }
    }

    /// A lease speaks devtools -- `-r`, `-l`, and who owns the copy -- so that
    /// nothing above this module has to know what a copy actually is.
    #[test]
    fn a_lease_names_what_makechrootpkg_needs() {
        let lease = lease();
        assert_eq!(lease.chroot_dir(), Path::new("/var/lib/aurcache-chroot"));
        assert_eq!(lease.label(), "job-42");
        assert!(lease.devtools_owns_copy());
    }

    /// A worker that has stacked to the hard cap stops claiming, so the builds
    /// in flight can finish and the layers be flattened. Claiming and then
    /// stalling the build instead would put a lease at risk; a job left queued
    /// on the server costs nothing and is visible.
    #[tokio::test]
    async fn a_worker_at_the_cap_stops_taking_work() {
        let tmp = tempfile::tempdir().unwrap();
        let chroots = Chroots::new(
            tmp.path().to_path_buf(),
            Duration::ZERO,
            ChrootMode::Overlay,
        );
        chroots.strategy.set(Strategy::Overlay).unwrap();
        std::fs::create_dir_all(tmp.path().join("root")).unwrap();
        let updates = tmp.path().join(OVERLAY_DIR).join(UPDATES_DIR);
        std::fs::create_dir_all(&updates).unwrap();

        for n in 1..HARD_MAX_LAYERS {
            std::fs::create_dir(updates.join(format!("{n:04}"))).unwrap();
        }
        assert!(
            chroots.ready_for_work().await,
            "below the cap the worker keeps building"
        );

        std::fs::create_dir(updates.join(format!("{HARD_MAX_LAYERS:04}"))).unwrap();
        assert!(
            !chroots.ready_for_work().await,
            "at the cap it drains instead: there is no idle moment to flatten in otherwise"
        );
    }

    /// Copying workers never stack anything, so they never drain.
    #[tokio::test]
    async fn a_copying_worker_never_drains() {
        let tmp = tempfile::tempdir().unwrap();
        let chroots = Chroots::new(tmp.path().to_path_buf(), Duration::ZERO, ChrootMode::Copy);
        chroots.strategy.set(Strategy::DevtoolsCopy).unwrap();
        assert!(chroots.ready_for_work().await);
    }

    /// A typo should not quietly turn off something the operator was turning
    /// on -- anything unrecognised means "let the machine decide".
    #[test]
    fn an_unrecognised_mode_asks_the_machine() {
        assert_eq!(ChrootMode::parse(Some("1")), ChrootMode::Overlay);
        assert_eq!(ChrootMode::parse(Some(" yes ")), ChrootMode::Overlay);
        assert_eq!(ChrootMode::parse(Some("off")), ChrootMode::Copy);
        assert_eq!(ChrootMode::parse(None), ChrootMode::Auto);
        assert_eq!(ChrootMode::parse(Some("overlay")), ChrootMode::Auto);
    }

    /// An overlay names all three layers, and the base is the one it must not
    /// be able to write through.
    #[test]
    fn overlay_options_name_the_three_layers() {
        let options = overlay_options(
            "/chroot/root",
            Path::new("/chroot/.overlay/job-1/upper"),
            Path::new("/chroot/.overlay/job-1/work"),
        )
        .unwrap();
        assert_eq!(
            options,
            "lowerdir=/chroot/root,upperdir=/chroot/.overlay/job-1/upper,\
             workdir=/chroot/.overlay/job-1/work"
        );
    }

    /// overlayfs separates options with commas and layers with colons, so a
    /// path containing either would have to be escaped. Copying is a better
    /// answer than escaping it subtly wrongly.
    #[test]
    fn a_path_needing_escapes_declines_the_overlay() {
        assert!(
            overlay_options(
                "/chroot,odd/root",
                Path::new("/chroot/upper"),
                Path::new("/chroot/work"),
            )
            .is_none()
        );
    }

    /// Layers shadow the base and each other newest-first, which is how an
    /// upgraded package in the newest layer wins over the one `mkarchroot`
    /// installed.
    #[test]
    fn the_newest_layer_comes_first_and_the_base_last() {
        let published = vec![
            PathBuf::from("/chroot/.overlay/updates/0001"),
            PathBuf::from("/chroot/.overlay/updates/0002"),
        ];
        assert_eq!(
            lower_stack(Path::new("/chroot/root"), &published).unwrap(),
            "/chroot/.overlay/updates/0002:/chroot/.overlay/updates/0001:/chroot/root"
        );
        assert_eq!(
            lower_stack(Path::new("/chroot/root"), &[]).unwrap(),
            "/chroot/root"
        );
    }

    /// Names are numbered so a directory listing sorts oldest-first, and the
    /// next one continues from the highest published rather than the count --
    /// flattening removes layers, and reusing a name would put an old layer's
    /// contents in a new layer's place.
    #[test]
    fn the_next_layer_continues_from_the_highest() {
        assert_eq!(next_layer_name(&[]), "0001");
        assert_eq!(
            next_layer_name(&[
                PathBuf::from("/x/0001"),
                PathBuf::from("/x/0009"),
                PathBuf::from("/x/0002"),
            ]),
            "0010"
        );
    }

    /// A copy left mounted by a killed worker is a directory `rm` cannot
    /// empty, so the sweep has to unmount first -- deepest first, since a
    /// parent will not unmount while a child is mounted under it.
    #[test]
    fn stale_mounts_are_found_deepest_first() {
        let mountinfo = "\
26 25 0:22 / /var/lib/aurcache-chroot rw,relatime shared:2 - btrfs /dev/sdc rw
41 26 0:44 / /var/lib/aurcache-chroot/job-7 rw,relatime - overlay overlay rw
42 41 0:45 / /var/lib/aurcache-chroot/job-7/var/cache/pacman/pkg rw - none none rw
43 26 0:46 / /var/other/thing rw,relatime - overlay overlay rw
";
        let found = mounts_under(mountinfo, Path::new("/var/lib/aurcache-chroot"));
        assert_eq!(
            found,
            vec![
                PathBuf::from("/var/lib/aurcache-chroot/job-7/var/cache/pacman/pkg"),
                PathBuf::from("/var/lib/aurcache-chroot/job-7"),
            ],
            "the chroot directory itself and mounts elsewhere are not ours to unmount"
        );
    }

    /// An overlay is the worker's to take down; a copy is devtools'. Getting
    /// this backwards either leaks a mount or asks devtools to delete
    /// something it never made.
    #[test]
    fn who_owns_the_copy_decides_who_takes_it_down() {
        assert!(Strategy::Overlay.needs_release());
        assert!(!Strategy::DevtoolsCopy.needs_release());
    }

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

    /// Nothing to give back while devtools owns the copy, which is what makes
    /// dropping a lease harmless today rather than a leaked mount.
    #[test]
    fn a_devtools_copy_needs_no_release() {
        assert!(!Strategy::DevtoolsCopy.needs_release());
    }
}
