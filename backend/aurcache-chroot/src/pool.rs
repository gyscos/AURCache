//! The pool: a btrfs filesystem the worker owns, mounted in one place, with
//! simple quotas enabled and a total over everything in it.
//!
//! It is backed by whatever the operator can give it ([`Backing`]): an image
//! file (the default, needing nothing from the host), a block device such as a
//! zvol, or an existing dedicated btrfs mount. Only the image needs a loop
//! device.

use crate::cmd::{privileged, query};
use crate::qgroup::{QgroupId, Qgroups, TOTAL, Usage};
use crate::volumes::CacheVolumes;
use anyhow::{Context, Result, bail};
use std::collections::HashSet;
use std::ffi::OsStr;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};

/// The label `mkfs.btrfs` gives a pool, and what an existing device must carry
/// before it is used: a device whose filesystem this worker did not create is
/// never formatted and never adopted.
pub const LABEL: &str = "aurcache-pool";

/// Kept free under the pool's size, whatever the total: a btrfs filesystem
/// that is really full is the one state where deleting from it can fail, so
/// the total binds before the filesystem fills.
pub const MIN_MARGIN: u64 = 2 << 30;

/// The base chroot's subvolume, inside the pool. `makechrootpkg -r <pool>`
/// expects it under this name.
pub const ROOT: &str = "root";

/// Mount options every pool gets. `-m single` at mkfs time, and these at
/// mount: the host has its own redundancy, atime updates are writes nobody
/// reads, and compressing inside the pool means fewer bytes reach the host.
/// `commit=120` halves the flushes a nested filesystem sends its host.
const MOUNT_OPTIONS: &str = "noatime,compress=zstd:1,commit=120";

/// What backs the pool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Backing {
    /// An image file, loop-mounted. Sparse unless `reserve`, in which case it
    /// is allocated in full and mounted without `discard`, so its space stays
    /// the pool's.
    Image { path: PathBuf, reserve: bool },
    /// A block device -- a zvol, a partition -- formatted the first time.
    Device(PathBuf),
    /// An existing btrfs filesystem, mounted at the pool's mount point and
    /// dedicated to it: quotas apply filesystem-wide.
    Mount,
}

/// Who owns something the pool makes for the worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Owner {
    pub uid: u32,
    pub gid: u32,
}

impl Owner {
    /// The user this process runs as.
    #[must_use]
    pub fn current() -> Self {
        // SAFETY: neither call has preconditions or can fail.
        unsafe {
            Self {
                uid: libc::getuid(),
                gid: libc::getgid(),
            }
        }
    }

    /// `uid:gid`, as `chown` takes it.
    fn chown_spec(self) -> String {
        format!("{}:{}", self.uid, self.gid)
    }
}

/// How to open a pool.
#[derive(Clone, Debug)]
pub struct PoolConfig {
    pub backing: Backing,
    /// Where the pool is mounted: made here for an image or a device, and
    /// already there for [`Backing::Mount`].
    pub mountpoint: PathBuf,
    /// Everything the worker may store, in bytes (`WORKER_DISK_MAX`).
    pub total: u64,
    /// The worker's uid and gid: the owner of what it writes in the pool.
    pub owner: Owner,
}

/// The size an image or device needs for a total of `total`: the total plus
/// its margin.
#[must_use]
pub fn size_for(total: u64) -> u64 {
    total.saturating_add((total / 20).max(MIN_MARGIN))
}

/// An open, mounted pool.
#[derive(Debug)]
pub struct Pool {
    mountpoint: PathBuf,
    qgroups: Qgroups,
    owner: Owner,
    backing: Backing,
    /// The loop device, for an image.
    loop_device: Option<PathBuf>,
    /// Set when an image could not shrink to its total: the size it would
    /// have had. See [`Self::oversized`].
    oversized: std::sync::Mutex<Option<u64>>,
}

/// What [`Pool::set_total`] did to the pool's size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use]
pub enum Resize {
    /// The pool's size follows the total: it was grown or shrunk to it, or
    /// needed neither, or is a device or mount the pool does not size.
    Fits,
    /// An image holds more than fits in the size the total asks for, so it
    /// kept its size. The total still binds. Emptying the pool and making it
    /// again is the way down; see [`Pool::destroy`].
    TooFull {
        /// The image's size now.
        size: u64,
        /// The size the total asks for.
        wanted: u64,
    },
}

impl Pool {
    /// Mount the pool, creating it the first time, and make sure its quotas
    /// are on and its total is `config.total`.
    pub async fn open(config: PoolConfig) -> Result<Self> {
        let PoolConfig {
            backing,
            mountpoint,
            total,
            owner,
        } = config;
        let mut loop_device = None;
        match &backing {
            Backing::Mount => check_dedicated_mount(&mountpoint).await?,
            Backing::Image { path, reserve } => {
                if !is_mountpoint(&mountpoint) {
                    // Formatted and checked as a file: neither needs a loop
                    // device.
                    if prepare_image(path, size_for(total), *reserve).await? {
                        mkfs(path).await?;
                    } else {
                        check_ours(path).await?;
                    }
                    match attached_loop(path).await {
                        // Attached by an older worker, without autoclear.
                        Some(device) => {
                            mount(&device, &mountpoint, !reserve, false, owner).await?;
                        }
                        None => {
                            ensure_free_loop_node().await?;
                            mount(path, &mountpoint, !reserve, true, owner).await?;
                        }
                    }
                }
                loop_device = attached_loop(path).await;
                if let Some(device) = &loop_device {
                    enable_direct_io(device).await;
                }
            }
            Backing::Device(device) => {
                if !is_mountpoint(&mountpoint) {
                    check_device(device).await?;
                    match filesystem_type(device).await?.as_deref() {
                        None => mkfs(device).await?,
                        Some(_) => check_ours(device).await?,
                    }
                    mount(device, &mountpoint, true, false, owner).await?;
                }
            }
        }

        let fsid = fsid(&mountpoint).await?;
        let pool = Self {
            qgroups: Qgroups::for_fsid(&fsid),
            mountpoint,
            owner,
            backing,
            loop_device,
            oversized: std::sync::Mutex::new(None),
        };
        pool.enable_quotas().await?;
        // A lowered total the image cannot shrink to is remembered in
        // `oversized` for the caller; the pool opens either way.
        let _ = pool.set_total(total).await?;
        Ok(pool)
    }

    /// Where the pool is mounted: `makechrootpkg -r` takes this.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.mountpoint
    }

    /// The base chroot's path, whether or not it exists yet.
    #[must_use]
    pub fn root(&self) -> PathBuf {
        self.mountpoint.join(ROOT)
    }

    /// Everything stored in the pool, against `WORKER_DISK_MAX`.
    #[must_use]
    pub fn total_usage(&self) -> Option<Usage> {
        self.qgroups.usage(TOTAL)
    }

    /// A qgroup's usage.
    #[must_use]
    pub fn usage(&self, group: QgroupId) -> Option<Usage> {
        self.qgroups.usage(group)
    }

    /// The size an image would have for its total, when it could not shrink
    /// to it: what it holds did not fit. `None` when the size follows the
    /// total.
    #[must_use]
    pub fn oversized(&self) -> Option<u64> {
        *self.oversized.lock().expect("not poisoned")
    }

    /// Delete an image pool: unmount it, detach it, and remove the image with
    /// everything in it. [`Self::open`] then makes a new one. The way to
    /// shrink an image whose contents do not fit the smaller size.
    ///
    /// Refused for a device or an existing mount: those are the operator's,
    /// and never formatted twice by the worker.
    pub async fn destroy(self) -> Result<()> {
        let Backing::Image { path, .. } = self.backing.clone() else {
            bail!(
                "only an image pool is destroyed and made again; a device or a mount is the operator's"
            );
        };
        self.unmount().await?;
        std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        tracing::info!("pool image {} removed", path.display());
        Ok(())
    }

    /// Unmount the pool, and detach an image's loop device. An existing mount
    /// is the operator's, and is left mounted.
    pub async fn unmount(self) -> Result<()> {
        if matches!(self.backing, Backing::Mount) {
            return Ok(());
        }
        privileged(&["umount".as_ref(), self.mountpoint.as_os_str()])
            .await
            .with_context(|| format!("unmounting the pool at {}", self.mountpoint.display()))?;
        // Normally gone already: a device the mount attached autoclears. One
        // an older worker attached with `losetup` does not.
        if let Backing::Image { path, .. } = &self.backing
            && let Some(device) = attached_loop(path).await
        {
            privileged(&["losetup".as_ref(), "-d".as_ref(), device.as_os_str()]).await?;
        }
        Ok(())
    }

    async fn enable_quotas(&self) -> Result<()> {
        match self.qgroups.mode().as_deref() {
            Some("squota") => Ok(()),
            // Full qgroups on an existing mount: its operator turned them on,
            // and switching them to simple quotas throws their accounting
            // away. That is theirs to do.
            Some("qgroup") => bail!(
                "{} has full btrfs quotas enabled; the pool needs simple quotas. Run \
                 `btrfs quota disable {0}` and let the worker enable simple quotas",
                self.mountpoint.display()
            ),
            _ => {
                privileged(&[
                    "btrfs".as_ref(),
                    "quota".as_ref(),
                    "enable".as_ref(),
                    "--simple".as_ref(),
                    self.mountpoint.as_os_str(),
                ])
                .await
                .with_context(|| {
                    format!(
                        "enabling simple quotas on {} (kernel 6.7 or later)",
                        self.mountpoint.display()
                    )
                })?;
                Ok(())
            }
        }
    }

    /// Set the total everything in the pool may use, and size an image to
    /// follow it.
    ///
    /// The limit applies at once: it is the total's qgroup, and a write past
    /// it fails however much was written before. An image grows online. It
    /// also shrinks online when what the pool holds fits in the smaller size
    /// -- btrfs moves data out of the part being cut off first. When it does
    /// not fit, the image keeps its size and says so ([`Resize::TooFull`]);
    /// the total still binds.
    pub async fn set_total(&self, total: u64) -> Result<Resize> {
        if !self.qgroups.exists(TOTAL) {
            self.btrfs(&["qgroup", "create", &TOTAL.to_string()])
                .await?;
        }
        // Before any resize: nothing new may land in space about to go.
        self.btrfs(&["qgroup", "limit", &total.to_string(), &TOTAL.to_string()])
            .await?;
        *self.oversized.lock().expect("not poisoned") = None;
        let (Backing::Image { path, reserve }, Some(device)) = (&self.backing, &self.loop_device)
        else {
            return Ok(Resize::Fits);
        };
        let want = size_for(total);
        let have = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        if want > have {
            size_image(path, want, *reserve)?;
            privileged(&["losetup".as_ref(), "-c".as_ref(), device.as_os_str()]).await?;
            self.btrfs(&["filesystem", "resize", "max"]).await?;
            tracing::info!(
                "pool image grown to {} for a total of {}",
                gib(want),
                gib(total)
            );
        } else if want < have {
            // The filesystem first, then the file under it. The other order
            // would cut off whatever btrfs had stored at the end.
            match self
                .btrfs(&["filesystem", "resize", &want.to_string()])
                .await
            {
                Ok(()) => {
                    std::fs::OpenOptions::new()
                        .write(true)
                        .open(path)
                        .and_then(|f| f.set_len(want))
                        .with_context(|| format!("truncating {}", path.display()))?;
                    privileged(&["losetup".as_ref(), "-c".as_ref(), device.as_os_str()]).await?;
                    tracing::info!(
                        "pool image shrunk to {} for a total of {}",
                        gib(want),
                        gib(total)
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        "the pool image stays at {}: what it holds does not fit in {} ({e:#}). \
                         The total of {} applies regardless",
                        gib(have),
                        gib(want),
                        gib(total)
                    );
                    *self.oversized.lock().expect("not poisoned") = Some(want);
                    return Ok(Resize::TooFull {
                        size: have,
                        wanted: want,
                    });
                }
            }
        }
        Ok(Resize::Fits)
    }

    /// Bytes free on the filesystem holding a sparse image: what the pool can
    /// still grow into on the host. `None` when the pool's space is its own --
    /// a device, an existing mount, or a reserved image.
    #[must_use]
    pub fn host_free(&self) -> Option<u64> {
        let Backing::Image {
            path,
            reserve: false,
        } = &self.backing
        else {
            return None;
        };
        let dir = path.parent()?;
        let c_path = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()).ok()?;
        // SAFETY: `c_path` is NUL-terminated and outlives the call; `stat` is a
        // valid out-parameter.
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::statvfs(c_path.as_ptr(), &raw mut stat) };
        // `as`: the field widths differ between targets (32-bit armv7 among
        // them), so no single `From` fits all.
        (rc == 0).then(|| stat.f_bavail as u64 * stat.f_frsize as u64)
    }

    /// A long-lived subvolume at the pool's top, `name`, counted against the
    /// pool's total: the caches, which outlive any one build. Made the first
    /// time with `owner` and `mode`, and left as it is after that.
    ///
    /// `name` is one path component, never a path: what the pool holds and
    /// where is the pool's to decide.
    pub async fn ensure_subvolume(&self, name: &str, owner: Owner, mode: u32) -> Result<PathBuf> {
        if name.is_empty()
            || name == ROOT
            || build_of(name).is_some()
            || !name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            bail!("{name:?} is not a name the pool gives a subvolume");
        }
        let path = self.mountpoint.join(name);
        if path.exists() {
            return Ok(path);
        }
        self.btrfs_paths(&["subvolume", "create"], &[&path]).await?;
        let made = async {
            privileged(&[
                "chown".as_ref(),
                owner.chown_spec().as_ref(),
                path.as_os_str(),
            ])
            .await?;
            privileged(&[
                "chmod".as_ref(),
                format!("{mode:o}").as_ref(),
                path.as_os_str(),
            ])
            .await?;
            self.charge_to_total(&path).await
        };
        if let Err(e) = made.await {
            // Never leave one behind that is not counted: the next start would
            // find it and take it as it is.
            let _ = self.btrfs_paths(&["subvolume", "delete"], &[&path]).await;
            return Err(e).with_context(|| format!("preparing {}", path.display()));
        }
        Ok(path)
    }

    /// The entries of the cache subvolume `name` (see
    /// [`Self::ensure_subvolume`]), for the cache code to make, measure and
    /// remove as subvolumes of their own.
    #[must_use]
    pub fn cache_volumes(&self, name: &str) -> CacheVolumes {
        CacheVolumes::new(self.mountpoint.join(name), self.qgroups.clone())
    }

    /// Count the subvolume at `path` against the pool's total. What
    /// `mkarchroot` creates, and anything else made outside [`Self::lease`],
    /// is charged to its own level-0 group and nothing above it until this.
    pub async fn charge_to_total(&self, path: &Path) -> Result<()> {
        let id = subvolume_id(path).await?;
        self.assign(QgroupId::subvolume(id), TOTAL).await
    }

    /// Make the subvolumes one build runs in: a snapshot of the base chroot,
    /// and an empty one for its workdir, both in one group limited to `limit`
    /// under the pool's total.
    ///
    /// The snapshot is charged nothing for what it shares with the base; only
    /// what the build writes counts.
    pub async fn lease(&self, build_id: i32, limit: Option<u64>) -> Result<BuildVolumes> {
        let root = self.root();
        if !root.join("usr").exists() {
            bail!("the pool has no base chroot yet at {}", root.display());
        }
        let volumes = BuildVolumes {
            chroot: self.mountpoint.join(chroot_name(build_id)),
            data: self.mountpoint.join(data_name(build_id)),
            group: QgroupId::build(build_id),
            released: false,
        };
        // Leftovers under this id, from a crash between making these and
        // recording the build as running.
        self.remove_build(build_id).await;
        // And the groups earlier builds could not take with them.
        self.tidy_groups().await;

        let made = async {
            self.btrfs_paths(&["subvolume", "snapshot"], &[&root, &volumes.chroot])
                .await?;
            self.btrfs_paths(&["subvolume", "create"], &[&volumes.data])
                .await?;
            privileged(&[
                "chown".as_ref(),
                self.owner.chown_spec().as_ref(),
                volumes.data.as_os_str(),
            ])
            .await?;
            if !self.qgroups.exists(volumes.group) {
                self.btrfs(&["qgroup", "create", &volumes.group.to_string()])
                    .await?;
            }
            for subvolume in [&volumes.chroot, &volumes.data] {
                let id = subvolume_id(subvolume).await?;
                self.assign(QgroupId::subvolume(id), volumes.group).await?;
            }
            self.assign(volumes.group, TOTAL).await?;
            let limit = limit.map_or_else(|| "none".to_string(), |l| l.to_string());
            self.btrfs(&["qgroup", "limit", &limit, &volumes.group.to_string()])
                .await?;
            anyhow::Ok(())
        };
        if let Err(e) = made.await {
            self.remove_build(build_id).await;
            return Err(e).with_context(|| format!("preparing build {build_id}'s volumes"));
        }
        Ok(volumes)
    }

    /// Give a build's volumes back. See [`BuildVolumes::release`].
    pub async fn release(&self, mut volumes: BuildVolumes) {
        volumes.released = true;
        let build_id = volumes
            .group
            .build_id()
            .expect("a build's volumes carry a build's group");
        self.remove_build(build_id).await;
    }

    /// Remove every build's volumes except those in `keep`, and the groups
    /// they leave behind. Returns how many builds were cleared.
    ///
    /// What makes teardown reliable: `release` loses to `SIGKILL`.
    pub async fn sweep(&self, keep: &HashSet<i32>) -> usize {
        let mut builds: HashSet<i32> = std::fs::read_dir(&self.mountpoint)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| build_of(&e.file_name().to_string_lossy()))
            .collect();
        builds.extend(
            self.qgroups
                .list()
                .into_iter()
                .filter_map(QgroupId::build_id),
        );
        builds.retain(|id| !keep.contains(id));
        for &id in &builds {
            self.remove_build(id).await;
        }
        // A deleted subvolume's own group lingers while any extent it wrote is
        // still referenced, and as an empty `<stale>` group after.
        self.tidy_groups().await;
        let _ = self.btrfs(&["qgroup", "clear-stale"]).await;
        builds.len()
    }

    /// Destroy the groups of builds whose subvolumes are gone.
    ///
    /// A release deletes a build's subvolumes and then its group, but btrfs
    /// refuses to destroy the group while a deleted subvolume's own group
    /// still hangs off it -- which it does until the cleaner has freed that
    /// subvolume, in the background, well after the release returned. Once
    /// the cleaner is done the subvolume's group is `<stale>`, clearing those
    /// empties the build's group, and it can go. Run before each lease, so
    /// they are cleared as builds come and go rather than only at startup.
    async fn tidy_groups(&self) {
        let orphans: Vec<QgroupId> = self
            .qgroups
            .list()
            .into_iter()
            .filter(|group| {
                group.build_id().is_some_and(|id| {
                    !self.mountpoint.join(chroot_name(id)).exists()
                        && !self.mountpoint.join(data_name(id)).exists()
                })
            })
            .collect();
        if orphans.is_empty() {
            return;
        }
        let _ = self.btrfs(&["qgroup", "clear-stale"]).await;
        for group in orphans {
            // Still refused while the cleaner has not reached it; the next
            // lease tries again.
            if let Err(e) = self.btrfs(&["qgroup", "destroy", &group.to_string()]).await {
                tracing::debug!("qgroup {group} not destroyed yet: {e:#}");
            }
        }
    }

    /// Delete one build's subvolumes and group, whatever is left of them.
    async fn remove_build(&self, build_id: i32) {
        let subvolumes: Vec<PathBuf> = [chroot_name(build_id), data_name(build_id)]
            .into_iter()
            .map(|name| self.mountpoint.join(name))
            .filter(|path| path.exists())
            .collect();
        if !subvolumes.is_empty() {
            let mut args: Vec<&OsStr> = vec![
                "btrfs".as_ref(),
                "subvolume".as_ref(),
                "delete".as_ref(),
                // Anything the build made below its root goes too.
                "--recursive".as_ref(),
            ];
            args.extend(subvolumes.iter().map(|p| p.as_os_str()));
            if let Err(e) = privileged(&args).await {
                tracing::warn!("could not delete build {build_id}'s subvolumes: {e:#}");
            }
        }
        // `makechrootpkg` takes a lock beside the copy and leaves it.
        let lock = self
            .mountpoint
            .join(format!("{}.lock", chroot_name(build_id)));
        if lock.exists() {
            let _ = privileged(&["rm".as_ref(), "-f".as_ref(), lock.as_os_str()]).await;
        }
        let group = QgroupId::build(build_id);
        if self.qgroups.exists(group) {
            // Refused while a member still carries charge -- an extent the
            // build wrote that something else still references. The next
            // sweep tries again.
            if let Err(e) = self.btrfs(&["qgroup", "destroy", &group.to_string()]).await {
                tracing::debug!("qgroup {group} not destroyed yet: {e:#}");
            }
        }
    }

    async fn assign(&self, child: QgroupId, parent: QgroupId) -> Result<()> {
        // `--no-rescan`: simple quotas never rescan, and a rescan request is
        // an error on them.
        self.btrfs(&[
            "qgroup",
            "assign",
            "--no-rescan",
            &child.to_string(),
            &parent.to_string(),
        ])
        .await
    }

    /// `btrfs <args> <mountpoint>`, as root.
    async fn btrfs(&self, args: &[&str]) -> Result<()> {
        let mut argv: Vec<&OsStr> = vec!["btrfs".as_ref()];
        argv.extend(args.iter().map(OsStr::new));
        argv.push(self.mountpoint.as_os_str());
        privileged(&argv).await.map(drop)
    }

    /// `btrfs <args> <paths...>`, as root.
    async fn btrfs_paths(&self, args: &[&str], paths: &[&Path]) -> Result<()> {
        let mut argv: Vec<&OsStr> = vec!["btrfs".as_ref()];
        argv.extend(args.iter().map(OsStr::new));
        argv.extend(paths.iter().map(|p| p.as_os_str()));
        privileged(&argv).await.map(drop)
    }
}

/// The two subvolumes one build runs in, and the group that limits them.
///
/// Given back with [`Pool::release`]; one that is dropped instead is left for
/// [`Pool::sweep`].
#[derive(Debug)]
pub struct BuildVolumes {
    chroot: PathBuf,
    data: PathBuf,
    group: QgroupId,
    released: bool,
}

impl BuildVolumes {
    /// The build's chroot: a snapshot of the base. Its name is the
    /// `makechrootpkg -l` label.
    #[must_use]
    pub fn chroot(&self) -> &Path {
        &self.chroot
    }

    /// The `-l` label `makechrootpkg` knows the chroot by.
    #[must_use]
    pub fn label(&self) -> &str {
        self.chroot
            .file_name()
            .and_then(OsStr::to_str)
            .expect("named by chroot_name")
    }

    /// The build's own writable space: its workdir, owned by the worker.
    #[must_use]
    pub fn data(&self) -> &Path {
        &self.data
    }

    /// The group both are limited by.
    #[must_use]
    pub const fn group(&self) -> QgroupId {
        self.group
    }
}

impl Drop for BuildVolumes {
    fn drop(&mut self) {
        if !self.released {
            tracing::warn!(
                "{} dropped without release; the next sweep removes it",
                self.chroot.display()
            );
        }
    }
}

fn chroot_name(build_id: i32) -> String {
    format!("job-{build_id}")
}

fn data_name(build_id: i32) -> String {
    format!("job-{build_id}.data")
}

/// The build a pool entry belongs to: `job-<id>`, `job-<id>.data` and the lock
/// `makechrootpkg` leaves as `job-<id>.lock`.
fn build_of(name: &str) -> Option<i32> {
    let rest = name.strip_prefix("job-")?;
    let digits = rest
        .strip_suffix(".data")
        .or_else(|| rest.strip_suffix(".lock"))
        .unwrap_or(rest);
    digits.parse().ok().filter(|&id: &i32| id > 0)
}

fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / f64::from(1u32 << 30))
}

/// Whether something is mounted exactly at `path`.
fn is_mountpoint(path: &Path) -> bool {
    std::fs::read_to_string("/proc/self/mountinfo")
        .is_ok_and(|info| mount_points(&info).any(|point| point == path))
}

/// The mount points `/proc/self/mountinfo` lists, unescaped.
fn mount_points(mountinfo: &str) -> impl Iterator<Item = PathBuf> + '_ {
    mountinfo
        .lines()
        .filter_map(|line| line.split(' ').nth(4))
        .map(|field| PathBuf::from(unescape_mountinfo(field)))
}

/// mountinfo escapes space, tab, newline and backslash as `\ooo`.
fn unescape_mountinfo(field: &str) -> String {
    let mut out = String::with_capacity(field.len());
    let mut chars = field.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            let code: String = chars.by_ref().take(3).collect();
            match u8::from_str_radix(&code, 8) {
                Ok(byte) => out.push(char::from(byte)),
                Err(_) => {
                    out.push('\\');
                    out.push_str(&code);
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// An existing mount is only used if it is a whole btrfs filesystem mounted
/// right there: quotas apply to all of it.
async fn check_dedicated_mount(path: &Path) -> Result<()> {
    let out = query(&[
        "findmnt".as_ref(),
        "--mountpoint".as_ref(),
        path.as_os_str(),
        "--noheadings".as_ref(),
        "--output".as_ref(),
        "FSTYPE,FSROOT".as_ref(),
    ])
    .await
    .with_context(|| format!("{} is not a mount point", path.display()))?;
    let mut fields = out.split_whitespace();
    match (fields.next(), fields.next()) {
        (Some("btrfs"), Some("/")) => Ok(()),
        (Some("btrfs"), Some(root)) => bail!(
            "{} mounts the subvolume {root}, not a whole btrfs filesystem; quotas apply to \
             the whole filesystem, so the pool needs one to itself",
            path.display()
        ),
        (fstype, _) => bail!(
            "{} is {}, not btrfs",
            path.display(),
            fstype.unwrap_or("not mounted")
        ),
    }
}

async fn check_device(device: &Path) -> Result<()> {
    let meta = std::fs::metadata(device)
        .with_context(|| format!("the pool device {}", device.display()))?;
    if !meta.file_type().is_block_device() {
        bail!("{} is not a block device", device.display());
    }
    Ok(())
}

/// The filesystem on a device, `None` if it has none.
async fn filesystem_type(device: &Path) -> Result<Option<String>> {
    // `blkid` exits 2 when it finds nothing to report, which is the answer
    // "empty" rather than a failure.
    let out = crate::cmd::privileged(&[
        "sh".as_ref(),
        "-c".as_ref(),
        r#"blkid -o value -s TYPE "$1" || [ $? -eq 2 ]"#.as_ref(),
        "blkid".as_ref(),
        device.as_os_str(),
    ])
    .await?;
    let fstype = out.trim();
    Ok((!fstype.is_empty()).then(|| fstype.to_string()))
}

/// A device or image that already has a filesystem is used only if it is a
/// pool this worker made.
async fn check_ours(device: &Path) -> Result<()> {
    let fstype = filesystem_type(device).await?;
    let label = privileged(&[
        "blkid".as_ref(),
        "-o".as_ref(),
        "value".as_ref(),
        "-s".as_ref(),
        "LABEL".as_ref(),
        device.as_os_str(),
    ])
    .await
    .unwrap_or_default();
    match (fstype.as_deref(), label.trim()) {
        (Some("btrfs"), LABEL) => Ok(()),
        (fstype, label) => bail!(
            "{} already holds a {} filesystem labelled {label:?}, not an AURCache pool; \
             refusing to use or format it",
            device.display(),
            fstype.unwrap_or("unknown")
        ),
    }
}

async fn mkfs(device: &Path) -> Result<()> {
    privileged(&[
        "mkfs.btrfs".as_ref(),
        "--quiet".as_ref(),
        "--label".as_ref(),
        LABEL.as_ref(),
        "--metadata".as_ref(),
        "single".as_ref(),
        "--data".as_ref(),
        "single".as_ref(),
        device.as_os_str(),
    ])
    .await
    .with_context(|| format!("formatting {}", device.display()))?;
    tracing::info!("created a pool filesystem on {}", device.display());
    Ok(())
}

/// Mount `source` at `mountpoint`. With `via_loop`, `source` is an image
/// file, attached to a loop device by the mount itself.
///
/// Letting `mount` attach it, rather than `losetup` beforehand, is what sets
/// the loop device's autoclear flag: it detaches when the last mount of it
/// goes -- including when a container holding the mount is torn down, which
/// otherwise leaves the device attached to a deleted image for good, its
/// space held and its number taken.
async fn mount(
    source: &Path,
    mountpoint: &Path,
    discard: bool,
    via_loop: bool,
    owner: Owner,
) -> Result<()> {
    std::fs::create_dir_all(mountpoint)
        .with_context(|| format!("creating {}", mountpoint.display()))?;
    let options = mount_options(discard, via_loop);
    privileged(&[
        "mount".as_ref(),
        "-o".as_ref(),
        options.as_ref(),
        source.as_os_str(),
        mountpoint.as_os_str(),
    ])
    .await
    .with_context(|| format!("mounting the pool at {}", mountpoint.display()))?;
    // The pool's top directory belongs to the worker, so its own directories
    // need no root; subvolumes are still made with it.
    privileged(&[
        "chown".as_ref(),
        owner.chown_spec().as_ref(),
        mountpoint.as_os_str(),
    ])
    .await?;
    Ok(())
}

/// The options a pool is mounted with.
fn mount_options(discard: bool, via_loop: bool) -> String {
    let mut options = MOUNT_OPTIONS.to_string();
    // `discard=async` hands space the pool frees back to what holds it. Not
    // for a reserved image, whose point is keeping that space.
    if discard {
        options.push_str(",discard=async");
    }
    if via_loop {
        options.push_str(",loop");
    }
    options
}

/// The UUID of the filesystem mounted at `mountpoint`, which names its sysfs
/// directory.
async fn fsid(mountpoint: &Path) -> Result<String> {
    let out = query(&[
        "btrfs".as_ref(),
        "filesystem".as_ref(),
        "show".as_ref(),
        mountpoint.as_os_str(),
    ])
    .await?;
    parse_fsid(&out).with_context(|| format!("no filesystem UUID for {}", mountpoint.display()))
}

fn parse_fsid(show: &str) -> Option<String> {
    show.lines()
        .find_map(|line| line.split_once("uuid:"))
        .map(|(_, uuid)| uuid.trim().to_string())
}

/// A subvolume's id; its own qgroup is `0/<id>`.
async fn subvolume_id(path: &Path) -> Result<u64> {
    let out = query(&[
        "btrfs".as_ref(),
        "inspect-internal".as_ref(),
        "rootid".as_ref(),
        path.as_os_str(),
    ])
    .await?;
    out.trim()
        .parse()
        .with_context(|| format!("subvolume id of {}: {out:?}", path.display()))
}

/// Create the image if it is missing. Returns whether it was, and so needs
/// formatting.
async fn prepare_image(path: &Path, size: u64, reserve: bool) -> Result<bool> {
    if path.exists() {
        return Ok(false);
    }
    let dir = path.parent().context("the image path has no directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    // Before the file exists: on a btrfs host `+C` only takes on an empty
    // file, and a new file inherits it from its directory. Elsewhere the
    // attribute does not exist, which is fine.
    let _ = query(&["chattr".as_ref(), "+C".as_ref(), dir.as_os_str()]).await;
    size_image(path, size, reserve)?;
    Ok(true)
}

/// Make the image `size` bytes: sparse, or allocated in full when reserving.
fn size_image(path: &Path, size: u64, reserve: bool) -> Result<()> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    if reserve {
        use std::os::fd::AsRawFd;
        let len = libc::off_t::try_from(size).context("image size")?;
        // SAFETY: a valid descriptor and an in-range length; `fallocate` has
        // no other preconditions.
        let rc = unsafe { libc::fallocate(file.as_raw_fd(), 0, 0, len) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("reserving {} for {}", gib(size), path.display()));
        }
    } else {
        file.set_len(size)
            .with_context(|| format!("sizing {}", path.display()))?;
    }
    Ok(())
}

/// The loop device `path` is already attached to, after a worker restart.
async fn attached_loop(path: &Path) -> Option<PathBuf> {
    let out = privileged(&["losetup".as_ref(), "-j".as_ref(), path.as_os_str()])
        .await
        .ok()?;
    out.lines()
        .next()
        .and_then(|line| line.split_once(':'))
        .map(|(device, _)| PathBuf::from(device))
}

/// Make sure the loop device the kernel hands out next has a node.
///
/// Inside a container `/dev` only has the loop nodes that existed when it
/// started, and a mount asking for a new device would find no node for it.
/// `losetup --find` names that device -- with ` (lost)` after it when its node
/// is missing -- so the node is made first.
async fn ensure_free_loop_node() -> Result<()> {
    let out = privileged(&["losetup", "--find"])
        .await
        .context("finding a free loop device")?;
    let device =
        free_loop_device(&out).with_context(|| format!("unexpected `losetup --find`: {out:?}"))?;
    if !device.path.exists() {
        privileged(&[
            "mknod".as_ref(),
            device.path.as_os_str(),
            "b".as_ref(),
            "7".as_ref(),
            device.minor.to_string().as_ref(),
        ])
        .await
        .with_context(|| format!("creating {}", device.path.display()))?;
    }
    Ok(())
}

/// A loop device, by node path and minor number.
#[derive(Debug, PartialEq, Eq)]
struct LoopDevice {
    path: PathBuf,
    minor: u32,
}

/// The device `losetup --find` printed, ignoring the ` (lost)` it adds when
/// the node does not exist.
fn free_loop_device(out: &str) -> Option<LoopDevice> {
    let name = out.split_whitespace().next()?;
    let minor = name.strip_prefix("/dev/loop")?.parse().ok()?;
    Some(LoopDevice {
        path: PathBuf::from(name),
        minor,
    })
}

/// Switch a loop device to direct I/O, which keeps the image's data out of
/// the host's page cache, where it would sit twice. A host filesystem that
/// refuses it keeps buffered I/O, which works the same, only slower.
async fn enable_direct_io(device: &Path) {
    if let Err(e) = privileged(&[
        "losetup".as_ref(),
        "--direct-io=on".as_ref(),
        device.as_os_str(),
    ])
    .await
    {
        tracing::info!("direct I/O refused for the pool image ({e:#}); using buffered I/O");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Inside a container, `losetup --find` marks a device whose node is
    /// missing -- the one case the name has to survive.
    #[test]
    fn a_free_loop_device_is_read_with_or_without_its_node() {
        let device = |path: &str, minor| LoopDevice {
            path: PathBuf::from(path),
            minor,
        };
        assert_eq!(
            free_loop_device("/dev/loop5\n"),
            Some(device("/dev/loop5", 5))
        );
        assert_eq!(
            free_loop_device("/dev/loop5 (lost)\n"),
            Some(device("/dev/loop5", 5))
        );
        assert_eq!(free_loop_device(""), None);
        assert_eq!(free_loop_device("/dev/sda"), None);
    }

    #[test]
    fn a_pool_image_is_mounted_through_a_loop_device_of_its_own() {
        assert!(mount_options(true, true).ends_with(",discard=async,loop"));
        assert!(!mount_options(false, false).contains("discard"));
        assert!(!mount_options(false, false).contains("loop"));
    }

    #[test]
    fn the_margin_is_a_twentieth_but_never_under_two_gib() {
        assert_eq!(size_for(200 << 30), 210 << 30);
        assert_eq!(size_for(10 << 30), (10 << 30) + MIN_MARGIN);
    }

    #[test]
    fn pool_entries_name_their_build() {
        assert_eq!(build_of("job-42"), Some(42));
        assert_eq!(build_of("job-42.data"), Some(42));
        assert_eq!(build_of("job-42.lock"), Some(42));
        assert_eq!(build_of("root"), None);
        assert_eq!(build_of("root.lock"), None);
        assert_eq!(build_of("job-x"), None);
        assert_eq!(build_of("job-0"), None);
    }

    #[test]
    fn mountinfo_points_are_unescaped() {
        let info = "36 35 98:0 / /mnt/pool rw,noatime shared:1 - btrfs /dev/loop1 rw\n\
                    37 35 98:1 / /mnt/with\\040space rw - ext4 /dev/sda1 rw\n";
        let points: Vec<_> = mount_points(info).collect();
        assert_eq!(
            points,
            [PathBuf::from("/mnt/pool"), PathBuf::from("/mnt/with space")]
        );
    }

    #[test]
    fn the_fsid_comes_from_filesystem_show() {
        let show = "Label: 'aurcache-pool'  uuid: cbfa34ed-0092-4af9-bf6f-bab7c0c3025f\n\
                    \tTotal devices 1 FS bytes used 144.00KiB\n";
        assert_eq!(
            parse_fsid(show).as_deref(),
            Some("cbfa34ed-0092-4af9-bf6f-bab7c0c3025f")
        );
        assert_eq!(parse_fsid("nothing"), None);
    }
}
