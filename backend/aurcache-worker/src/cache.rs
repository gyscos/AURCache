//! Worker-local build caches (best-effort). Persisted under the cache volume
//! and bind-mounted into each `makechrootpkg` copy:
//!
//! * per-pkgbase `SRCDEST` — incremental git fetch / tarball reuse,
//! * a persistent `GNUPGHOME` — keys stay trusted across updates,
//! * the pacman package cache.
//!
//! Every accessor is graceful: a missing directory is simply created, and any
//! I/O error is logged and never propagated into the build.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Handle to the on-disk cache layout.
#[derive(Clone, Debug)]
pub struct Cache {
    root: PathBuf,
    max_size: u64,
    ttl: Duration,
    pkg_max_size: u64,
    pkg_ttl: Duration,
}

/// A per-pkgbase source cache entry considered for eviction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheEntry {
    pub pkgbase: String,
    pub size: u64,
    pub last_used: SystemTime,
}

impl Cache {
    pub fn new(
        root: &Path,
        max_size: u64,
        ttl_secs: u64,
        pkg_max_size: u64,
        pkg_ttl_secs: u64,
    ) -> Self {
        Self {
            root: root.to_path_buf(),
            max_size,
            ttl: Duration::from_secs(ttl_secs),
            pkg_max_size,
            pkg_ttl: Duration::from_secs(pkg_ttl_secs),
        }
    }

    /// `SRCDEST` for a pkgbase; created on demand. Returns `None` only if the
    /// directory truly cannot be created (build falls back to an ephemeral dir).
    pub fn srcdest(&self, pkgbase: &str) -> Option<PathBuf> {
        Self::ensured(self.root.join("srcdest").join(sanitize(pkgbase)))
    }

    /// Persistent build tree root for one architecture, bound over `/build`
    /// when a package asks to keep its tree between builds.
    ///
    /// Keyed by platform and *not* by package: makepkg already namespaces the
    /// tree as `$BUILDDIR/$pkgbase/src`, so keying by package here would nest
    /// the name twice. Platform must be in the key though -- a build tree holds
    /// compiled objects, unlike `srcdest`, whose downloads are architecture
    /// independent -- and it also keeps a native and an emulated build of one
    /// package from sharing a tree on the same worker.
    ///
    /// Only ever populated for packages that opted in, because the bind only
    /// happens for those; see `design/persistent-build-directory.md`.
    pub fn builddir(&self, platform: &str) -> Option<PathBuf> {
        Self::ensured(self.root.join("builddir").join(sanitize(platform)))
    }

    /// Name of the file caching a tree's measured size, written after each
    /// build so reclaim never has to walk one.
    const SIZE_STAMP: &'static str = ".aurcache-size";

    /// Record how big a package's persistent tree is, so reclaim can total the
    /// cache without walking it.
    ///
    /// Called once after a build, where the cost rides on top of something that
    /// already took minutes or hours. Measuring during reclaim instead would
    /// mean walking every candidate on every build, and these trees reach
    /// 130 GB and millions of files.
    pub fn record_builddir_size(&self, platform: &str, pkgbase: &str) {
        let Some(tree) = self.builddir(platform).map(|r| r.join(sanitize(pkgbase))) else {
            return;
        };
        if !tree.is_dir() {
            return;
        }
        let size = dir_size(&tree);
        if let Err(e) = std::fs::write(tree.join(Self::SIZE_STAMP), size.to_string()) {
            tracing::debug!("could not record size of {}: {e}", tree.display());
        }
    }

    /// Drop persistent build trees, least recently used first, until the cache
    /// is within `max_bytes` *and* the filesystem has `min_free` bytes spare.
    /// Never touches `keep`.
    ///
    /// **Nothing is evicted while the cache is within its limits.** Disk that
    /// nothing else needs is not worth reclaiming, and a tree kept is a rebuild
    /// avoided, so eviction is only ever a response to pressure -- never a
    /// tidy-up. It follows that a tree belonging to a deleted package is left
    /// alone until the space is actually wanted, which is the right time to
    /// notice: a worker is never told that a package went away, and being
    /// unused is the only evidence available.
    ///
    /// Least recently used, and deliberately nothing cleverer. Ranking by what
    /// a rebuild costs was tried and removed: the time a tree saves is roughly
    /// proportional to what it holds -- most of it is download the tree spares
    /// you -- so cost per byte comes out near constant and sorts nothing.
    /// Measured, it spans about 5x across packages as different as `libpng12`
    /// (17.9 MB, saves 8 s) and `unreal-engine` (130 GB, saves hours), against
    /// the 500x spread that *build duration* suggested. Duration was the wrong
    /// number: a warm `libpng12` build still took 11 s of the cold 19 s, so the
    /// tree saved 8 s and not 19.
    ///
    /// Two limits, because each is useless alone. A free-space floor does
    /// nothing on a large pool -- trees would grow into the terabytes before it
    /// triggered -- so `max_bytes` is what bounds the cache whatever the
    /// storage underneath it. A cap alone cannot see the rest of the machine,
    /// so the floor still covers a small disk, or one shared with something
    /// else that grew.
    ///
    /// Sizes come from the stamp each build leaves behind; a tree without one
    /// is walked once and stamped. So the usual case totals the cache by
    /// reading a handful of small files.
    ///
    /// Whole trees, never partial contents: half a tree is worse than none,
    /// because makepkg would treat it as resumable.
    ///
    /// Best-effort. Failing to reclaim is worth reporting and carrying on;
    /// refusing to build over it would turn a full disk into an idle worker.
    pub fn reclaim_builddirs(&self, platform: &str, keep: &str, max_bytes: u64, min_free: u64) {
        let Some(root) = self.builddir(platform) else {
            return;
        };

        // Oldest first. `mtime` on the tree root moves whenever a build writes
        // into it, which makes it a serviceable "last used" -- and a tree whose
        // package was deleted simply stops being touched, so it drifts to the
        // front on its own.
        let mut trees: Vec<(std::time::SystemTime, PathBuf, u64)> = std::fs::read_dir(&root)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let modified = e.metadata().ok()?.modified().ok()?;
                let path = e.path();
                let size = Self::stamped_size(&path);
                Some((modified, path, size))
            })
            .collect();
        trees.sort_by_key(|(modified, _, _)| *modified);

        let mut total: u64 = trees.iter().map(|(_, _, size)| size).sum();

        for (_, path, size) in trees {
            let over_budget = max_bytes > 0 && total > max_bytes;
            let short_of_free = min_free > 0 && free_bytes(&root).is_some_and(|f| f < min_free);
            if !over_budget && !short_of_free {
                return;
            }
            // The tree this build is about to use is the one thing worth
            // keeping, even when it is itself what breaches the budget.
            if path.file_name().is_some_and(|n| n == keep) {
                continue;
            }
            match std::fs::remove_dir_all(&path) {
                Ok(()) => {
                    total = total.saturating_sub(size);
                    tracing::info!(
                        "reclaimed persistent build tree {} ({size} bytes)",
                        path.display()
                    );
                }
                Err(e) => tracing::warn!("could not reclaim {}: {e}", path.display()),
            }
        }
    }

    /// A tree's recorded size, measuring and stamping it if no stamp is there.
    /// Tolerates a stamp carrying more than one field, from the brief time a
    /// rebuild cost was recorded alongside.
    fn stamped_size(tree: &Path) -> u64 {
        let stamp = tree.join(Self::SIZE_STAMP);
        if let Ok(text) = std::fs::read_to_string(&stamp)
            && let Some(Ok(size)) = text.split_whitespace().next().map(str::parse)
        {
            return size;
        }
        let size = dir_size(tree);
        let _ = std::fs::write(&stamp, size.to_string());
        size
    }

    /// Shared persistent GnuPG home for validpgpkeys.
    pub fn gnupg_home(&self) -> Option<PathBuf> {
        Self::ensured(self.root.join("gnupg"))
    }

    /// One build's private replica of that keyring, staged by the worker and
    /// written by nothing once the build starts.
    ///
    /// The build cannot read the shared keyring directly. Source signatures are
    /// verified on the worker, as the build user, while sibling jobs may be
    /// importing keys into it — and a keybox rewritten under a reader fails the
    /// verification whether or not gpg takes its dotlock. A replica has no
    /// writer for the life of the build, which no shared keyring can offer.
    ///
    /// Deliberately not group-writable, unlike every other directory here: the
    /// build only reads it, so leaving it read-only keeps a PKGBUILD out of the
    /// trust store its own sources are checked against.
    pub fn gnupg_job(&self, label: &str) -> Option<PathBuf> {
        Self::ensured_readable(self.root.join(format!("gnupg-{label}")))
    }

    /// Discard a job's keyring replica when its build ends.
    pub fn wipe_gnupg_job(&self, label: &str) {
        let path = self.root.join(format!("gnupg-{label}"));
        if let Err(e) = std::fs::remove_dir_all(&path)
            && path.exists()
        {
            tracing::warn!("could not wipe job keyring {}: {e}", path.display());
        }
    }

    /// Shared pacman package cache, bound **read-only** into every chroot as
    /// the second `CacheDir`, so builds get hits without being able to write.
    pub fn pacman_pkg(&self) -> Option<PathBuf> {
        Self::ensured(self.root.join("pacman-pkg"))
    }

    /// Private writable pacman cache for one job, bound over
    /// `/var/cache/pacman/pkg` so downloads cannot collide.
    ///
    /// Concurrent builds otherwise share one writable cache — `arch-nspawn`
    /// bind-mounts the host's first `CacheDir` read-write into every chroot —
    /// and two jobs downloading the same dependency race on the same
    /// `<pkg>.part` file, leaving a corrupt archive behind.
    pub fn pacman_pkg_job(&self, label: &str) -> Option<PathBuf> {
        Self::ensured(self.root.join(format!("pacman-pkg-{label}")))
    }

    /// Discard a job's private pacman cache once its packages are promoted.
    pub fn wipe_pacman_pkg_job(&self, label: &str) {
        let path = self.root.join(format!("pacman-pkg-{label}"));
        if let Err(e) = std::fs::remove_dir_all(&path)
            && path.exists()
        {
            tracing::warn!("could not wipe job pkg cache {}: {e}", path.display());
        }
    }

    /// Move a job's freshly downloaded packages into the shared cache so the
    /// next build gets them as hits.
    ///
    /// `rename` within one filesystem is atomic, so a concurrent build reading
    /// the shared cache sees either the old file or the complete new one, never
    /// a partial write — which is the whole failure this design exists to
    /// avoid. Both directories live under the cache root, so the same-filesystem
    /// requirement holds.
    ///
    /// Runs only after `makechrootpkg` has exited. Promoting mid-build would
    /// move a package out from under the running pacman; a hard link would be
    /// needed instead, and there is no reason to promote early.
    pub fn promote_job_pkgs(&self, label: &str) -> usize {
        let (Some(shared), Some(job)) = (self.pacman_pkg(), self.pacman_pkg_job(label)) else {
            return 0;
        };
        let Ok(read) = std::fs::read_dir(&job) else {
            return 0;
        };
        let mut promoted = 0;
        for entry in read.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            // Only finished artifacts: `.part` files are interrupted downloads.
            if !is_package_artifact(name) {
                continue;
            }
            match std::fs::rename(entry.path(), shared.join(name)) {
                Ok(()) => promoted += 1,
                Err(e) => tracing::warn!("could not promote {name} to the shared cache: {e}"),
            }
        }
        promoted
    }

    /// Evict shared-cache packages over the package budget.
    ///
    /// Deliberately a separate pool from `SRCDEST`: the two differ by orders of
    /// magnitude in size and in refill cost, and one shared budget would let a
    /// few large VCS checkouts starve the package cache — every build then
    /// re-downloading its dependencies — or a big package cache evict sources
    /// that are expensive to re-clone.
    ///
    /// No `in_use` set is needed. Unlinking a file another chroot has open is
    /// safe on POSIX: the reader keeps its descriptor, and the worst case is a
    /// re-download.
    pub fn evict_pkgs(&self) -> Vec<String> {
        let Some(dir) = self.pacman_pkg() else {
            return Vec::new();
        };
        let entries = scan_pkgcache(&dir);
        let plan = plan_eviction(
            &entries,
            self.pkg_max_size,
            self.pkg_ttl,
            SystemTime::now(),
            &[],
        );
        for name in &plan {
            if let Err(e) = std::fs::remove_file(dir.join(name)) {
                tracing::warn!("could not evict cached package {name}: {e}");
            }
            // Detached signatures follow their package.
            let _ = std::fs::remove_file(dir.join(format!("{name}.sig")));
        }
        plan
    }

    /// Create a cache directory, group-writable.
    ///
    /// These directories are created by the worker but written by *builds*,
    /// which run as a different user (see `Config::build_user`). The parent is
    /// setgid so the group is inherited, but the mode is not: with a default
    /// umask the new directory would be `rwxr-xr-x` and owned by the worker,
    /// and devtools would fail with "You do not have write permission for the
    /// directory $SRCDEST". Group write is what bridges the two users.
    /// Like [`Self::ensured`], but for a directory the build user reads rather
    /// than writes: `0755` instead of group-writable.
    fn ensured_readable(path: PathBuf) -> Option<PathBuf> {
        match std::fs::create_dir_all(&path) {
            Ok(()) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if let Err(e) =
                        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                    {
                        tracing::debug!("could not set mode on {}: {e}", path.display());
                    }
                }
                Some(path)
            }
            Err(e) => {
                tracing::warn!("cache dir {} unavailable: {e}", path.display());
                None
            }
        }
    }

    fn ensured(path: PathBuf) -> Option<PathBuf> {
        match std::fs::create_dir_all(&path) {
            Ok(()) => {
                Self::make_group_writable(&path);
                Some(path)
            }
            Err(e) => {
                tracing::warn!("cache dir {} unavailable: {e}", path.display());
                None
            }
        }
    }

    /// Best-effort `chmod g+rwx`. A failure is not fatal on its own: the
    /// directory may already be owned by the build user, in which case it is
    /// writable anyway.
    fn make_group_writable(path: &Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let Ok(meta) = std::fs::metadata(path) else {
                return;
            };
            let mode = meta.permissions().mode();
            let wanted = mode | 0o070;
            if mode != wanted
                && let Err(e) =
                    std::fs::set_permissions(path, std::fs::Permissions::from_mode(wanted))
            {
                tracing::debug!("could not make {} group-writable: {e}", path.display());
            }
        }
    }

    /// Wipe a possibly-corrupt pkgbase source cache so the next build starts
    /// cold (self-heal). Best-effort.
    pub fn wipe_srcdest(&self, pkgbase: &str) {
        let path = self.root.join("srcdest").join(sanitize(pkgbase));
        if let Err(e) = std::fs::remove_dir_all(&path)
            && path.exists()
        {
            tracing::warn!("could not wipe cache {}: {e}", path.display());
        }
    }

    /// Evict LRU source-cache entries above the size/TTL budget, skipping any
    /// pkgbase currently in use. Returns the pkgbases evicted.
    pub fn evict(&self, in_use: &[String]) -> Vec<String> {
        let entries = self.scan_srcdest();
        let plan = plan_eviction(&entries, self.max_size, self.ttl, SystemTime::now(), in_use);
        for pkgbase in &plan {
            self.wipe_srcdest(pkgbase);
            tracing::info!("evicted cache entry {pkgbase}");
        }
        plan
    }

    /// Best-effort scan of the source cache directory; an unreadable or
    /// missing directory yields no entries rather than an error.
    fn scan_srcdest(&self) -> Vec<CacheEntry> {
        let dir = self.root.join("srcdest");
        let mut entries = Vec::new();
        let Ok(read) = std::fs::read_dir(&dir) else {
            return entries;
        };
        for ent in read.flatten() {
            if !ent.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let pkgbase = ent.file_name().to_string_lossy().into_owned();
            let path = ent.path();
            let size = aurcache_common::fs::dir_size(&path);
            let last_used = ent
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            entries.push(CacheEntry {
                pkgbase,
                size,
                last_used,
            });
        }
        entries
    }
}

/// True for a finished package archive (not an in-flight `.part` download,
/// and not a detached signature, which is evicted with its package).
fn is_package_artifact(name: &str) -> bool {
    name.contains(".pkg.tar") && !name.ends_with(".part") && !name.ends_with(".sig")
}

/// One cache entry per package file. `last_used` is the file's mtime, which for
/// a package is its *download* time — pacman does not touch it on a cache hit.
/// That is why the package pool defaults to size-bounded only: age would evict
/// a package used daily just for having been fetched a while ago.
fn scan_pkgcache(dir: &Path) -> Vec<CacheEntry> {
    let mut entries = Vec::new();
    let Ok(read) = std::fs::read_dir(dir) else {
        return entries;
    };
    for ent in read.flatten() {
        let name = ent.file_name().to_string_lossy().into_owned();
        if !is_package_artifact(&name) {
            continue;
        }
        let Ok(md) = ent.metadata() else { continue };
        entries.push(CacheEntry {
            pkgbase: name,
            size: md.len(),
            last_used: md.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        });
    }
    entries
}

/// Replace path-unsafe characters so a pkgbase maps to a single directory.
fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Decide which entries to evict: everything older than `ttl` (when enabled),
/// then LRU entries until the total fits `max_size` (when enabled). Entries in
/// `in_use` are never evicted.
pub fn plan_eviction(
    entries: &[CacheEntry],
    max_size: u64,
    ttl: Duration,
    now: SystemTime,
    in_use: &[String],
) -> Vec<String> {
    let mut evict = Vec::new();
    let mut kept: Vec<&CacheEntry> = Vec::new();

    // Age-based eviction first.
    for e in entries {
        if in_use.contains(&e.pkgbase) {
            kept.push(e);
            continue;
        }
        let aged = !ttl.is_zero() && now.duration_since(e.last_used).is_ok_and(|age| age > ttl);
        if aged {
            evict.push(e.pkgbase.clone());
        } else {
            kept.push(e);
        }
    }

    // Size-based eviction: drop LRU survivors until within budget.
    if max_size > 0 {
        let mut total: u64 = kept.iter().map(|e| e.size).sum();
        // Oldest first.
        kept.sort_by_key(|e| e.last_used);
        for e in kept {
            if total <= max_size {
                break;
            }
            if !in_use.contains(&e.pkgbase) {
                evict.push(e.pkgbase.clone());
                total = total.saturating_sub(e.size);
            }
        }
    }
    evict
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(secs_ago: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 - secs_ago)
    }
    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000)
    }

    #[test]
    fn evicts_aged_entries() {
        let entries = vec![
            CacheEntry {
                pkgbase: "old".into(),
                size: 10,
                last_used: t(100),
            },
            CacheEntry {
                pkgbase: "fresh".into(),
                size: 10,
                last_used: t(1),
            },
        ];
        let evict = plan_eviction(&entries, 0, Duration::from_secs(50), now(), &[]);
        assert_eq!(evict, vec!["old".to_string()]);
    }

    #[test]
    fn evicts_lru_over_size_budget() {
        let entries = vec![
            CacheEntry {
                pkgbase: "a".into(),
                size: 100,
                last_used: t(30),
            },
            CacheEntry {
                pkgbase: "b".into(),
                size: 100,
                last_used: t(20),
            },
            CacheEntry {
                pkgbase: "c".into(),
                size: 100,
                last_used: t(10),
            },
        ];
        // Budget 250 → must drop the oldest (a).
        let evict = plan_eviction(&entries, 250, Duration::ZERO, now(), &[]);
        assert_eq!(evict, vec!["a".to_string()]);
    }

    #[test]
    fn never_evicts_in_use() {
        let entries = vec![CacheEntry {
            pkgbase: "busy".into(),
            size: 1000,
            last_used: t(999),
        }];
        let evict = plan_eviction(
            &entries,
            10,
            Duration::from_secs(1),
            now(),
            &["busy".to_string()],
        );
        assert!(evict.is_empty());
    }

    #[test]
    fn disabled_budgets_evict_nothing() {
        let entries = vec![CacheEntry {
            pkgbase: "a".into(),
            size: 10_000,
            last_used: t(10_000),
        }];
        assert!(plan_eviction(&entries, 0, Duration::ZERO, now(), &[]).is_empty());
    }

    #[test]
    fn sanitizes_pkgbase() {
        assert_eq!(sanitize("ttf-google-fonts-git"), "ttf-google-fonts-git");
        assert_eq!(sanitize("../etc/passwd"), ".._etc_passwd");
        assert_eq!(sanitize("a/b"), "a_b");
    }
}

#[cfg(test)]
mod pkgcache_tests {
    use super::*;

    fn cache(dir: &Path, pkg_max: u64) -> Cache {
        Cache::new(dir, 0, 0, pkg_max, 0)
    }

    fn write(path: &Path, bytes: usize) {
        std::fs::write(path, vec![b'x'; bytes]).unwrap();
    }

    /// Every other cache directory is group-writable so the build user can
    /// write it. This one must not be: the build reads its own trust store and
    /// has no business rewriting it.
    #[test]
    fn a_keyring_replica_is_not_writable_by_the_build() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let c = cache(tmp.path(), 0);
        let replica = c.gnupg_job("job-7").unwrap();
        let shared = c.gnupg_home().unwrap();

        let mode = std::fs::metadata(&replica).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o022,
            0,
            "the replica must not be writable: {mode:o}"
        );
        assert_ne!(
            std::fs::metadata(&shared).unwrap().permissions().mode() & 0o020,
            0,
            "the shared keyring stays group-writable for the worker"
        );
    }

    /// A replica belongs to one build and goes away with it.
    #[test]
    fn a_keyring_replica_is_wiped_with_its_job() {
        let tmp = tempfile::tempdir().unwrap();
        let c = cache(tmp.path(), 0);
        let replica = c.gnupg_job("job-7").unwrap();
        std::fs::write(replica.join("pubring.kbx"), b"keys").unwrap();

        c.wipe_gnupg_job("job-7");

        assert!(!replica.exists());
        assert!(
            c.gnupg_home().unwrap().exists(),
            "wiping a replica must not touch the shared keyring"
        );
    }

    /// Least recently used goes first, and the tree the build is about to use
    /// is never evicted -- even when it is itself what breaches the cap, since
    /// dropping it would defeat the point of keeping trees at all.
    #[test]
    fn reclaim_evicts_oldest_but_never_the_one_in_use() {
        use std::time::{Duration, SystemTime};

        let tmp = tempfile::tempdir().unwrap();
        let c = cache(tmp.path(), 0);
        let root = c.builddir("x86_64").unwrap();

        for (name, age) in [("old", 300), ("mid", 200), ("wanted", 100)] {
            let tree = root.join(name);
            std::fs::create_dir_all(&tree).unwrap();
            std::fs::write(tree.join(Cache::SIZE_STAMP), "100").unwrap();
            let when = SystemTime::now() - Duration::from_secs(age);
            std::fs::File::open(&tree)
                .unwrap()
                .set_modified(when)
                .unwrap();
        }

        // Room for one tree, so two must go -- but "wanted" is in use.
        c.reclaim_builddirs("x86_64", "wanted", 150, 0);

        assert!(!root.join("old").exists(), "oldest goes first");
        assert!(!root.join("mid").exists(), "then the next oldest");
        assert!(
            root.join("wanted").exists(),
            "the tree this build needs must survive"
        );
    }

    /// Room to spare means nothing is touched, however old. Reclaiming disk
    /// nothing else wants would trade a rebuild for no gain -- and it is what
    /// makes the tree of a deleted package cost nothing until the space is
    /// actually needed.
    #[test]
    fn nothing_is_evicted_while_there_is_room() {
        use std::time::{Duration, SystemTime};

        let tmp = tempfile::tempdir().unwrap();
        let c = cache(tmp.path(), 0);
        let root = c.builddir("x86_64").unwrap();

        let tree = root.join("ancient");
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::write(tree.join(Cache::SIZE_STAMP), "100").unwrap();
        let when = SystemTime::now() - Duration::from_secs(365 * 24 * 60 * 60);
        std::fs::File::open(&tree)
            .unwrap()
            .set_modified(when)
            .unwrap();

        // A cap nothing comes close to.
        c.reclaim_builddirs("x86_64", "none", u64::MAX, 0);

        assert!(
            tree.exists(),
            "an unused tree is not worth reclaiming while there is room for it"
        );
    }

    /// A tree with no stamp is measured rather than counted as free.
    #[test]
    fn an_unstamped_tree_is_measured() {
        let tmp = tempfile::tempdir().unwrap();
        let c = cache(tmp.path(), 0);
        let root = c.builddir("x86_64").unwrap();
        let tree = root.join("unstamped");
        std::fs::create_dir_all(tree.join("deep")).unwrap();
        write(&tree.join("deep").join("blob"), 4096);

        assert_eq!(Cache::stamped_size(&tree), 4096);
        // And it is stamped, so the walk happens once.
        assert_eq!(
            std::fs::read_to_string(tree.join(Cache::SIZE_STAMP)).unwrap(),
            "4096"
        );
    }

    #[test]
    fn only_finished_archives_are_promoted() {
        let tmp = tempfile::tempdir().unwrap();
        let c = cache(tmp.path(), 0);
        let job = c.pacman_pkg_job("job-1").unwrap();
        write(&job.join("foo-1.0-1-x86_64.pkg.tar.zst"), 10);
        // An interrupted download must not be published as a real package.
        write(&job.join("bar-2.0-1-x86_64.pkg.tar.zst.part"), 10);

        assert_eq!(c.promote_job_pkgs("job-1"), 1);
        let shared = c.pacman_pkg().unwrap();
        assert!(shared.join("foo-1.0-1-x86_64.pkg.tar.zst").exists());
        assert!(!shared.join("bar-2.0-1-x86_64.pkg.tar.zst.part").exists());
        // The partial stays behind and goes with the job directory.
        assert!(job.join("bar-2.0-1-x86_64.pkg.tar.zst.part").exists());
    }

    /// Two jobs downloading the same dependency both promote it. The second
    /// overwrites the first with identical content; `rename` makes that atomic,
    /// so no reader can observe a half-written file.
    #[test]
    fn promoting_the_same_package_twice_is_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let c = cache(tmp.path(), 0);
        for label in ["job-1", "job-2"] {
            let job = c.pacman_pkg_job(label).unwrap();
            write(&job.join("cmake-1.0-1-x86_64.pkg.tar.zst"), 10);
            assert_eq!(c.promote_job_pkgs(label), 1);
        }
        assert!(
            c.pacman_pkg()
                .unwrap()
                .join("cmake-1.0-1-x86_64.pkg.tar.zst")
                .exists()
        );
    }

    #[test]
    fn evicts_packages_over_the_package_budget() {
        let tmp = tempfile::tempdir().unwrap();
        let c = cache(tmp.path(), 150);
        let shared = c.pacman_pkg().unwrap();
        write(&shared.join("a-1.0-1-x86_64.pkg.tar.zst"), 100);
        write(&shared.join("b-1.0-1-x86_64.pkg.tar.zst"), 100);

        let evicted = c.evict_pkgs();
        assert_eq!(evicted.len(), 1, "one package should be dropped");
    }

    /// The package budget is separate from the source budget on purpose: a
    /// shared one lets large VCS checkouts starve the package cache.
    #[test]
    fn source_budget_does_not_evict_packages() {
        let tmp = tempfile::tempdir().unwrap();
        // Source budget of 1 byte, package budget generous.
        let c = Cache::new(tmp.path(), 1, 0, 10_000, 0);
        let shared = c.pacman_pkg().unwrap();
        write(&shared.join("a-1.0-1-x86_64.pkg.tar.zst"), 100);

        assert!(c.evict_pkgs().is_empty());
        assert!(shared.join("a-1.0-1-x86_64.pkg.tar.zst").exists());
    }

    #[test]
    fn signatures_follow_their_package() {
        let tmp = tempfile::tempdir().unwrap();
        let c = cache(tmp.path(), 1);
        let shared = c.pacman_pkg().unwrap();
        write(&shared.join("a-1.0-1-x86_64.pkg.tar.zst"), 100);
        write(&shared.join("a-1.0-1-x86_64.pkg.tar.zst.sig"), 10);

        c.evict_pkgs();
        assert!(!shared.join("a-1.0-1-x86_64.pkg.tar.zst").exists());
        assert!(!shared.join("a-1.0-1-x86_64.pkg.tar.zst.sig").exists());
    }
}

/// Free bytes on the filesystem holding `path`.
#[cfg(unix)]
fn free_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c_path` is NUL-terminated and outlives the call; `stat` is a
    // valid out-parameter.
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &raw mut stat) };
    (rc == 0).then(|| stat.f_bavail as u64 * stat.f_frsize as u64)
}

/// Bytes occupied under `path`, following no symlinks.
///
/// Only ever called for a tree with no size stamp -- the first reclaim after
/// one appears, or one left by an older version.
fn dir_size(path: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                stack.push(entry.path());
            } else if meta.is_file() {
                total += meta.len();
            }
        }
    }
    total
}
