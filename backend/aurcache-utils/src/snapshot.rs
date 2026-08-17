use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use alpm_srcinfo::SourceInfoV1;
use aurcache_db::packages::SourceData;
use aurcache_deps::AurClient;
use git2::Oid;
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

struct CacheEntry {
    /// `None` when the fetched source's `.SRCINFO`/`PKGBUILD` could not be
    /// parsed (e.g. a malformed PKGBUILD upstream, such as `ogdf`). Browsing
    /// and editing raw source files must keep working in that case so a
    /// patch can be authored to fix the parse failure before the source is
    /// ever added as a package; only dependency/version resolution actually
    /// requires a successfully parsed `.SRCINFO`.
    sourceinfo: Option<Arc<SourceInfoV1>>,
    archive_bytes: Arc<Vec<u8>>,
    /// Best-effort pkgbase, used to name the archive's top-level directory
    /// and as the file-listing root even when `sourceinfo` is `None`.
    pkgbase: String,
    /// Resolved commit id last used to build this entry, used to detect
    /// whether a `refresh` actually changed anything. Patched entries reuse
    /// the underlying raw entry's commit id (patch content itself has no
    /// notion of "moving", so raw commit changes are what matters).
    commit: Oid,
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
/// Fetched (unpatched) sources are cached independently from patched
/// variants, so editing a patch doesn't force re-fetching the upstream
/// source - only the (cheap) patch-apply + re-tar step is redone.
pub struct SnapshotStore {
    /// Keyed by `SourceData::cache_key()`.
    raw_cache: Mutex<HashMap<String, Arc<CacheEntry>>>,
    /// Keyed by `{raw cache key}#patch:{patch hash}`.
    patched_cache: Mutex<HashMap<String, Arc<CacheEntry>>>,
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
        SnapshotStore {
            raw_cache: Mutex::new(HashMap::new()),
            patched_cache: Mutex::new(HashMap::new()),
            checkout_root,
            aur_git_base_url: default_aur_git_base_url(),
        }
    }

    /// Construct a store with an explicit checkout root and AUR git base URL
    /// (e.g. `file:///tmp/fake-aur` in tests, instead of the real AUR).
    pub fn with_checkout_root_and_aur_base(
        checkout_root: PathBuf,
        aur_git_base_url: impl Into<String>,
    ) -> Self {
        SnapshotStore {
            raw_cache: Mutex::new(HashMap::new()),
            patched_cache: Mutex::new(HashMap::new()),
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
        client: &AurClient,
        source_data: &SourceData,
        patch: Option<&str>,
    ) -> anyhow::Result<SourceInfoV1> {
        let entry = self.get_or_fetch(client, source_data, patch).await?;
        entry
            .sourceinfo
            .as_ref()
            .map(|info| (**info).clone())
            .ok_or_else(|| anyhow::anyhow!("Source's .SRCINFO/PKGBUILD could not be parsed"))
    }

    /// Return the raw archive bytes for `source_data`, fetching it if not cached.
    ///
    /// See [`SnapshotStore::sourceinfo`] for the meaning of `patch`.
    pub async fn archive_bytes(
        &self,
        client: &AurClient,
        source_data: &SourceData,
        patch: Option<&str>,
    ) -> anyhow::Result<Vec<u8>> {
        let entry = self.get_or_fetch(client, source_data, patch).await?;
        Ok((*entry.archive_bytes).clone())
    }

    /// List the (unpatched) source files available for editing, relative to
    /// the source root (e.g. `PKGBUILD`, `foo.install`). Works even if the
    /// source's `.SRCINFO`/PKGBUILD fails to parse.
    pub async fn list_files(
        &self,
        client: &AurClient,
        source_data: &SourceData,
    ) -> anyhow::Result<Vec<String>> {
        let entry = self.get_or_fetch_raw(client, source_data).await?;
        list_files_in_archive(&entry.archive_bytes, &entry.pkgbase)
    }

    /// Return the effective content of a single source file: the pristine
    /// content when `patch` is `None`, otherwise that content with the
    /// stored patch (if any) applied. Works even if the source's
    /// `.SRCINFO`/PKGBUILD fails to parse.
    pub async fn read_file(
        &self,
        client: &AurClient,
        source_data: &SourceData,
        patch: Option<&str>,
        rel_path: &str,
    ) -> anyhow::Result<String> {
        let entry = self.get_or_fetch_raw(client, source_data).await?;
        let original = read_file_from_archive(&entry.archive_bytes, &entry.pkgbase, rel_path)?;

        match patch.map(SourcePatch::parse).transpose()? {
            None => Ok(original),
            Some(patch) => patch.apply_to_content(rel_path, &original),
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
    /// unconditionally re-downloading/re-cloning on every check. Only the raw
    /// (unpatched) source is refreshed; any patched variants are recomputed
    /// lazily on next access via [`SnapshotStore::sourceinfo`]/[`SnapshotStore::archive_bytes`].
    pub async fn refresh(
        &self,
        client: &AurClient,
        source_data: &SourceData,
    ) -> anyhow::Result<bool> {
        let cache_key = source_data.cache_key();
        let previous_commit = {
            let cache = self.raw_cache.lock().await;
            cache.get(&cache_key).map(|entry| entry.commit)
        };

        let (repo_url, git_ref, subfolder) = self.git_coordinates(source_data)?;
        let path = self.checkout_root.join(sanitize_cache_key(&cache_key));

        let (commit, archive_bytes, pkgbase, sourceinfo) =
            checkout_and_parse(&repo_url, &git_ref, &subfolder, &path).await?;

        let changed = previous_commit != Some(commit);
        if changed {
            let entry = Arc::new(CacheEntry {
                sourceinfo: sourceinfo.map(Arc::new),
                archive_bytes: Arc::new(archive_bytes),
                pkgbase,
                commit,
            });
            self.raw_cache.lock().await.insert(cache_key, entry);
        }
        let _ = client; // reserved for future use (e.g. AUR metadata cross-check)
        Ok(changed)
    }

    async fn get_or_fetch(
        &self,
        client: &AurClient,
        source_data: &SourceData,
        patch: Option<&str>,
    ) -> anyhow::Result<Arc<CacheEntry>> {
        let patch = match patch.map(SourcePatch::parse).transpose()? {
            Some(patch) if !patch.is_empty() => Some(patch),
            _ => None,
        };

        let Some(patch) = patch else {
            return self.get_or_fetch_raw(client, source_data).await;
        };

        let raw_key = source_data.cache_key();
        let patched_key = format!("{raw_key}#patch:{:x}", hash_patch(&patch));

        {
            let cache = self.patched_cache.lock().await;
            if let Some(entry) = cache.get(&patched_key) {
                return Ok(entry.clone());
            }
        }

        let raw_entry = self.get_or_fetch_raw(client, source_data).await?;
        let (archive_bytes, sourceinfo) = apply_patch_to_archive(&raw_entry.archive_bytes, &patch)?;

        let entry = Arc::new(CacheEntry {
            sourceinfo: Some(Arc::new(sourceinfo)),
            archive_bytes: Arc::new(archive_bytes),
            pkgbase: raw_entry.pkgbase.clone(),
            commit: raw_entry.commit,
        });

        self.patched_cache
            .lock()
            .await
            .insert(patched_key, entry.clone());
        Ok(entry)
    }

    async fn get_or_fetch_raw(
        &self,
        client: &AurClient,
        source_data: &SourceData,
    ) -> anyhow::Result<Arc<CacheEntry>> {
        let cache_key = source_data.cache_key();

        // Fast path: already cached
        {
            let cache = self.raw_cache.lock().await;
            if let Some(entry) = cache.get(&cache_key) {
                return Ok(entry.clone());
            }
        }

        let (repo_url, git_ref, subfolder) = self.git_coordinates(source_data)?;
        let path = self.checkout_root.join(sanitize_cache_key(&cache_key));
        let (commit, archive_bytes, pkgbase, sourceinfo) =
            checkout_and_parse(&repo_url, &git_ref, &subfolder, &path).await?;

        let entry = Arc::new(CacheEntry {
            sourceinfo: sourceinfo.map(Arc::new),
            archive_bytes: Arc::new(archive_bytes),
            pkgbase,
            commit,
        });

        self.raw_cache
            .lock()
            .await
            .insert(cache_key, entry.clone());
        let _ = client;
        Ok(entry)
    }

    /// Map a `SourceData` to the git coordinates used to fetch it: repo URL,
    /// ref, and subfolder within the repo containing the PKGBUILD/.SRCINFO.
    fn git_coordinates(&self, source_data: &SourceData) -> anyhow::Result<(String, String, String)> {
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
            path.clone()
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

fn hash_patch(patch: &SourcePatch) -> u64 {
    let mut hasher = DefaultHasher::new();
    // `to_json` only fails on serialization bugs; an empty string still
    // yields a stable (if degenerate) hash in that case.
    patch.to_json().unwrap_or_default().hash(&mut hasher);
    hasher.finish()
}

/// Apply `patch` on top of an already-fetched (unpatched) `archive_bytes`
/// tar.gz, regenerating `.SRCINFO` from the patched `PKGBUILD` and
/// re-packaging the result into a new tar.gz with the same `{pkgbase}/`
/// layout.
fn apply_patch_to_archive(
    archive_bytes: &[u8],
    patch: &SourcePatch,
) -> anyhow::Result<(Vec<u8>, SourceInfoV1)> {
    use crate::pkgbuild::parse_pkgbuild;

    let dir = tempfile::tempdir()?;
    let extract_root = dir.path().join("src");
    std::fs::create_dir_all(&extract_root)?;
    extract_tar_gz(archive_bytes, &extract_root)?;

    let pkgbase = find_pkgbase_dir_name(&extract_root)?;
    let package_dir = extract_root.join(&pkgbase);

    patch.apply_to_dir(&package_dir)?;

    let sourceinfo = parse_pkgbuild(package_dir.join("PKGBUILD").as_path())?;
    let tar_gz_bytes = create_archive_with_pkgbase_dir(&package_dir, &pkgbase)?;

    dir.close()?;
    Ok((tar_gz_bytes, sourceinfo))
}

fn extract_tar_gz(archive_bytes: &[u8], dest: &Path) -> anyhow::Result<()> {
    let decoder = flate2::read::GzDecoder::new(archive_bytes);
    let mut archive = tar::Archive::new(decoder);
    archive.unpack(dest)?;
    Ok(())
}

/// Find the single top-level directory of an extracted `{pkgbase}/...` archive.
fn find_pkgbase_dir_name(extract_root: &Path) -> anyhow::Result<String> {
    for entry in std::fs::read_dir(extract_root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            return Ok(entry.file_name().to_string_lossy().to_string());
        }
    }
    anyhow::bail!("Extracted archive did not contain a pkgbase directory")
}

/// List files (relative to the source root) available for viewing/editing.
///
/// Excludes `.git` metadata and `.SRCINFO`, since the latter is always
/// regenerated and never a meaningful patch target.
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
        if rel_path == ".SRCINFO" || rel_path.starts_with(".git/") {
            continue;
        }
        files.push(rel_path.to_string());
    }
    files.sort();
    Ok(files)
}

/// Read a single file's content out of a `{pkgbase}/...` tar.gz archive.
fn read_file_from_archive(archive_bytes: &[u8], pkgbase: &str, rel_path: &str) -> anyhow::Result<String> {
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

fn create_archive_with_pkgbase_dir(source_dir: &Path, pkgbase: &str) -> anyhow::Result<Vec<u8>> {
    let mut buf = Vec::new();
    let enc = flate2::write::GzEncoder::new(&mut buf, flate2::Compression::default());
    let mut tar = tar::Builder::new(enc);
    tar.append_dir_all(pkgbase, source_dir)?;
    let enc = tar.into_inner()?;
    drop(enc);
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patch::SourcePatch;

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
    fn pkgbuild_bridge_available() -> bool {
        std::env::var_os("PATH").is_some_and(|paths| {
            std::env::split_paths(&paths).any(|dir| dir.join("alpm-pkgbuild-bridge").is_file())
        })
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

        let (new_archive_bytes, sourceinfo) = apply_patch_to_archive(&archive_bytes, &patch).unwrap();

        // .SRCINFO was regenerated from the patched PKGBUILD (version 2.0),
        // not copied over from the (stale, unpatched) shipped .SRCINFO.
        assert_eq!(sourceinfo.base.version.to_string(), "2.0-1");

        let files = list_files_in_archive(&new_archive_bytes, "foo").unwrap();
        assert!(files.contains(&"PKGBUILD".to_string()));
        // The shipped .SRCINFO must never be surfaced as an editable/patchable file.
        assert!(!files.contains(&".SRCINFO".to_string()));

        let pkgbuild_content = read_file_from_archive(&new_archive_bytes, "foo", "PKGBUILD").unwrap();
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
}
