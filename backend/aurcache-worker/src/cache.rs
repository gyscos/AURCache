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
}

/// A per-pkgbase source cache entry considered for eviction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheEntry {
    pub pkgbase: String,
    pub size: u64,
    pub last_used: SystemTime,
}

impl Cache {
    pub fn new(root: &Path, max_size: u64, ttl_secs: u64) -> Self {
        Self {
            root: root.to_path_buf(),
            max_size,
            ttl: Duration::from_secs(ttl_secs),
        }
    }

    /// `SRCDEST` for a pkgbase; created on demand. Returns `None` only if the
    /// directory truly cannot be created (build falls back to an ephemeral dir).
    pub fn srcdest(&self, pkgbase: &str) -> Option<PathBuf> {
        self.ensured(self.root.join("srcdest").join(sanitize(pkgbase)))
    }

    /// Shared persistent GnuPG home for validpgpkeys.
    pub fn gnupg_home(&self) -> Option<PathBuf> {
        self.ensured(self.root.join("gnupg"))
    }

    /// Shared pacman package cache (bind-mounted into the chroot copy).
    #[allow(dead_code)]
    pub fn pacman_pkg(&self) -> Option<PathBuf> {
        self.ensured(self.root.join("pacman-pkg"))
    }

    fn ensured(&self, path: PathBuf) -> Option<PathBuf> {
        match std::fs::create_dir_all(&path) {
            Ok(()) => Some(path),
            Err(e) => {
                tracing::warn!("cache dir {} unavailable: {e}", path.display());
                None
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
        let entries = match self.scan_srcdest() {
            Ok(e) => e,
            Err(e) => {
                tracing::debug!("cache scan skipped: {e}");
                return Vec::new();
            }
        };
        let plan = plan_eviction(&entries, self.max_size, self.ttl, SystemTime::now(), in_use);
        for pkgbase in &plan {
            self.wipe_srcdest(pkgbase);
            tracing::info!("evicted cache entry {pkgbase}");
        }
        plan
    }

    fn scan_srcdest(&self) -> std::io::Result<Vec<CacheEntry>> {
        let dir = self.root.join("srcdest");
        let mut entries = Vec::new();
        let Ok(read) = std::fs::read_dir(&dir) else {
            return Ok(entries);
        };
        for ent in read.flatten() {
            if !ent.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let pkgbase = ent.file_name().to_string_lossy().to_string();
            let path = ent.path();
            let size = dir_size(&path);
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
        Ok(entries)
    }
}

/// Recursively compute a directory's size in bytes (best-effort).
fn dir_size(path: &Path) -> u64 {
    let mut total = 0;
    if let Ok(read) = std::fs::read_dir(path) {
        for ent in read.flatten() {
            let Ok(ft) = ent.file_type() else { continue };
            if ft.is_dir() {
                total += dir_size(&ent.path());
            } else if let Ok(md) = ent.metadata() {
                total += md.len();
            }
        }
    }
    total
}

/// Replace path-unsafe characters so a pkgbase maps to a single directory.
fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+') {
            c
        } else {
            '_'
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
        if in_use.iter().any(|u| u == &e.pkgbase) {
            kept.push(e);
            continue;
        }
        let aged = !ttl.is_zero()
            && now
                .duration_since(e.last_used)
                .map(|age| age > ttl)
                .unwrap_or(false);
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
        let mut i = 0;
        while total > max_size && i < kept.len() {
            let e = kept[i];
            if !in_use.iter().any(|u| u == &e.pkgbase) {
                evict.push(e.pkgbase.clone());
                total = total.saturating_sub(e.size);
            }
            i += 1;
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
            CacheEntry { pkgbase: "old".into(), size: 10, last_used: t(100) },
            CacheEntry { pkgbase: "fresh".into(), size: 10, last_used: t(1) },
        ];
        let evict = plan_eviction(&entries, 0, Duration::from_secs(50), now(), &[]);
        assert_eq!(evict, vec!["old".to_string()]);
    }

    #[test]
    fn evicts_lru_over_size_budget() {
        let entries = vec![
            CacheEntry { pkgbase: "a".into(), size: 100, last_used: t(30) },
            CacheEntry { pkgbase: "b".into(), size: 100, last_used: t(20) },
            CacheEntry { pkgbase: "c".into(), size: 100, last_used: t(10) },
        ];
        // Budget 250 → must drop the oldest (a).
        let evict = plan_eviction(&entries, 250, Duration::ZERO, now(), &[]);
        assert_eq!(evict, vec!["a".to_string()]);
    }

    #[test]
    fn never_evicts_in_use() {
        let entries = vec![
            CacheEntry { pkgbase: "busy".into(), size: 1000, last_used: t(999) },
        ];
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
        let entries = vec![
            CacheEntry { pkgbase: "a".into(), size: 10_000, last_used: t(10_000) },
        ];
        assert!(plan_eviction(&entries, 0, Duration::ZERO, now(), &[]).is_empty());
    }

    #[test]
    fn sanitizes_pkgbase() {
        assert_eq!(sanitize("ttf-google-fonts-git"), "ttf-google-fonts-git");
        assert_eq!(sanitize("../etc/passwd"), ".._etc_passwd");
        assert_eq!(sanitize("a/b"), "a_b");
    }
}
