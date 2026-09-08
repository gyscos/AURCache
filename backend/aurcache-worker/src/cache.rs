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

    /// Name of the file recording a tree's measured size and what its last
    /// build cost, written after each build so reclaim never has to walk one.
    /// Two whitespace-separated numbers: bytes, then seconds. A stamp with
    /// only the first is from before cost was recorded.
    const SIZE_STAMP: &'static str = ".aurcache-size";

    /// Record how big a package's persistent tree is, so reclaim can total the
    /// cache without walking it.
    ///
    /// Called once after a build, where the cost rides on top of something that
    /// already took minutes or hours. Measuring during reclaim instead would
    /// mean walking every candidate on every build, and these trees reach
    /// 130 GB and millions of files.
    pub fn record_builddir_size(&self, platform: &str, pkgbase: &str, build_secs: u64) {
        let Some(tree) = self.builddir(platform).map(|r| r.join(sanitize(pkgbase))) else {
            return;
        };
        if !tree.is_dir() {
            return;
        }
        let size = dir_size(&tree);
        let stamp = format!("{size} {build_secs}");
        if let Err(e) = std::fs::write(tree.join(Self::SIZE_STAMP), stamp) {
            tracing::debug!("could not record size of {}: {e}", tree.display());
        }
    }

    /// Drop persistent build trees, oldest first, until the cache is within
    /// `max_bytes` *and* the filesystem has `min_free` bytes spare. Never
    /// touches `keep`.
    ///
    /// Two limits because they answer different questions, and each is useless
    /// alone. A free-space floor does nothing on a large pool -- trees would
    /// grow into the terabytes before it ever triggered -- so `max_bytes` is
    /// what actually bounds the cache, independently of how big the underlying
    /// storage is. But a cap alone cannot see the rest of the machine, so the
    /// floor still covers a small disk, or one shared with something else that
    /// grew.
    ///
    /// Sizes come from the stamp each build leaves behind; a tree without one
    /// is walked once and stamped. So the usual case totals the cache by
    /// reading a handful of small files.
    ///
    /// **Nothing is evicted while the cache is within its limits.** Disk that
    /// nothing else needs is not worth reclaiming, and a tree kept is a rebuild
    /// avoided, so eviction happens only under real pressure -- never as a
    /// tidy-up.
    ///
    /// Under pressure, trees go in this order: **abandoned ones first**, then
    /// by worst *value density* -- rebuild seconds per byte.
    ///
    /// Abandoned first is what stops a tree outliving the package it belongs
    /// to. A worker is never told that a package was deleted from the server,
    /// and an expensive tree is the last thing density would ever give up, so
    /// `unreal-engine`'s 130 GB could otherwise sit there indefinitely after
    /// the package went away. Nothing having touched it in `max_age` is the
    /// available evidence that it is dead. Ordering and not a sweep, because an
    /// abandoned tree costs nothing while there is room for it.
    ///
    /// Density for the rest, rather than age or cost alone.
    ///
    /// Plain LRU is backwards among live trees: the one worth keeping is the
    /// one that took four hours, and that is exactly the package built rarely
    /// enough to look stale beside a dozen small ones rebuilt daily. But cost
    /// alone is wrong in the other direction, because a huge tree only earns
    /// its place while there is room for it. `unreal-engine` is four hours over
    /// 130 GB, about 1.0e-7 s/byte; a thirty-second package over 200 MB is
    /// 1.4e-7. The big tree is the *worst* value per byte despite being the
    /// most expensive -- and freeing 130 GB by dropping it costs four hours,
    /// where freeing the same space in small trees costs over five.
    ///
    /// So the expensive tree is kept while there is room and given up first
    /// when space is genuinely short, which is when it stops being worth its
    /// footprint.
    ///
    /// A staleness threshold and one ratio, rather than a weighted score over
    /// age, size and cost: those weights would be invented, and there is no
    /// evidence here to choose them with. A tree stamped before cost was
    /// recorded sorts as free to discard.
    ///
    /// Whole trees, never partial contents: half a tree is worse than none,
    /// because makepkg would treat it as resumable.
    ///
    /// Best-effort. Failing to reclaim is worth reporting and carrying on;
    /// refusing to build over it would turn a full disk into an idle worker.
    pub fn reclaim_builddirs(
        &self,
        platform: &str,
        keep: &str,
        max_bytes: u64,
        min_free: u64,
        max_age: std::time::Duration,
    ) {
        let Some(root) = self.builddir(platform) else {
            return;
        };

        // Oldest first. `mtime` on the tree root moves whenever a build writes
        // into it, which makes it a serviceable "last used".
        let mut trees: Vec<(u64, std::time::SystemTime, PathBuf, u64)> = std::fs::read_dir(&root)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let modified = e.metadata().ok()?.modified().ok()?;
                let path = e.path();
                let (size, cost) = Self::stamped(&path);
                Some((cost, modified, path, size))
            })
            .collect();
        // Abandoned trees first, then worst value per byte. Density is
        // compared by cross-multiplying rather than dividing, to stay in
        // integers: cost_a/size_a < cost_b/size_b is the same as
        // cost_a*size_b < cost_b*size_a. A zero-size tree frees nothing by
        // going, so it sorts last.
        let now = std::time::SystemTime::now();
        let abandoned = |modified: &std::time::SystemTime| {
            !max_age.is_zero()
                && now
                    .duration_since(*modified)
                    .is_ok_and(|unused_for| unused_for > max_age)
        };
        trees.sort_by(
            |(cost_a, mtime_a, _, size_a), (cost_b, mtime_b, _, size_b)| {
                abandoned(mtime_b).cmp(&abandoned(mtime_a)).then_with(|| {
                    match (*size_a == 0, *size_b == 0) {
                        (true, false) => std::cmp::Ordering::Greater,
                        (false, true) => std::cmp::Ordering::Less,
                        _ => (u128::from(*cost_a) * u128::from(*size_b))
                            .cmp(&(u128::from(*cost_b) * u128::from(*size_a)))
                            .then(mtime_a.cmp(mtime_b)),
                    }
                })
            },
        );

        let mut total: u64 = trees.iter().map(|(_, _, _, size)| size).sum();

        for (_, _, path, size) in trees {
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

    /// A tree's recorded size in bytes and what its last build cost in
    /// seconds, measuring and stamping the size if no stamp is there.
    fn stamped(tree: &Path) -> (u64, u64) {
        let stamp = tree.join(Self::SIZE_STAMP);
        if let Ok(text) = std::fs::read_to_string(&stamp) {
            let mut fields = text.split_whitespace();
            if let Some(Ok(size)) = fields.next().map(str::parse) {
                let cost = fields.next().and_then(|c| c.parse().ok()).unwrap_or(0);
                return (size, cost);
            }
        }
        let size = dir_size(tree);
        let _ = std::fs::write(&stamp, format!("{size} 0"));
        (size, 0)
    }

    /// Shared persistent GnuPG home for validpgpkeys.
    pub fn gnupg_home(&self) -> Option<PathBuf> {
        Self::ensured(self.root.join("gnupg"))
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

    /// Among trees of one size, the cheap ones go first and the expensive one
    /// survives even though it is the oldest -- plain LRU would have discarded
    /// exactly the tree worth keeping. The tree the build is about to use is
    /// never evicted either, even when it is itself what breaches the cap.
    #[test]
    fn reclaim_evicts_cheapest_and_never_the_one_in_use() {
        use std::time::{Duration, SystemTime};

        let tmp = tempfile::tempdir().unwrap();
        let c = cache(tmp.path(), 0);
        let root = c.builddir("x86_64").unwrap();

        // "costly" is both the oldest and the most expensive to rebuild: four
        // hours against thirty seconds. Age alone would evict it first.
        for (name, age, cost) in [
            ("costly", 300, 14400),
            ("cheap-old", 200, 30),
            ("cheap-new", 100, 30),
            ("wanted", 50, 5),
        ] {
            let tree = root.join(name);
            std::fs::create_dir_all(&tree).unwrap();
            std::fs::write(tree.join(Cache::SIZE_STAMP), format!("100 {cost}")).unwrap();
            let when = SystemTime::now() - Duration::from_secs(age);
            std::fs::File::open(&tree)
                .unwrap()
                .set_modified(when)
                .unwrap();
        }

        // Room for two of the four trees.
        c.reclaim_builddirs("x86_64", "wanted", 250, 0, Duration::ZERO);

        assert!(
            !root.join("cheap-old").exists(),
            "cheapest and oldest goes first"
        );
        assert!(!root.join("cheap-new").exists(), "then the other cheap one");
        assert!(
            root.join("costly").exists(),
            "the four-hour tree must outlive the thirty-second ones"
        );
        assert!(
            root.join("wanted").exists(),
            "the tree this build needs must survive"
        );
    }

    /// A huge tree is given up before small ones when space runs short, even
    /// though it cost the most to build: it is the worst value per byte, and
    /// dropping it frees more than every small tree put together. It only
    /// earns its place while the cache is within its limits.
    #[test]
    fn a_huge_tree_goes_before_small_ones_when_space_is_short() {
        use std::time::Duration;

        let tmp = tempfile::tempdir().unwrap();
        let c = cache(tmp.path(), 0);
        let root = c.builddir("x86_64").unwrap();

        // 4 hours over 130 GB is ~1.0e-7 s/byte; 30 s over 200 MB is ~1.4e-7.
        for (name, size, cost) in [
            ("huge", 130_000_000_000u64, 14400u64),
            ("small-a", 200_000_000, 30),
            ("small-b", 200_000_000, 30),
        ] {
            let tree = root.join(name);
            std::fs::create_dir_all(&tree).unwrap();
            std::fs::write(tree.join(Cache::SIZE_STAMP), format!("{size} {cost}")).unwrap();
        }

        c.reclaim_builddirs("x86_64", "none", 1_000_000_000, 0, Duration::ZERO);

        assert!(
            !root.join("huge").exists(),
            "the giant tree is the worst value per byte and goes first"
        );
        assert!(root.join("small-a").exists());
        assert!(root.join("small-b").exists());
    }

    /// Room to spare means nothing is touched -- not even a tree abandoned two
    /// months ago. Reclaiming disk nothing else wants would trade a rebuild for
    /// no gain.
    #[test]
    fn nothing_is_evicted_while_there_is_room() {
        use std::time::{Duration, SystemTime};

        let tmp = tempfile::tempdir().unwrap();
        let c = cache(tmp.path(), 0);
        let root = c.builddir("x86_64").unwrap();

        let tree = root.join("abandoned");
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::write(tree.join(Cache::SIZE_STAMP), "100 30").unwrap();
        let when = SystemTime::now() - Duration::from_secs(60 * 24 * 60 * 60);
        std::fs::File::open(&tree)
            .unwrap()
            .set_modified(when)
            .unwrap();

        // A cap nothing comes close to.
        c.reclaim_builddirs(
            "x86_64",
            "none",
            u64::MAX,
            0,
            Duration::from_secs(30 * 24 * 60 * 60),
        );

        assert!(
            tree.exists(),
            "an unused tree is not worth reclaiming while there is room for it"
        );
    }

    /// Once space is short, the abandoned tree goes first even though it is the
    /// most expensive to rebuild -- otherwise a tree outlives the package it
    /// belongs to, since the worker is never told a package was deleted and
    /// density would never give up the costly one.
    #[test]
    fn an_abandoned_tree_goes_first_once_space_is_short() {
        use std::time::{Duration, SystemTime};

        let tmp = tempfile::tempdir().unwrap();
        let c = cache(tmp.path(), 0);
        let root = c.builddir("x86_64").unwrap();

        for (name, age_days) in [("abandoned", 60), ("recent", 1)] {
            let tree = root.join(name);
            std::fs::create_dir_all(&tree).unwrap();
            // Expensive, so trimming would never pick it.
            std::fs::write(tree.join(Cache::SIZE_STAMP), "100 14400").unwrap();
            let when = SystemTime::now() - Duration::from_secs(age_days * 24 * 60 * 60);
            std::fs::File::open(&tree)
                .unwrap()
                .set_modified(when)
                .unwrap();
        }

        // Room for one of the two, so exactly one must go.
        c.reclaim_builddirs(
            "x86_64",
            "none",
            150,
            0,
            Duration::from_secs(30 * 24 * 60 * 60),
        );

        assert!(
            !root.join("abandoned").exists(),
            "the abandoned tree goes first, however costly it was to build"
        );
        assert!(
            root.join("recent").exists(),
            "a tree still in use is kept over one nothing has touched"
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

        assert_eq!(Cache::stamped(&tree), (4096, 0));
        // And it is stamped, so the walk happens once.
        assert_eq!(
            std::fs::read_to_string(tree.join(Cache::SIZE_STAMP)).unwrap(),
            "4096 0"
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
