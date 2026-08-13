use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use alpm_compress::tarball::TarballReader;
use url::Url;

use crate::client::AurClient;
use crate::deps::parse_dep;
use crate::model::Error;

const OFFICIAL_REPO_NAMES: &[&str] = &["core", "extra", "multilib"];
const OFFICIAL_REPO_CACHE_TTL_SECS: u64 = 60 * 60;

pub(crate) fn default_repo_root() -> PathBuf {
    std::env::var("AURCACHE_REPO_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./repo"))
}

pub(crate) fn default_official_mirrorlist_path() -> PathBuf {
    if let Ok(path) = std::env::var("OFFICIAL_MIRRORLIST_PATH") {
        return PathBuf::from(path);
    }

    let base = std::env::var("MIRRORLIST_PATH_X86_64")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./config/pacman_x86_64"));
    base.join("mirrorlist")
}

pub(crate) fn default_official_repo_cache_dir() -> PathBuf {
    if let Ok(path) = std::env::var("OFFICIAL_REPO_CACHE_DIR") {
        return PathBuf::from(path);
    }

    default_official_mirrorlist_path()
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("official_repo_cache")
}

impl AurClient {
    pub(crate) fn local_repo_dependency_exists(&self, dep_name: &str) -> Result<bool, Error> {
        if !self.repo_root.exists() {
            return Ok(false);
        }

        let mut archives = Vec::new();
        for entry in fs::read_dir(&self.repo_root).map_err(|e| Error::Rpc(e.to_string()))? {
            let entry = entry.map_err(|e| Error::Rpc(e.to_string()))?;
            archives.push(entry.path().join("repo.db.tar.gz"));
        }

        any_archive_provides(archives, dep_name)
    }

    pub(crate) async fn cached_official_dependency_exists(
        &self,
        dep_name: &str,
    ) -> Result<bool, Error> {
        self.refresh_official_repo_cache_if_needed().await?;
        let archives = OFFICIAL_REPO_NAMES.iter().map(|repo_name| {
            self.official_repo_cache_dir
                .join(cache_file_name(repo_name))
        });
        any_archive_provides(archives, dep_name)
    }

    async fn refresh_official_repo_cache_if_needed(&self) -> Result<(), Error> {
        fs::create_dir_all(&self.official_repo_cache_dir).map_err(|e| Error::Rpc(e.to_string()))?;
        let mirrors = mirror_servers(&self.official_mirrorlist_path)?;
        if mirrors.is_empty() {
            return Err(Error::Rpc(
                "No official repo mirrors configured".to_string(),
            ));
        }

        for repo_name in OFFICIAL_REPO_NAMES {
            let archive_path = self
                .official_repo_cache_dir
                .join(cache_file_name(repo_name));
            if !cache_is_stale(&archive_path)? {
                continue;
            }
            self.download_official_repo_db(&mirrors, repo_name, &archive_path)
                .await?;
        }

        Ok(())
    }

    async fn download_official_repo_db(
        &self,
        mirrors: &[String],
        repo_name: &str,
        archive_path: &Path,
    ) -> Result<(), Error> {
        let mut last_error = None;
        for mirror in mirrors {
            let url = official_repo_db_url(mirror, repo_name)?;
            match self.download_to_path(&url, archive_path).await {
                Ok(()) => return Ok(()),
                Err(err) => last_error = Some(err),
            }
        }

        Err(last_error
            .unwrap_or_else(|| Error::Rpc("Failed to download official repo db".to_string())))
    }

    async fn download_to_path(&self, url: &Url, archive_path: &Path) -> Result<(), Error> {
        let bytes = self
            .http
            .get(url.clone())
            .send()
            .await
            .map_err(Error::Http)?
            .error_for_status()
            .map_err(Error::Http)?
            .bytes()
            .await
            .map_err(Error::Http)?;
        fs::write(archive_path, bytes).map_err(|e| Error::Rpc(e.to_string()))?;
        Ok(())
    }
}

fn cache_is_stale(path: &Path) -> Result<bool, Error> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(err) => return Err(Error::Rpc(err.to_string())),
    };
    let modified = metadata.modified().map_err(|e| Error::Rpc(e.to_string()))?;
    let age = SystemTime::now()
        .duration_since(modified)
        .map_err(|e| Error::Rpc(e.to_string()))?;
    Ok(age.as_secs() > OFFICIAL_REPO_CACHE_TTL_SECS)
}

fn mirror_servers(path: &Path) -> Result<Vec<String>, Error> {
    let content = fs::read_to_string(path).map_err(|e| Error::Rpc(e.to_string()))?;
    Ok(content
        .lines()
        .map(str::trim)
        .filter_map(|line| line.strip_prefix("Server = "))
        .map(str::trim)
        .map(ToString::to_string)
        .collect())
}

fn official_repo_db_url(mirror: &str, repo_name: &str) -> Result<Url, Error> {
    let base = mirror
        .replace("$repo", repo_name)
        .replace("$arch", "x86_64");
    let separator = if base.ends_with('/') { "" } else { "/" };
    Url::parse(&format!("{base}{separator}{repo_name}.db")).map_err(|e| Error::Rpc(e.to_string()))
}

fn cache_file_name(repo_name: &str) -> String {
    format!("{repo_name}.db.tar.gz")
}

/// Returns true if any of the given `repo.db`-style archives provides `dep_name`
/// (by package name or `%PROVIDES%`). Missing archive paths are skipped.
fn any_archive_provides(
    archive_paths: impl IntoIterator<Item = PathBuf>,
    dep_name: &str,
) -> Result<bool, Error> {
    for archive_path in archive_paths {
        if !archive_path.exists() {
            continue;
        }
        if repo_archive_provides(&archive_path, dep_name)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn repo_archive_provides(archive_path: &Path, dep_name: &str) -> Result<bool, Error> {
    let mut reader =
        TarballReader::try_from(archive_path).map_err(|e| Error::Rpc(e.to_string()))?;
    for entry in reader.entries().map_err(|e| Error::Rpc(e.to_string()))? {
        let mut entry = entry.map_err(|e| Error::Rpc(e.to_string()))?;
        if entry.path().file_name().and_then(|name| name.to_str()) != Some("desc") {
            continue;
        }

        let content = String::from_utf8(entry.content().map_err(|e| Error::Rpc(e.to_string()))?)
            .map_err(|e| Error::Rpc(e.to_string()))?;
        if desc_matches_dependency(&content, dep_name) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Parses `name` and `provides` from a desc file content string without strict validation,
/// so it works for both signed and unsigned packages (no %PGPSIG% required).
fn desc_matches_dependency(content: &str, dep_name: &str) -> bool {
    let sections = parse_desc_sections(content);
    let name = sections
        .get("NAME")
        .and_then(|v| v.first())
        .cloned()
        .unwrap_or_default();
    if name == dep_name {
        return true;
    }
    let provides = sections.get("PROVIDES").cloned().unwrap_or_default();
    provides.iter().any(|p| parse_dep(p).0 == dep_name)
}

/// Extracts all sections from a pacman desc file into a map of section name → values.
/// Each section has the form `%SECTION_NAME%\nval1\nval2\n\n`.
///
/// We parse desc files manually rather than using `alpm-repo-db` because that crate
/// auto-detects the schema version by the presence of `%MD5SUM%`: entries with it are
/// treated as v1, which requires `%PGPSIG%`. AURCache doesn't sign packages, so
/// `%PGPSIG%` is always absent and `alpm-repo-db` would fail on every local repo entry.
/// Since we only need `%NAME%` and `%PROVIDES%` for dependency resolution, a lenient
/// section extractor is both simpler and more robust.
fn parse_desc_sections(content: &str) -> std::collections::HashMap<String, Vec<String>> {
    let mut map = std::collections::HashMap::new();
    let mut current_key: Option<String> = None;
    let mut current_values: Vec<String> = Vec::new();

    for line in content.lines() {
        if line.starts_with('%') && line.ends_with('%') {
            if let Some(key) = current_key.take() {
                map.insert(
                    key,
                    current_values.drain(..).filter(|v| !v.is_empty()).collect(),
                );
            }
            current_key = Some(line[1..line.len() - 1].to_string());
        } else if current_key.is_some() {
            current_values.push(line.to_string());
        }
    }
    if let Some(key) = current_key.take() {
        map.insert(
            key,
            current_values
                .into_iter()
                .filter(|v| !v.is_empty())
                .collect(),
        );
    }
    map
}
