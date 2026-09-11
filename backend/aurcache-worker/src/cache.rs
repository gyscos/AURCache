//! Worker-local build caches (best-effort). Persisted under the cache volume
//! and bind-mounted into each `makechrootpkg` copy:
//!
//! * per-pkgbase `SRCDEST` — incremental git fetch / tarball reuse,
//! * a persistent `GNUPGHOME` — keys stay trusted across updates,
//! * the pacman package cache.
//!
//! Every accessor is graceful: a missing directory is simply created, and any
//! I/O error is logged and never propagated into the build.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, SystemTime};

/// The part of a `repo.db` that tells us whether a cached archive is current:
/// `filename -> (compressed size, sha256)`.
pub type RepoDb = HashMap<String, (u64, String)>;

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
    ///
    /// A `repo_db` means the caller has fetched the repository this worker
    /// serves, and every job artifact that the DB names is checked against what
    /// the repository publishes *right now* before it enters the shared cache.
    /// That check is not a formality: the whole point of this change is that a
    /// build can start while the repository still carries the *old* bytes of a
    /// same-version rebuild of a dependency, download those old bytes, and then
    /// finish after the new ones are live. Without re-validating at promote
    /// time, this job would restore the exact stale bytes the start-of-job
    /// reconcile removed.
    ///
    /// `None` means the check could not be performed (the DB fetch failed, which
    /// this method does not know about), so the promoted names are recorded as
    /// *unverified* and the next reconcile hashes them rather than trusting a
    /// stale cached value. A file is never silently blocked from promotion —
    /// the shared cache just stops assuming it is correct until proved.
    pub fn promote_job_pkgs(&self, label: &str, repo_db: Option<&RepoDb>) -> usize {
        let (Some(shared), Some(job)) = (self.pacman_pkg(), self.pacman_pkg_job(label)) else {
            return 0;
        };
        let Ok(read) = std::fs::read_dir(&job) else {
            return 0;
        };
        let mut promoted = 0;
        let mut promoted_names = Vec::new();
        for entry in read.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            // Only finished artifacts: `.part` files are interrupted downloads.
            if !is_package_artifact(name) {
                continue;
            }
            let path = entry.path();
            // Official-repository files are trusted as they are: they are not
            // in the aurcache DB to compare against, and nothing here can have
            // gone stale through the rename cycle.
            if let Some(want) = repo_db
                && let Some(&(csize, ref sha)) = want.get(name)
            {
                let matches = match std::fs::metadata(&path) {
                    Ok(meta) if meta.len() != csize => false,
                    Ok(_) => sha256_file(&path).is_ok_and(|got| &got == sha),
                    Err(_) => false,
                };
                if !matches {
                    tracing::info!(
                        "not promoting {name}: the repository no longer publishes these bytes"
                    );
                    continue;
                }
            }
            match std::fs::rename(&path, shared.join(name)) {
                Ok(()) => promoted += 1,
                Err(e) => {
                    tracing::warn!("could not promote {name} to the shared cache: {e}");
                    continue;
                }
            }
            promoted_names.push(name.to_string());
        }
        // What the DB vouches for is recorded as verified; whatever had to be
        // promoted without a DB to name it is recorded as *not* verified, so a
        // later reconcile of the same name hashes it from scratch.
        {
            let mut known = verified_lock();
            match repo_db {
                Some(want) => {
                    for name in &promoted_names {
                        if let Some((_, sha)) = want.get(name) {
                            known.insert((shared.clone(), name.clone()), sha.clone());
                        }
                    }
                }
                None => {
                    for name in &promoted_names {
                        known.remove(&(shared.clone(), name.clone()));
                    }
                }
            }
        }
        promoted
    }

    /// Reconcile the shared package cache against what the served repository
    /// publishes, removing cache entries that no longer match.
    ///
    /// A cached AURCache archive and the `repo.db` entry that names it can only
    /// diverge inside this process: the chroots that use the cache mount it
    /// read-only, so every write goes through promote/evict/reconcile here. The
    /// one mode of divergence is a package deleted and re-added at the same
    /// version, whose rebuilt bytes differ from the still-cached bytes. pacman
    /// then finds the old file in a cache, skips its download, fails the
    /// integrity check at install because the file is stale, and cannot delete
    /// it from the read-only mount — the dependency install aborts.
    ///
    /// The pass is *event-driven*: a per-file record of the `repo.db` sha we
    /// last verified makes a no-op job cost a directory read and a hash-map
    /// lookup per package, with no stat and no hashing. A file is suspicious
    /// only when its record is missing or its entry changed, and even then a
    /// size mismatch removes it without reading a byte. Only files whose size
    /// still matches get hashed, in parallel once there are enough of them.
    ///
    /// Files the DB does not name (official core/extra/multilib packages) are
    /// left alone entirely: absence from the aurcache repo is not staleness.
    /// Returns how many stale archives were removed.
    pub fn reconcile_pkgs(&self, repo_db: &RepoDb) -> usize {
        let Some(shared) = self.pacman_pkg() else {
            return 0;
        };
        let mut removed = 0;
        let mut suspects: Vec<(String, String)> = Vec::new();
        {
            let mut known = verified_lock();
            for (name, (csize, sha)) in repo_db {
                let key = (shared.clone(), name.clone());
                if known.get(&key).is_some_and(|seen| seen == sha) {
                    continue;
                }
                let path = shared.join(name);
                match std::fs::metadata(&path) {
                    Err(_) => {
                        // Nothing cached; nothing to verify until some promote
                        // actually lands the file.
                        known.insert(key, sha.clone());
                    }
                    Ok(meta) if meta.len() != *csize => {
                        if remove_cached(&shared, name) {
                            removed += 1;
                        }
                        known.remove(&key);
                    }
                    Ok(_) => suspects.push((name.clone(), sha.clone())),
                }
            }
            // Files no longer published are not consulted anymore; drop their
            // records so the map tracks the repository instead of growing.
            known.retain(|key, _| repo_db.contains_key(&key.1));
        }
        let verdicts = hash_suspects(&shared, &suspects);
        {
            let mut known = verified_lock();
            for ((name, sha), matches) in suspects.iter().zip(verdicts) {
                let key = (shared.clone(), name.clone());
                if matches {
                    known.insert(key, sha.clone());
                } else if remove_cached(&shared, name) {
                    removed += 1;
                    known.remove(&key);
                }
            }
        }
        removed
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

/// Repository DB entries verified against the shared package cache:
/// `(cache dir, filename) -> last-verified `repo.db` sha256`.
///
/// Keyed on the cache dir as well as the name so the tests, which stack many
/// caches in one process, never see each other's records. The wrinkle this
/// records is narrow: the shared cache is written only by this worker
/// (chroots mount it read-only), so a cached AURCache file can only go stale
/// when its `repo.db` entry changes, and that a straightforward comparison
/// catches — no stat and no hashing for an unchanged file.
static VERIFIED: OnceLock<Mutex<HashMap<(PathBuf, String), String>>> = OnceLock::new();

fn verified_lock() -> MutexGuard<'static, HashMap<(PathBuf, String), String>> {
    VERIFIED
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Many files by fiat; hashing this many without threads is the one case where
/// the added threads are plainly worth it.
const PARALLEL_HASH_THRESHOLD: usize = 8;

/// Hash each suspect archive and report whether it matches the `repo.db` sha.
///
/// The steady state is zero; a restart's first pass or a mass-`--from-installed`
/// rebuild re-verifies the whole repository at most once, and that is the only
/// case with enough files to pay for threads, so the work stays serial below
/// [`PARALLEL_HASH_THRESHOLD`] and fans out with `std::thread::scope` past it —
/// no channeling and no new dependency. Each file is, and remains, a single
/// answer, so the results keep positional order.
fn hash_suspects(shared: &Path, suspects: &[(String, String)]) -> Vec<bool> {
    if suspects.len() < PARALLEL_HASH_THRESHOLD {
        return suspects
            .iter()
            .map(|(name, sha)| sha256_file(&shared.join(name)).is_ok_and(|got| &got == sha))
            .collect();
    }
    let workers = std::thread::available_parallelism()
        .map_or(4, std::num::NonZeroUsize::get)
        .min(suspects.len());
    #[allow(clippy::needless_collect)]
    std::thread::scope(|scope| {
        let threads: Vec<_> = suspects
            .chunks(suspects.len().div_ceil(workers))
            .map(|chunk| {
                scope.spawn(move || -> Vec<bool> {
                    chunk
                        .iter()
                        .map(|(name, sha)| {
                            sha256_file(&shared.join(name)).is_ok_and(|got| &got == sha)
                        })
                        .collect()
                })
            })
            .collect();
        threads
            .into_iter()
            .flat_map(|t| t.join().unwrap_or_default())
            .collect()
    })
}

/// A package archive whose stored bytes no longer match its `repo.db` entry.
/// Removed, along with its detached signature. A file already gone counts as
/// removed; a genuine permission trouble surfaces as a warning and leaves the
/// file — and the manifest record that would have been cleared — in place, so
/// the next reconcile tries again.
fn remove_cached(shared: &Path, name: &str) -> bool {
    let gone = match std::fs::remove_file(shared.join(name)) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(e) => {
            tracing::warn!("could not remove stale cached package {name}: {e}");
            false
        }
    };
    let _ = std::fs::remove_file(shared.join(format!("{name}.sig")));
    gone
}

/// Hex sha256 of a file, streamed through 64 KiB so multi-hundred-MB packages
/// are hashed without ever being in memory whole.
fn sha256_file(path: &Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Parse `repo.db` (or `<name>.db`) — gzip'd tar of one `desc` per package, as
/// `repo-add` writes it — into `filename -> (compressed size, sha256)`.
///
/// Each package's entry is a `<name>-<version>/desc` file holding
/// `%FILENAME%`, `%CSIZE%` and `%SHA256SUM%` fields. An entry missing any of
/// them is skipped rather than fatal; so is a tar member that is not a
/// two-level `desc`.
pub fn parse_repo_db(db: &[u8]) -> anyhow::Result<RepoDb> {
    use std::io::Read;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(db));
    let mut out = RepoDb::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        let Ok(path) = entry.path() else { continue };
        if path.components().count() != 2
            || path.file_name().and_then(|n| n.to_str()) != Some("desc")
        {
            continue;
        }
        let mut text = String::new();
        entry.read_to_string(&mut text)?;
        if let Some(triple) = parse_desc(&text) {
            out.insert(triple.0, (triple.1, triple.2));
        }
    }
    Ok(out)
}

/// `%FILENAME%`, `%CSIZE%` and `%SHA256SUM%` of one `desc` file. A field is
/// one line plus one following value line, so values use the same `i+1` shape
/// as every other field parser in this repository.
fn parse_desc(desc: &str) -> Option<(String, u64, String)> {
    let lines: Vec<&str> = desc.lines().collect();
    let mut filename = None;
    let mut size = None;
    let mut sha = None;
    for (i, line) in lines.iter().enumerate() {
        let value = lines.get(i + 1).map(|l| l.trim());
        match *line {
            "%FILENAME%" => filename = value.filter(|v| !v.is_empty()).map(str::to_string),
            "%CSIZE%" => size = value.and_then(|v| v.trim().parse().ok()),
            "%SHA256SUM%" => sha = value.filter(|v| !v.is_empty()).map(str::to_string),
            _ => {}
        }
    }
    Some((filename?, size?, sha?))
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

        assert_eq!(c.promote_job_pkgs("job-1", None), 1);
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
            assert_eq!(c.promote_job_pkgs(label, None), 1);
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

    fn sha_of(bytes: &[u8]) -> String {
        use sha2::Digest;
        hex::encode(sha2::Sha256::digest(bytes))
    }

    fn make_db(entries: &[(&str, u64, &str)]) -> RepoDb {
        entries
            .iter()
            .map(|&(name, size, sha)| (name.to_string(), (size, sha.to_string())))
            .collect()
    }

    /// A real `repo-add`-shaped `repo.db`: gzip'd tar of one `desc` per package.
    fn repo_db_bytes(entries: &[(&str, u64, &str)]) -> Vec<u8> {
        use std::io::Write;
        let mut tar = tar::Builder::new(Vec::new());
        for (name, size, sha) in entries {
            let mut header = tar::Header::new_gnu();
            let data = format!("%FILENAME%\n{name}\n%CSIZE%\n{size}\n%SHA256SUM%\n{sha}\n");
            header.set_size(data.len() as u64);
            tar.append_data(&mut header, format!("{name}-1-any/desc"), data.as_bytes())
                .unwrap();
        }
        let raw = tar.into_inner().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&raw).unwrap();
        gz.finish().unwrap()
    }

    #[test]
    fn parses_a_repo_db_into_names_sizes_and_shas() {
        let name_a = "a-1.0-1-x86_64.pkg.tar.zst";
        let bytes = repo_db_bytes(&[
            (name_a, 100, &sha_of(&[b'x'; 100])),
            ("b-2.0-1-x86_64.pkg.tar.zst", 42, &"f".repeat(64)),
        ]);
        let repo = parse_repo_db(&bytes).unwrap();
        assert_eq!(repo.len(), 2);
        assert_eq!(repo.get(name_a), Some(&(100, sha_of(&[b'x'; 100]))));
        assert_eq!(
            repo.get("b-2.0-1-x86_64.pkg.tar.zst"),
            Some(&(42, "f".repeat(64)))
        );
    }

    /// The canonical failure this all exists for: a cache entry between two
    /// runs of the same-version rebuild, whose size already betrays it. The
    /// size check removes it without hashing a byte.
    #[test]
    fn reconcile_removes_a_file_whose_entry_grew_in_size() {
        let tmp = tempfile::tempdir().unwrap();
        let c = cache(tmp.path(), 0);
        let shared = c.pacman_pkg().unwrap();
        let name = "a-1.0-1-x86_64.pkg.tar.zst";
        write(&shared.join(name), 100);
        write(&shared.join(format!("{name}.sig")), 10);

        let repo = make_db(&[(name, 120, &"f".repeat(64))]);
        assert_eq!(c.reconcile_pkgs(&repo), 1);
        assert!(!shared.join(name).exists());
        assert!(!shared.join(format!("{name}.sig")).exists());
    }

    /// Same size, new bytes — the sizing trick cannot see it, so the file is
    /// hashed before it is trusted again.
    #[test]
    fn reconcile_hashes_and_removes_a_file_whose_bytes_changed_at_the_same_size() {
        let tmp = tempfile::tempdir().unwrap();
        let c = cache(tmp.path(), 0);
        let shared = c.pacman_pkg().unwrap();
        let name = "a-1.0-1-x86_64.pkg.tar.zst";
        write(&shared.join(name), 100);

        let repo = make_db(&[(name, 100, &sha_of(&[b'y'; 100]))]);
        assert_eq!(c.reconcile_pkgs(&repo), 1);
        assert!(!shared.join(name).exists());
    }

    /// A matching file costs a directory read and a map lookup, not a hash, and
    /// a repeat pass is a no-op.
    #[test]
    fn reconcile_leaves_matching_files_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let c = cache(tmp.path(), 0);
        let shared = c.pacman_pkg().unwrap();
        let name = "a-1.0-1-x86_64.pkg.tar.zst";
        write(&shared.join(name), 100);

        let repo = make_db(&[(name, 100, &sha_of(&[b'x'; 100]))]);
        assert_eq!(c.reconcile_pkgs(&repo), 0);
        assert!(shared.join(name).exists());
        assert_eq!(c.reconcile_pkgs(&repo), 0);
    }

    /// Official core/extra/multilib archives are absent from the aurcache DB
    /// *by construction*; absence is not staleness, so they are left alone.
    #[test]
    fn reconcile_leaves_files_the_db_does_not_name_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let c = cache(tmp.path(), 0);
        let shared = c.pacman_pkg().unwrap();
        let official = "gcc-13.2.1-3-x86_64.pkg.tar.zst";
        write(&shared.join(official), 50);

        assert_eq!(c.reconcile_pkgs(&RepoDb::new()), 0);
        assert!(shared.join(official).exists());
    }

    /// A job that fetched the *old* bytes of a same-version rebuild must not be
    /// able to restore them over the read-only cache: the DB names them, the
    /// size prefilter catches them, and the stale bytes never reach the shared
    /// side at all.
    #[test]
    fn promote_refuses_old_bytes_of_a_rebuilt_dependency() {
        let tmp = tempfile::tempdir().unwrap();
        let c = cache(tmp.path(), 0);
        let job = c.pacman_pkg_job("job-1").unwrap();
        let shared = c.pacman_pkg().unwrap();
        let name = "a-1.0-1-x86_64.pkg.tar.zst";
        write(&job.join(name), 100);

        let repo = make_db(&[(name, 100, &sha_of(&[b'y'; 100]))]);
        assert_eq!(c.promote_job_pkgs("job-1", Some(&repo)), 0);
        assert!(job.join(name).exists(), "the artifact must stay put");
        assert!(!shared.join(name).exists());
    }

    /// An artifact that passes the check (size and hash) is promoted and
    /// recorded as verified, so the very next reconcile costs nothing.
    #[test]
    fn promote_with_a_matching_db_records_verified() {
        let tmp = tempfile::tempdir().unwrap();
        let c = cache(tmp.path(), 0);
        let job = c.pacman_pkg_job("job-1").unwrap();
        let shared = c.pacman_pkg().unwrap();
        let name = "a-1.0-1-x86_64.pkg.tar.zst";
        write(&job.join(name), 100);

        let repo = make_db(&[(name, 100, &sha_of(&[b'x'; 100]))]);
        assert_eq!(c.promote_job_pkgs("job-1", Some(&repo)), 1);
        assert!(shared.join(name).exists());
        assert_eq!(c.reconcile_pkgs(&repo), 0);
    }

    /// The end-to-end story: the DB fetch failed, so a job's stale old bytes
    /// were promoted unvalidated; the manifest records that, and the reconcile
    /// that sees the rebuilt DB re-hashes and removes them before pacman can
    /// pick them up from the read-only mount.
    #[test]
    fn an_unvalidated_promote_is_rechecked_by_the_next_reconcile() {
        let tmp = tempfile::tempdir().unwrap();
        let c = cache(tmp.path(), 0);
        let job = c.pacman_pkg_job("job-1").unwrap();
        let shared = c.pacman_pkg().unwrap();
        let name = "a-1.0-1-x86_64.pkg.tar.zst";
        write(&job.join(name), 100);

        // No DB (the fetch failed): the promote cannot tell old from new bytes.
        assert_eq!(c.promote_job_pkgs("job-1", None), 1);
        assert!(shared.join(name).exists());

        // The rebuilt DB says these bytes are no longer what it publishes.
        let repo = make_db(&[(name, 100, &sha_of(&[b'y'; 100]))]);
        assert_eq!(c.reconcile_pkgs(&repo), 1);
        assert!(!shared.join(name).exists());
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
