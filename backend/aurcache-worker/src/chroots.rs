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
fn overlay_options(lower: &Path, upper: &Path, work: &Path) -> Option<String> {
    let parts = [lower, upper, work]
        .map(|p| p.to_str().map(str::to_string))
        .into_iter()
        .collect::<Option<Vec<_>>>()?;
    if parts
        .iter()
        .any(|p| p.contains(',') || p.contains(':') || p.contains('\\'))
    {
        return None;
    }
    Some(format!(
        "lowerdir={},upperdir={},workdir={}",
        parts[0], parts[1], parts[2]
    ))
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
        }
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
        let options = overlay_options(&lower, &upper, &work)
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
        let options = overlay_options(&root, &upper, &work)
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
        let layers = self.dir.join(OVERLAY_DIR);
        if layers.exists() {
            let _ = sudo(&["rm".as_ref(), "-rf".as_ref(), layers.as_os_str()]).await;
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
            Path::new("/chroot/root"),
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
                Path::new("/chroot,odd/root"),
                Path::new("/chroot/upper"),
                Path::new("/chroot/work"),
            )
            .is_none()
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
