use std::collections::BTreeMap;
use std::io::Read;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use alpm_srcinfo::SourceInfoV1;
use aurcache_db::packages::SourceData;
use git2::Oid;
use lru::LruCache;
use tokio::sync::Mutex;

use crate::patch::SourcePatch;
use crate::pkgbuild::fix_source_urls;

/// Base URL for AUR git repositories. AUR packages are unified with git
/// sources: `https://aur.archlinux.org/{pkgbase}.git`, ref `HEAD`, no
/// subfolder. The AUR RPC (`AurClient`) is only used for metadata/dependency
/// resolution, not for fetching sources.
fn default_aur_git_base_url() -> String {
    "https://aur.archlinux.org".to_string()
}

/// Git's per-checkout metadata directory, excluded from source archives.
const GIT_METADATA_DIR: &str = ".git";

/// Maximum number of distinct sources kept in the in-memory cache at once.
/// At roughly 5-20KB per entry (a small tar.gz of PKGBUILD/.SRCINFO/aux
/// files, not the package's actual upstream source code, plus a parsed
/// `.SRCINFO`), this bounds worst-case memory to a low tens-of-MB even if
/// every package were touched in a single sweep (e.g. periodic version
/// checks), while comfortably covering the working set of packages actively
/// being added/built/edited at once.
const CACHE_CAPACITY: usize = 1000;

/// A single rendered view of a source: either the pristine upstream fetch,
/// or the result of applying a patch on top of it.
struct SourceSnapshot {
    archive_bytes: Vec<u8>,
    /// `None` when this snapshot's `.SRCINFO`/`PKGBUILD` could not be parsed
    /// (e.g. a malformed PKGBUILD upstream, such as `ogdf`). Browsing and
    /// editing raw source files must keep working in that case so a patch
    /// can be authored to fix the parse failure before the source is ever
    /// added as a package; only dependency/version resolution actually
    /// requires a successfully parsed `.SRCINFO`.
    sourceinfo: Option<SourceInfoV1>,
    /// Best-effort pkgbase, used to name the archive's top-level directory
    /// and as the file-listing root even when `sourceinfo` is `None`.
    pkgbase: String,
}

struct CacheEntry {
    /// Resolved commit id of the raw (unpatched) checkout, used to detect
    /// whether a `refresh` actually changed anything.
    commit: Oid,
    /// What most callers want: the patched result if a patch is currently
    /// active, otherwise identical to the raw fetch. There is at most one
    /// active patch per source at any time (matching the data model - a
    /// package has a single `patch` column), so a new/cleared patch simply
    /// replaces this entry rather than accumulating variants.
    active: SourceSnapshot,
    /// Only `Some` when a patch is currently active, holding the pristine
    /// pre-patch snapshot. Needed so `read_file(patch=None)` can diff edits
    /// against the true original rather than the currently-patched content.
    /// When no patch is active this is `None` (rather than a redundant copy
    /// of `active`), since a caller wanting the original in that case can
    /// just use `active` directly.
    original: Option<SourceSnapshot>,
    /// The patch that produced `active` from `original`, if any. Kept so
    /// `refresh` can re-apply the same patch on top of a freshly fetched
    /// raw source instead of silently dropping it.
    patch: Option<SourcePatch>,
}

impl CacheEntry {
    /// The pristine (pre-patch) snapshot: `original` if a patch is active,
    /// otherwise `active` itself (which already *is* the pristine content).
    fn original(&self) -> &SourceSnapshot {
        self.original.as_ref().unwrap_or(&self.active)
    }
}

/// Directory under which persistent git checkouts are kept, one subdirectory
/// per `SourceData::cache_key()`. Reusing these clones across calls means a
/// `refresh` only needs to `git fetch` (transfer new objects) instead of a
/// full re-clone.
fn default_checkout_root() -> PathBuf {
    std::env::var("AURCACHE_SOURCE_CACHE_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./source_cache"))
}

/// Cache for checked-out source repositories and their parsed `.SRCINFO`.
///
/// A single instance should be passed around so that repeated requests for
/// the same source return the cached result. Backed by a persistent on-disk
/// git checkout per source (see `default_checkout_root`), so repeat fetches
/// are cheap incremental `git fetch`s rather than full clones/downloads.
///
/// Patched content is never written to disk - only the persistent raw git
/// checkout is - a patch is applied in a temporary directory that's deleted
/// once the resulting archive/`.SRCINFO` have been computed, with the result
/// held only in this in-memory cache. Applying a patch on top of an
/// already-fetched raw checkout is cheap (in-memory extract + patch + re-tar,
/// no network), so a cache miss on the patched content never requires
/// re-fetching from the remote.
///
/// Bounded by an LRU eviction policy (see [`CACHE_CAPACITY`]) so memory use
/// doesn't grow unboundedly as more distinct sources are touched over the
/// process's lifetime.
pub struct SnapshotStore {
    /// Keyed by `SourceData::cache_key()`. At most one entry per source.
    cache: Mutex<LruCache<String, Arc<CacheEntry>>>,
    checkout_root: PathBuf,
    aur_git_base_url: String,
}

impl Default for SnapshotStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SnapshotStore {
    pub fn new() -> Self {
        Self::with_checkout_root(default_checkout_root())
    }

    pub fn with_checkout_root(checkout_root: PathBuf) -> Self {
        Self::with_checkout_root_and_aur_base(checkout_root, default_aur_git_base_url())
    }

    /// Construct a store with an explicit checkout root and AUR git base URL
    /// (e.g. `file:///tmp/fake-aur` in tests, instead of the real AUR).
    pub fn with_checkout_root_and_aur_base(
        checkout_root: PathBuf,
        aur_git_base_url: impl Into<String>,
    ) -> Self {
        Self {
            cache: Mutex::new(LruCache::new(
                NonZeroUsize::new(CACHE_CAPACITY).expect("CACHE_CAPACITY must be non-zero"),
            )),
            checkout_root,
            aur_git_base_url: aur_git_base_url.into(),
        }
    }

    /// Return the parsed `.SRCINFO` for `source_data`, fetching it if not cached.
    ///
    /// `patch` is the raw JSON representation of a [`SourcePatch`] (as stored
    /// in `packages.patch`); pass `None` for unpatched sources. When a
    /// (non-empty) patch is given, `.SRCINFO` is always regenerated from the
    /// patched `PKGBUILD`.
    ///
    /// Returns an error if the (possibly patched) source's `.SRCINFO`/PKGBUILD
    /// could not be parsed - unlike [`SnapshotStore::list_files`]/
    /// [`SnapshotStore::read_file`], which work even for unparseable sources
    /// so a patch can be authored to fix the parse failure.
    pub async fn sourceinfo(
        &self,
        source_data: &SourceData,
        patch: Option<&str>,
    ) -> anyhow::Result<SourceInfoV1> {
        let entry = self.get_or_fetch(source_data, patch).await?;
        entry
            .active
            .sourceinfo
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Source's .SRCINFO/PKGBUILD could not be parsed"))
    }

    /// Return the raw archive bytes for `source_data`, fetching it if not cached.
    ///
    /// See [`SnapshotStore::sourceinfo`] for the meaning of `patch`.
    pub async fn archive_bytes(
        &self,
        source_data: &SourceData,
        patch: Option<&str>,
    ) -> anyhow::Result<Vec<u8>> {
        let entry = self.get_or_fetch(source_data, patch).await?;
        Ok(entry.active.archive_bytes.clone())
    }

    /// List the (unpatched) source files available for editing, relative to
    /// the source root (e.g. `PKGBUILD`, `foo.install`). Works even if the
    /// source's `.SRCINFO`/PKGBUILD fails to parse.
    pub async fn list_files(&self, source_data: &SourceData) -> anyhow::Result<Vec<String>> {
        let entry = self.get_or_fetch_any(source_data).await?;
        let original = entry.original();
        list_files_in_archive(&original.archive_bytes, &original.pkgbase)
    }

    /// Return the effective content of a single source file: the pristine
    /// content when `patch` is `None`, otherwise that content with the
    /// stored patch (if any) applied. Works even if the source's
    /// `.SRCINFO`/PKGBUILD fails to parse.
    pub async fn read_file(
        &self,
        source_data: &SourceData,
        patch: Option<&str>,
        rel_path: &str,
    ) -> anyhow::Result<String> {
        // Always read the pristine content, regardless of whatever patch (if
        // any) happens to already be cached for this source, since `patch`
        // here is applied fresh on top of it below.
        let entry = self.get_or_fetch_any(source_data).await?;
        let original_snapshot = entry.original();
        let original = read_file_from_archive(
            &original_snapshot.archive_bytes,
            &original_snapshot.pkgbase,
            rel_path,
        )?;

        match patch.map(SourcePatch::parse).transpose()? {
            None => Ok(original),
            Some(patch) => patch.apply_to_content(rel_path, &original),
        }
    }

    /// Like [`SnapshotStore::read_file`], but never fails solely because the
    /// stored patch no longer applies cleanly to the current pristine
    /// content: returns the pristine content unconditionally, plus the
    /// patched content if this file is part of `patch` and it still applies,
    /// plus an error message if it's part of `patch` but no longer applies.
    /// Intended for UI consumption, where the user should always be able to
    /// see (and revert to) the original content even when their patch is
    /// stale.
    pub async fn read_file_with_patch_status(
        &self,
        source_data: &SourceData,
        patch: Option<&str>,
        rel_path: &str,
    ) -> anyhow::Result<(String, Option<String>, Option<String>)> {
        let entry = self.get_or_fetch_any(source_data).await?;
        let original_snapshot = entry.original();
        let original = read_file_from_archive(
            &original_snapshot.archive_bytes,
            &original_snapshot.pkgbase,
            rel_path,
        )?;

        let patch = match patch.map(SourcePatch::parse).transpose()? {
            Some(patch) if patch.diff_for(rel_path).is_some() => patch,
            _ => return Ok((original, None, None)),
        };

        match patch.apply_to_content(rel_path, &original) {
            Ok(patched) => Ok((original, Some(patched), None)),
            Err(e) => Ok((original, None, Some(e.to_string()))),
        }
    }

    /// Proactively refresh the cache entry for `source_data`: fetch the
    /// latest state from the remote and, if the resolved ref actually moved
    /// (or there was no cached entry yet), re-parse/re-tar it. Returns `true`
    /// if the entry changed (i.e. the source was not up to date with what
    /// was previously cached), `false` if it was already current.
    ///
    /// This is intended to be called from the periodic version-check loop so
    /// that staleness is detected (and long-lived caches kept honest) without
    /// unconditionally re-downloading/re-cloning on every check. If a patch
    /// was previously active for this source it is re-applied on top of the
    /// freshly fetched raw source, so the cache entry stays consistent.
    /// Metadata about a package read from its checkout: the `.SRCINFO`
    /// fields, the maintainer comment in the PKGBUILD, and the packaging
    /// history from git.
    ///
    /// Everything here is already on disk — this store cloned the repository
    /// to resolve the source in the first place — so it replaces what used to
    /// be a live AUR lookup per request.
    pub async fn source_metadata(
        &self,
        source_data: &SourceData,
        patch: Option<&str>,
    ) -> anyhow::Result<crate::package::source_metadata::SourceMetadata> {
        let entry = self.get_or_fetch(source_data, patch).await?;
        Ok(self.metadata_from_entry(&entry, source_data))
    }

    /// The parsed `.SRCINFO` and the metadata, from a single resolve.
    ///
    /// The version check needs both: the `.SRCINFO` to sync VCS sources, and
    /// the metadata to mirror onto the row. Asking for them separately meant
    /// two `get_or_fetch` round trips per package per pass — cheap, since the
    /// second is served from cache, but pointless.
    pub async fn sourceinfo_and_metadata(
        &self,
        source_data: &SourceData,
        patch: Option<&str>,
    ) -> anyhow::Result<(
        SourceInfoV1,
        crate::package::source_metadata::SourceMetadata,
    )> {
        let entry = self.get_or_fetch(source_data, patch).await?;
        let metadata = self.metadata_from_entry(&entry, source_data);
        let sourceinfo = entry
            .active
            .sourceinfo
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Source's .SRCINFO/PKGBUILD could not be parsed"))?;
        Ok((sourceinfo, metadata))
    }

    fn metadata_from_entry(
        &self,
        entry: &CacheEntry,
        source_data: &SourceData,
    ) -> crate::package::source_metadata::SourceMetadata {
        use crate::package::source_metadata::{from_sourceinfo, maintainer_from_pkgbuild};

        let mut metadata = entry
            .active
            .sourceinfo
            .as_ref()
            .map(from_sourceinfo)
            .unwrap_or_default();

        // Best-effort: a PKGBUILD that cannot be read still leaves the
        // `.SRCINFO`-derived fields usable.
        if let Ok(pkgbuild) = read_file_from_archive(
            &entry.active.archive_bytes,
            &entry.active.pkgbase,
            "PKGBUILD",
        ) {
            metadata.maintainer = maintainer_from_pkgbuild(&pkgbuild);
        }

        let (first, last) = self.packaging_history(source_data);
        metadata.first_submitted = first;
        metadata.last_modified = last;
        metadata
    }

    /// When a package's packaging was first and last touched, from the commit
    /// history of its checkout.
    ///
    /// This is the packaging repository's history, not the upstream project's:
    /// for an AUR package it is exactly "first submitted" and "last modified".
    ///
    /// Returns `(None, None)` rather than failing — a shallow or unreadable
    /// repository should cost two display fields, not the whole lookup.
    fn packaging_history(&self, source_data: &SourceData) -> (Option<i64>, Option<i64>) {
        let path = self
            .checkout_root
            .join(sanitize_cache_key(&source_data.cache_key()));

        let Ok(repo) = git2::Repository::open(&path) else {
            return (None, None);
        };
        let Ok(mut walk) = repo.revwalk() else {
            return (None, None);
        };
        if walk.push_head().is_err() {
            return (None, None);
        }

        let mut newest = None;
        let mut oldest = None;
        for oid in walk.flatten() {
            let Ok(commit) = repo.find_commit(oid) else {
                continue;
            };
            let time = commit.time().seconds();
            // The walk starts at HEAD and goes back, so the first commit seen
            // is the newest and the last is the initial one.
            if newest.is_none() {
                newest = Some(time);
            }
            oldest = Some(time);
        }

        (oldest, newest)
    }

    pub async fn refresh(&self, source_data: &SourceData) -> anyhow::Result<bool> {
        let cache_key = source_data.cache_key();
        let previous = {
            let mut cache = self.cache.lock().await;
            cache.get(&cache_key).cloned()
        };
        let previous_commit = previous.as_ref().map(|entry| entry.commit);

        let (repo_url, git_ref, subfolder) = self.git_coordinates(source_data)?;
        let path = self.checkout_root.join(sanitize_cache_key(&cache_key));

        let (commit, archive_bytes, pkgbase, sourceinfo) =
            checkout_and_parse(&repo_url, &git_ref, &subfolder, &path).await?;

        let changed = previous_commit != Some(commit);
        if changed {
            let raw = SourceSnapshot {
                archive_bytes,
                sourceinfo,
                pkgbase,
            };

            // Re-apply whichever patch (if any) was previously active for
            // this source, so a `refresh` doesn't silently drop it.
            let existing_patch = previous.and_then(|entry| entry.patch.clone());
            let entry = match existing_patch {
                None => Arc::new(CacheEntry {
                    commit,
                    active: raw,
                    original: None,
                    patch: None,
                }),
                Some(patch) => {
                    let (patched_bytes, patched_sourceinfo) =
                        apply_patch_to_archive(&raw.archive_bytes, &patch)?;
                    let patched = SourceSnapshot {
                        archive_bytes: patched_bytes,
                        sourceinfo: Some(patched_sourceinfo),
                        pkgbase: raw.pkgbase.clone(),
                    };
                    Arc::new(CacheEntry {
                        commit,
                        active: patched,
                        original: Some(raw),
                        patch: Some(patch),
                    })
                }
            };
            self.cache.lock().await.put(cache_key, entry);
        }
        Ok(changed)
    }

    async fn get_or_fetch(
        &self,
        source_data: &SourceData,
        patch: Option<&str>,
    ) -> anyhow::Result<Arc<CacheEntry>> {
        let cache_key = source_data.cache_key();
        let patch = match patch.map(SourcePatch::parse).transpose()? {
            Some(patch) if !patch.is_empty() => Some(patch),
            _ => None,
        };

        // Fast path: already cached with the exact same patch state.
        {
            let mut cache = self.cache.lock().await;
            if let Some(entry) = cache.get(&cache_key)
                && Self::entry_matches_patch(entry, patch.as_ref())
            {
                return Ok(Arc::clone(entry));
            }
        }

        let (repo_url, git_ref, subfolder) = self.git_coordinates(source_data)?;
        let path = self.checkout_root.join(sanitize_cache_key(&cache_key));
        let (commit, archive_bytes, pkgbase, sourceinfo) =
            checkout_and_parse(&repo_url, &git_ref, &subfolder, &path).await?;
        let raw = SourceSnapshot {
            archive_bytes,
            sourceinfo,
            pkgbase,
        };

        let entry = match patch {
            None => Arc::new(CacheEntry {
                commit,
                active: raw,
                original: None,
                patch: None,
            }),
            Some(patch) => {
                let (patched_bytes, patched_sourceinfo) =
                    apply_patch_to_archive(&raw.archive_bytes, &patch)?;
                let patched = SourceSnapshot {
                    archive_bytes: patched_bytes,
                    sourceinfo: Some(patched_sourceinfo),
                    pkgbase: raw.pkgbase.clone(),
                };
                Arc::new(CacheEntry {
                    commit,
                    active: patched,
                    original: Some(raw),
                    patch: Some(patch),
                })
            }
        };

        self.cache.lock().await.put(cache_key, Arc::clone(&entry));
        Ok(entry)
    }

    /// Fetch (or reuse) the raw checkout for `source_data`, regardless of
    /// whatever patch state (if any) happens to already be cached for it.
    /// Used by [`SnapshotStore::list_files`]/[`SnapshotStore::read_file`],
    /// which only ever need the pristine source (`entry.original()`) and
    /// must not force a redundant re-fetch just because a different/no
    /// patch is currently active in the cache.
    async fn get_or_fetch_any(&self, source_data: &SourceData) -> anyhow::Result<Arc<CacheEntry>> {
        let cache_key = source_data.cache_key();

        {
            let mut cache = self.cache.lock().await;
            if let Some(entry) = cache.get(&cache_key) {
                return Ok(Arc::clone(entry));
            }
        }

        self.get_or_fetch(source_data, None).await
    }

    /// Whether a cached entry already reflects the given patch state (both
    /// `None`, or both set to byte-identical patch content), so the fast
    /// path can be taken without redoing the (cheap but non-free) patch
    /// application step.
    fn entry_matches_patch(entry: &CacheEntry, patch: Option<&SourcePatch>) -> bool {
        entry.patch.as_ref() == patch
    }

    /// Map a `SourceData` to the git coordinates used to fetch it: repo URL,
    /// ref, and subfolder within the repo containing the PKGBUILD/.SRCINFO.
    fn git_coordinates(
        &self,
        source_data: &SourceData,
    ) -> anyhow::Result<(String, String, String)> {
        match source_data {
            SourceData::Aur { name } => Ok((
                format!("{}/{name}.git", self.aur_git_base_url),
                "HEAD".to_string(),
                String::new(),
            )),
            SourceData::Git { spec } => {
                Ok((spec.url.clone(), spec.r#ref.clone(), spec.subfolder.clone()))
            }
            SourceData::Upload { .. } => anyhow::bail!("Upload sources are not yet supported"),
        }
    }
}

/// Turn a `cache_key()` into a filesystem-safe directory name.
fn sanitize_cache_key(cache_key: &str) -> String {
    cache_key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Checkout (or fetch-and-update) `repo_url`@`git_ref` into the persistent
/// `path`, then parse the `.SRCINFO`/PKGBUILD and build a tar.gz archive of
/// `subfolder` (or the repo root if empty) with `{pkgbase}/` as the
/// top-level directory, matching the structure of AUR snapshots.
///
/// Parsing the fetched `.SRCINFO`/PKGBUILD may fail for malformed real-world
/// PKGBUILDs (e.g. `ogdf`) that `alpm-srcinfo` cannot handle. When that
/// happens, `sourceinfo` is `None` rather than the whole fetch failing, so
/// the raw source can still be browsed/edited (and a patch authored to fix
/// the parse failure) before it's ever successfully added as a package. A
/// best-effort `pkgbase` is derived from the repo URL/subfolder in that case.
async fn checkout_and_parse(
    repo_url: &str,
    git_ref: &str,
    subfolder: &str,
    path: &Path,
) -> anyhow::Result<(Oid, Vec<u8>, String, Option<SourceInfoV1>)> {
    use crate::git::checkout::checkout_or_fetch_repo_ref;
    use crate::pkgbuild::parse_pkgbuild;

    let repo_url = repo_url.to_string();
    let git_ref = git_ref.to_string();
    let path = path.to_path_buf();
    let subfolder = subfolder.to_string();

    // git2 types are not `Send`, so the checkout itself must run fully
    // within the blocking closure.
    let (commit, package_dir) = tokio::task::spawn_blocking(move || {
        let commit = checkout_or_fetch_repo_ref(&repo_url, &git_ref, &path)?;
        let package_dir = if subfolder.is_empty() {
            path
        } else {
            path.join(&subfolder)
        };
        anyhow::Ok((commit, package_dir))
    })
    .await??;

    let srcinfo_path = package_dir.join(".SRCINFO");
    let parsed = if srcinfo_path.exists() {
        std::fs::read_to_string(&srcinfo_path)
            .map_err(anyhow::Error::from)
            .and_then(|content| Ok(SourceInfoV1::from_string(&fix_source_urls(&content))?))
    } else {
        parse_pkgbuild(package_dir.join("PKGBUILD").as_path())
    };

    let (pkgbase, sourceinfo) = match parsed {
        Ok(sourceinfo) => (sourceinfo.base.name.to_string(), Some(sourceinfo)),
        Err(_) => (fallback_pkgbase(&package_dir), None),
    };

    let tar_gz_bytes = create_archive_with_pkgbase_dir(&package_dir, &pkgbase)?;

    Ok((commit, tar_gz_bytes, pkgbase, sourceinfo))
}

/// Best-effort pkgbase name to use as the archive's top-level directory when
/// `.SRCINFO`/PKGBUILD parsing fails: read `pkgbase=`/`pkgname=` directly out
/// of the PKGBUILD text, falling back to the checkout directory's name.
fn fallback_pkgbase(package_dir: &Path) -> String {
    if let Ok(content) = std::fs::read_to_string(package_dir.join("PKGBUILD")) {
        for line in content.lines() {
            let line = line.trim();
            for prefix in ["pkgbase=", "pkgname="] {
                if let Some(value) = line.strip_prefix(prefix) {
                    let value = value.trim_matches(['"', '\''].as_ref()).trim();
                    if !value.is_empty() {
                        return value.to_string();
                    }
                }
            }
        }
    }
    package_dir
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "source".to_string())
}

/// Apply `patch` on top of an already-fetched (unpatched) `archive_bytes`
/// tar.gz, regenerating `.SRCINFO` from the patched `PKGBUILD` and
/// re-packaging the result into a new tar.gz with the same `{pkgbase}/`
/// layout.
///
/// This operates entirely in memory: the archive is unpacked into a
/// `{relative path -> bytes}` map, the patch is applied to that map, and the
/// result is re-tarred directly from memory. The lone exception is the
/// (patched) `PKGBUILD`, which is written to a short-lived temp file purely
/// because parsing it shells out to a bash script (`alpm-pkgbuild-bridge`)
/// that needs a real path to `source` - the file is removed again as soon as
/// parsing finishes.
fn apply_patch_to_archive(
    archive_bytes: &[u8],
    patch: &SourcePatch,
) -> anyhow::Result<(Vec<u8>, SourceInfoV1)> {
    use crate::pkgbuild::parse_pkgbuild_content;

    let (pkgbase, mut files) = extract_tar_gz_to_memory(archive_bytes)?;

    for rel_path in patch.paths() {
        let original = files.get(rel_path).cloned().unwrap_or_default();
        let original = String::from_utf8(original)
            .map_err(|_| anyhow::anyhow!("File '{rel_path}' is not valid UTF-8, cannot patch"))?;
        let patched = patch.apply_to_content(rel_path, &original)?;
        files.insert(rel_path.to_string(), patched.into_bytes());
    }

    let pkgbuild = files
        .get("PKGBUILD")
        .ok_or_else(|| anyhow::anyhow!("Archive has no PKGBUILD to parse"))?;
    let pkgbuild = std::str::from_utf8(pkgbuild)
        .map_err(|_| anyhow::anyhow!("PKGBUILD is not valid UTF-8, cannot parse"))?;
    let sourceinfo = parse_pkgbuild_content(pkgbuild)?;

    let tar_gz_bytes = create_archive_from_memory(&pkgbase, &files)?;

    Ok((tar_gz_bytes, sourceinfo))
}

/// Unpack a `{pkgbase}/...` tar.gz archive entirely into memory, returning
/// the pkgbase directory name and a map of file paths (relative to that
/// directory) to their raw bytes.
fn extract_tar_gz_to_memory(
    archive_bytes: &[u8],
) -> anyhow::Result<(String, BTreeMap<String, Vec<u8>>)> {
    let decoder = flate2::read::GzDecoder::new(archive_bytes);
    let mut archive = tar::Archive::new(decoder);

    let mut pkgbase = None;
    let mut files = BTreeMap::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path()?.to_string_lossy().to_string();
        let Some((dir, rel_path)) = path.split_once('/') else {
            continue;
        };
        if pkgbase.is_none() {
            pkgbase = Some(dir.to_string());
        }
        let mut buf = Vec::new();
        entry.read_to_end(&mut buf)?;
        files.insert(rel_path.to_string(), buf);
    }

    let pkgbase = pkgbase
        .ok_or_else(|| anyhow::anyhow!("Extracted archive did not contain a pkgbase directory"))?;
    Ok((pkgbase, files))
}

/// Re-package an in-memory `{relative path -> bytes}` map into a `{pkgbase}/...`
/// tar.gz archive, matching the layout produced by [`extract_tar_gz_to_memory`].
fn create_archive_from_memory(
    pkgbase: &str,
    files: &BTreeMap<String, Vec<u8>>,
) -> anyhow::Result<Vec<u8>> {
    let mut buf = Vec::new();
    let enc = flate2::write::GzEncoder::new(&mut buf, flate2::Compression::default());
    let mut tar = tar::Builder::new(enc);
    for (rel_path, content) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(
            &mut header,
            format!("{pkgbase}/{rel_path}"),
            content.as_slice(),
        )?;
    }
    // Finish explicitly: dropping the encoder would swallow a compression error.
    tar.into_inner()?.finish()?;
    Ok(buf)
}

/// List files (relative to the source root) available for viewing/editing.
///
/// Excludes `.SRCINFO`, which is always regenerated and so is never a
/// meaningful patch target. Git metadata never reaches the archive in the
/// first place (see [`create_archive_with_pkgbase_dir`]).
fn list_files_in_archive(archive_bytes: &[u8], pkgbase: &str) -> anyhow::Result<Vec<String>> {
    let decoder = flate2::read::GzDecoder::new(archive_bytes);
    let mut archive = tar::Archive::new(decoder);

    let prefix = format!("{pkgbase}/");
    let mut files = Vec::new();
    for entry in archive.entries()? {
        let entry = entry?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path()?.to_string_lossy().to_string();
        let Some(rel_path) = path.strip_prefix(&prefix) else {
            continue;
        };
        if rel_path == ".SRCINFO" {
            continue;
        }
        files.push(rel_path.to_string());
    }
    files.sort();
    Ok(files)
}

/// Read a single file's content out of a `{pkgbase}/...` tar.gz archive.
fn read_file_from_archive(
    archive_bytes: &[u8],
    pkgbase: &str,
    rel_path: &str,
) -> anyhow::Result<String> {
    let decoder = flate2::read::GzDecoder::new(archive_bytes);
    let mut archive = tar::Archive::new(decoder);

    let target = format!("{pkgbase}/{rel_path}");
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_string_lossy().to_string();
        if path == target {
            let mut buf = String::new();
            entry.read_to_string(&mut buf)?;
            return Ok(buf);
        }
    }
    anyhow::bail!("File '{rel_path}' not found in source")
}

/// Package `source_dir` as a tar.gz whose single top-level directory is
/// `pkgbase`, matching the layout of an AUR snapshot.
///
/// `source_dir` is the persistent git checkout, so its git metadata sits right
/// next to the sources. `.git` is excluded: the archive is a source snapshot
/// served to workers, and shipping it would attach the repository's entire
/// history to every job download. Only the top level is filtered, which is
/// sufficient because the checkout is a plain clone with no submodules.
fn create_archive_with_pkgbase_dir(source_dir: &Path, pkgbase: &str) -> anyhow::Result<Vec<u8>> {
    let mut buf = Vec::new();
    let enc = flate2::write::GzEncoder::new(&mut buf, flate2::Compression::default());
    let mut tar = tar::Builder::new(enc);

    let root = Path::new(pkgbase);
    tar.append_dir(root, source_dir)?;
    for entry in std::fs::read_dir(source_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == GIT_METADATA_DIR {
            continue;
        }
        let dest = root.join(&name);
        if entry.file_type()?.is_dir() {
            tar.append_dir_all(dest, entry.path())?;
        } else {
            tar.append_path_with_name(entry.path(), dest)?;
        }
    }

    // Finish explicitly: dropping the encoder would swallow a compression error.
    tar.into_inner()?.finish()?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patch::SourcePatch;
    use git2::{Repository, Signature};

    const PKGBUILD: &str = "\
pkgname=foo
pkgbase=foo
pkgver=1.0
pkgrel=1
pkgdesc=\"A test package\"
url=\"https://example.org/\"
arch=('x86_64')
license=('MIT')
";

    fn make_fixture_archive(pkgbase: &str, pkgbuild: &str) -> Vec<u8> {
        let dir = tempfile::tempdir().unwrap();
        let package_dir = dir.path().join(pkgbase);
        std::fs::create_dir_all(&package_dir).unwrap();
        std::fs::write(package_dir.join("PKGBUILD"), pkgbuild).unwrap();
        // Include a stale/mismatched .SRCINFO to prove it's ignored once a
        // patch is applied - the regenerated one must come from PKGBUILD.
        std::fs::write(
            package_dir.join(".SRCINFO"),
            "pkgbase = foo\n\tpkgver = 0.0.1\n\tpkgrel = 1\n\narch = x86_64\n\npkgname = foo\n",
        )
        .unwrap();
        create_archive_with_pkgbase_dir(&package_dir, pkgbase).unwrap()
    }

    /// `parse_pkgbuild` shells out to an external bridge script that's only
    /// guaranteed to be present inside the builder image (see
    /// `docker/Dockerfile`), not in plain `cargo test` environments. Skip
    /// tests that need it when it's missing instead of failing the suite.
    ///
    /// Anything that applies a `SourcePatch` needs it too: patching regenerates
    /// `.SRCINFO` from the patched `PKGBUILD` via the same bridge.
    fn pkgbuild_bridge_available() -> bool {
        std::env::var_os("PATH").is_some_and(|paths| {
            std::env::split_paths(&paths).any(|dir| dir.join("alpm-pkgbuild-bridge").is_file())
        })
    }

    /// Every entry path in an archive, directories included.
    ///
    /// Deliberately raw rather than going through [`list_files_in_archive`],
    /// which filters for the UI: the point is to assert on what the tarball
    /// actually carries.
    fn archive_entry_paths(archive_bytes: &[u8]) -> Vec<String> {
        let decoder = flate2::read::GzDecoder::new(archive_bytes);
        let mut archive = tar::Archive::new(decoder);
        archive
            .entries()
            .unwrap()
            .map(|entry| entry.unwrap().path().unwrap().to_string_lossy().to_string())
            .collect()
    }

    fn parse_srcinfo_from_archive(archive_bytes: &[u8], pkgbase: &str) -> SourceInfoV1 {
        let content = read_file_from_archive(archive_bytes, pkgbase, ".SRCINFO").unwrap();
        SourceInfoV1::from_string(&fix_source_urls(&content)).unwrap()
    }

    #[test]
    fn apply_patch_to_archive_regenerates_srcinfo_from_patched_pkgbuild() {
        if !pkgbuild_bridge_available() {
            eprintln!("skipping: alpm-pkgbuild-bridge not found on PATH");
            return;
        }

        let archive_bytes = make_fixture_archive("foo", PKGBUILD);

        let mut patch = SourcePatch::default();
        let patched_pkgbuild = PKGBUILD.replace("pkgver=1.0", "pkgver=2.0");
        patch.merge_file("PKGBUILD", PKGBUILD, &patched_pkgbuild);

        let (new_archive_bytes, sourceinfo) =
            apply_patch_to_archive(&archive_bytes, &patch).unwrap();

        // .SRCINFO was regenerated from the patched PKGBUILD (version 2.0),
        // not copied over from the (stale, unpatched) shipped .SRCINFO.
        assert_eq!(sourceinfo.base.version.to_string(), "2.0-1");

        let files = list_files_in_archive(&new_archive_bytes, "foo").unwrap();
        assert!(files.contains(&"PKGBUILD".to_string()));
        // The shipped .SRCINFO must never be surfaced as an editable/patchable file.
        assert!(!files.contains(&".SRCINFO".to_string()));

        let pkgbuild_content =
            read_file_from_archive(&new_archive_bytes, "foo", "PKGBUILD").unwrap();
        assert_eq!(pkgbuild_content, patched_pkgbuild);
    }

    #[test]
    fn snapshot_store_falls_back_to_shipped_srcinfo_without_patch() {
        // Without a patch, list_files/read_file/sourceinfo & archive_bytes on
        // an AUR-shaped source all operate on the raw fetched archive (i.e.
        // the shipped, possibly stale, .SRCINFO is what's used).
        let archive_bytes = make_fixture_archive("foo", PKGBUILD);
        let sourceinfo = parse_srcinfo_from_archive(&archive_bytes, "foo");
        assert_eq!(sourceinfo.base.version.to_string(), "0.0.1-1");
    }

    /// Create a local git repository at `aur_root/{pkgbase}.git` containing
    /// a PKGBUILD + a matching `.SRCINFO`, standing in for the real AUR git
    /// remote in tests.
    fn create_aur_git_repo(aur_root: &Path, pkgbase: &str, version: &str) -> PathBuf {
        let repo_path = aur_root.join(format!("{pkgbase}.git"));
        let repo = Repository::init(&repo_path).unwrap();

        let pkgbuild = format!(
            "pkgname={pkgbase}\npkgver={version}\npkgrel=1\narch=('x86_64')\ndepends=()\nsource=()\nsha256sums=()\npackage() {{\n  :\n}}\n"
        );
        let srcinfo = format!(
            "pkgbase = {pkgbase}\n\tpkgver = {version}\n\tpkgrel = 1\n\narch = x86_64\n\npkgname = {pkgbase}\n"
        );

        std::fs::write(repo_path.join("PKGBUILD"), pkgbuild).unwrap();
        std::fs::write(repo_path.join(".SRCINFO"), srcinfo).unwrap();

        let mut index = repo.index().unwrap();
        index.add_path(Path::new("PKGBUILD")).unwrap();
        index.add_path(Path::new(".SRCINFO")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = Signature::now("Test", "test@example.com").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();

        repo_path
    }

    /// Build a `SnapshotStore` for tests: AUR sources resolve against local
    /// git repos under `aur_root` instead of the real AUR.
    fn test_store(aur_root: &Path) -> (SnapshotStore, tempfile::TempDir) {
        let checkout_dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::with_checkout_root_and_aur_base(
            checkout_dir.path().to_path_buf(),
            aur_root.to_string_lossy().to_string(),
        );
        (store, checkout_dir)
    }

    fn some_patch(new_pkgver: &str) -> SourcePatch {
        let original =
            "pkgname=bar\npkgver=1.0\npkgrel=1\narch=('x86_64')\ndepends=()\nsource=()\nsha256sums=()\npackage() {\n  :\n}\n"
                .to_string();
        let patched = original.replace("pkgver=1.0", &format!("pkgver={new_pkgver}"));
        let mut patch = SourcePatch::default();
        patch.merge_file("PKGBUILD", &original, &patched);
        patch
    }

    /// Commit a PKGBUILD (with a fixed pkgbase of `bar`, version `1.0`) plus
    /// a matching `.SRCINFO` to `main` in `repo`, creating the branch on the
    /// first call and extending its history thereafter. `marker` is added as
    /// a harmless trailing comment so each commit's tree differs.
    fn commit_to_repo(repo: &Repository, marker: &str, message: &str) {
        let pkgbuild = format!(
            "pkgname=bar\npkgver=1.0\npkgrel=1\narch=('x86_64')\ndepends=()\nsource=()\nsha256sums=()\npackage() {{\n  :\n}}\n# {marker}\n"
        );
        let srcinfo =
            "pkgbase = bar\n\tpkgver = 1.0\n\tpkgrel = 1\n\narch = x86_64\n\npkgname = bar\n";

        std::fs::write(repo.workdir().unwrap().join("PKGBUILD"), pkgbuild).unwrap();
        std::fs::write(repo.workdir().unwrap().join(".SRCINFO"), srcinfo).unwrap();

        let mut index = repo.index().unwrap();
        index.add_path(Path::new("PKGBUILD")).unwrap();
        index.add_path(Path::new(".SRCINFO")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = Signature::now("Test", "test@example.com").unwrap();
        let parents = repo
            .head()
            .ok()
            .and_then(|head| head.target())
            .and_then(|oid| repo.find_commit(oid).ok())
            .map(|commit| vec![commit])
            .unwrap_or_default();
        let parent_refs = parents.iter().collect::<Vec<_>>();
        repo.commit(
            Some("refs/heads/main"),
            &sig,
            &sig,
            message,
            &tree,
            &parent_refs,
        )
        .unwrap();
        repo.set_head("refs/heads/main").unwrap();
        repo.checkout_head(None).unwrap();
    }

    /// The source archive served to workers must not carry the checkout's git
    /// metadata: `.git` holds the repository's entire history, and the archive
    /// is downloaded fresh for every build job.
    ///
    /// This drives the real path — resolve source, clone into the persistent
    /// checkout, build the archive — so a regression anywhere along it fails
    /// here rather than only showing up as bloated job downloads.
    #[tokio::test]
    async fn fetched_archive_excludes_git_metadata() {
        let aur_root = tempfile::tempdir().unwrap();
        create_aur_git_repo(aur_root.path(), "bar", "1.0");
        let (store, checkout_dir) = test_store(aur_root.path());
        let source = SourceData::Aur {
            name: "bar".to_string(),
        };

        let archive = store.archive_bytes(&source, None).await.unwrap();

        // Guard against the assertion below passing for the wrong reason: the
        // checkout the archive was built from really does have a `.git` dir.
        let checkout = checkout_dir
            .path()
            .join(sanitize_cache_key(&source.cache_key()));
        assert!(
            checkout.join(".git").is_dir(),
            "fixture checkout has no git metadata to exclude"
        );

        let entries = archive_entry_paths(&archive);
        assert!(
            entries.iter().any(|path| path == "bar/PKGBUILD"),
            "sources are missing from the archive: {entries:?}"
        );
        assert!(
            !entries
                .iter()
                .any(|path| path == "bar/.git" || path.starts_with("bar/.git/")),
            "archive must not carry git metadata: {entries:?}"
        );
    }

    /// Regression test for a bug introduced (and fixed) while reworking the
    /// cache to hold a single entry per source: `list_files`/`read_file`
    /// must reuse whatever is already cached for a source - regardless of
    /// whether a patch happens to be active for it - rather than treating a
    /// patch-vs-no-patch mismatch as a cache miss and forcing a redundant
    /// re-fetch from the (in this test, local-only) git remote.
    #[tokio::test]
    async fn list_files_and_read_file_reuse_cache_regardless_of_active_patch() {
        if !pkgbuild_bridge_available() {
            eprintln!("skipping: alpm-pkgbuild-bridge not found on PATH");
            return;
        }

        let aur_root = tempfile::tempdir().unwrap();
        create_aur_git_repo(aur_root.path(), "bar", "1.0");
        let (store, _checkout_dir) = test_store(aur_root.path());
        let source = SourceData::Aur {
            name: "bar".to_string(),
        };

        // Prime the cache with a *patched* entry (active = patched content).
        let patch = some_patch("2.0");
        let patch_json = patch.to_json().unwrap();
        let sourceinfo = store.sourceinfo(&source, Some(&patch_json)).await.unwrap();
        assert_eq!(sourceinfo.base.version.to_string(), "2.0-1");

        // Once the upstream git remote is gone, any code path that would
        // force a re-fetch (rather than reusing the cached entry) fails
        // here - proving list_files/read_file only ever reuse the cache.
        std::fs::remove_dir_all(aur_root.path().join("bar.git")).unwrap();

        let files = store.list_files(&source).await.unwrap();
        assert!(files.contains(&"PKGBUILD".to_string()));

        // read_file(patch=None) must return the *pristine* content (pkgver
        // 1.0), not the currently-active patched content (pkgver 2.0),
        // proving `original` was correctly preserved alongside `active`.
        let pristine = store.read_file(&source, None, "PKGBUILD").await.unwrap();
        assert!(pristine.contains("pkgver=1.0"));
        assert!(!pristine.contains("pkgver=2.0"));

        // read_file with the same patch re-applied must return the patched
        // content, computed from the still-cached pristine original.
        let patched = store
            .read_file(&source, Some(&patch_json), "PKGBUILD")
            .await
            .unwrap();
        assert!(patched.contains("pkgver=2.0"));
    }

    /// There is at most one cache entry per source at a time: switching from
    /// no patch, to a patch, and back to no patch must each fully replace
    /// the previous entry rather than accumulating stale variants that could
    /// be read back by mistake.
    #[tokio::test]
    async fn at_most_one_cache_entry_per_source_across_patch_changes() {
        if !pkgbuild_bridge_available() {
            eprintln!("skipping: alpm-pkgbuild-bridge not found on PATH");
            return;
        }

        let aur_root = tempfile::tempdir().unwrap();
        create_aur_git_repo(aur_root.path(), "bar", "1.0");
        let (store, _checkout_dir) = test_store(aur_root.path());
        let source = SourceData::Aur {
            name: "bar".to_string(),
        };

        // No patch: active == pristine.
        let unpatched = store.sourceinfo(&source, None).await.unwrap();
        assert_eq!(unpatched.base.version.to_string(), "1.0-1");
        assert_eq!(store.cache.lock().await.len(), 1);

        // Apply patch A: single entry now reflects patch A.
        let patch_a = some_patch("2.0").to_json().unwrap();
        let a = store.sourceinfo(&source, Some(&patch_a)).await.unwrap();
        assert_eq!(a.base.version.to_string(), "2.0-1");
        assert_eq!(store.cache.lock().await.len(), 1);

        // Switch to patch B: the entry for patch A must be gone - reading
        // with patch B must not somehow see stale patch-A content, and only
        // one entry must exist for this source.
        let patch_b = some_patch("3.0").to_json().unwrap();
        let b = store.sourceinfo(&source, Some(&patch_b)).await.unwrap();
        assert_eq!(b.base.version.to_string(), "3.0-1");
        assert_eq!(store.cache.lock().await.len(), 1);

        // Clearing the patch must revert to the pristine content, not
        // whatever the last-active patch happened to produce.
        let cleared = store.sourceinfo(&source, None).await.unwrap();
        assert_eq!(cleared.base.version.to_string(), "1.0-1");
        assert_eq!(store.cache.lock().await.len(), 1);
    }

    /// `refresh` must re-apply whichever patch was active before the
    /// refresh, rather than silently reverting the source to unpatched.
    #[tokio::test]
    async fn refresh_reapplies_previously_active_patch() {
        if !pkgbuild_bridge_available() {
            eprintln!("skipping: alpm-pkgbuild-bridge not found on PATH");
            return;
        }

        // Use a plain Git source (rather than an AUR one) so the "main"
        // branch ref reliably reflects newly pushed commits, matching the
        // pattern used elsewhere in this crate for exercising
        // `checkout_or_fetch_repo_ref`'s fetch-and-update behavior.
        let repo_dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(repo_dir.path()).unwrap();
        commit_to_repo(&repo, "v1", "init");

        let (store, _checkout_dir) = test_store(Path::new("unused"));
        let source = SourceData::Git {
            spec: aurcache_db::packages::GitSourceSpec {
                url: repo_dir.path().to_string_lossy().to_string(),
                r#ref: "origin/main".to_string(),
                subfolder: String::new(),
            },
        };

        let patch = some_patch("2.0").to_json().unwrap();
        store.sourceinfo(&source, Some(&patch)).await.unwrap();

        // Upstream moves to a new commit (still parseable as version 1.0,
        // since the patch bumps it to 2.0 - if refresh dropped the patch,
        // this would come back as 1.0 instead of 2.0).
        commit_to_repo(&repo, "v2", "bump");

        let changed = store.refresh(&source).await.unwrap();
        assert!(changed, "refresh should detect the new upstream commit");

        let after_refresh = store.sourceinfo(&source, Some(&patch)).await.unwrap();
        assert_eq!(after_refresh.base.version.to_string(), "2.0-1");
    }
}
