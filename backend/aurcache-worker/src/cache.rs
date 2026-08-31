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
            let pkgbase = ent.file_name().to_string_lossy().to_string();
            let path = ent.path();
            let size = aurcache_utils::utils::dir_size::dir_size(&path).unwrap_or(0);
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
        let name = ent.file_name().to_string_lossy().to_string();
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
