use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use alpm_compress::tarball::TarballReader;
use url::Url;

use crate::client::AurClient;
use crate::model::Error;
use crate::satisfy::SatisfyIndex;

const OFFICIAL_REPO_NAMES: &[&str] = &["core", "extra", "multilib"];
const OFFICIAL_REPO_CACHE_TTL_SECS: u64 = 60 * 60;

/// Mirrorlist used to locate official repo databases.
///
/// Official repo DBs are fetched for `x86_64` only today (see
/// [`official_repo_db_url`], which substitutes `$arch` with `x86_64`), so this
/// asks for that architecture explicitly rather than guessing the host's.
/// `OFFICIAL_MIRRORLIST_PATH` still overrides it outright.
///
/// This resolves through [`crate::paths`] so it cannot drift from where the
/// mirrorlist is actually written — it previously defaulted to
/// `./config/pacman_x86_64/mirrorlist`, a path nothing ever wrote, which made
/// dependency resolution fail for every package that had dependencies.
pub(crate) fn default_official_mirrorlist_path() -> PathBuf {
    if let Ok(path) = std::env::var("OFFICIAL_MIRRORLIST_PATH") {
        return PathBuf::from(path);
    }
    crate::paths::mirrorlist_path("x86_64")
}

pub(crate) fn default_official_repo_cache_dir() -> PathBuf {
    crate::paths::official_repo_cache_dir()
}

impl AurClient {
    pub(crate) async fn official_repo_index(
        &self,
        wanted: &HashSet<&str>,
    ) -> Result<SatisfyIndex, Error> {
        self.refresh_official_repo_cache_if_needed().await?;
        let archives = OFFICIAL_REPO_NAMES.iter().map(|repo_name| {
            self.official_repo_cache_dir
                .join(cache_file_name(repo_name))
        });
        index_archives(archives, wanted)
    }

    /// Bring the cached official databases up to date, downloading only the
    /// ones that have aged out.
    ///
    /// The mirrorlist is read only if something actually needs downloading. It
    /// is written by the mirror-ranking scheduler, so on a fresh instance it
    /// may not exist yet -- and a resolution that could have been answered
    /// entirely from a warm cache should not fail because of that.
    async fn refresh_official_repo_cache_if_needed(&self) -> Result<(), Error> {
        fs::create_dir_all(&self.official_repo_cache_dir)?;

        let mut stale = Vec::new();
        for repo_name in OFFICIAL_REPO_NAMES {
            let archive_path = self
                .official_repo_cache_dir
                .join(cache_file_name(repo_name));
            if cache_is_stale(&archive_path)? {
                stale.push((*repo_name, archive_path));
            }
        }
        if stale.is_empty() {
            return Ok(());
        }

        let mirrors = official_mirror_servers(&self.official_mirrorlist_path)?;
        if mirrors.is_empty() {
            return Err(Error::Rpc(
                "No official repo mirrors configured".to_string(),
            ));
        }

        for (repo_name, archive_path) in stale {
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

        // Deliberately fatal. Answering "not found in the official
        // repositories" when the truth is "could not ask" sends every ordinary
        // `core`/`extra` name off to be resolved against the AUR, where a
        // `provides` search can turn `glibc` into something to build.
        Err(last_error.unwrap_or_else(|| {
            Error::Rpc(format!(
                "could not refresh the {repo_name} database from any mirror; \
                 dependency resolution cannot tell what the official \
                 repositories hold"
            ))
        }))
    }

    /// Downloads `url` to `archive_path`, disabling reqwest's transparent gzip
    /// decompression (via `Accept-Encoding: identity`) so the bytes written to
    /// disk match the wire format of an Arch repo DB (gzip, `.db` served as
    /// `.db.tar.gz`). Without this, reqwest's `gzip` feature auto-decompresses
    /// the response body while the file is still named/expected as `.tar.gz`,
    /// causing `TarballReader` to try to gunzip already-plain tar bytes and
    /// silently fail every lookup against the cache.
    async fn download_to_path(&self, url: &Url, archive_path: &Path) -> Result<(), Error> {
        let bytes = self
            .http
            .get(url.clone())
            .header(reqwest::header::ACCEPT_ENCODING, "identity")
            .send()
            .await
            .map_err(Error::Http)?
            .error_for_status()
            .map_err(Error::Http)?
            .bytes()
            .await
            .map_err(Error::Http)?;
        fs::write(archive_path, bytes)?;
        Ok(())
    }
}

fn cache_is_stale(path: &Path) -> Result<bool, Error> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(err) => return Err(err.into()),
    };
    let modified = metadata.modified()?;
    let age = SystemTime::now()
        .duration_since(modified)
        .map_err(|e| Error::Rpc(e.to_string()))?;
    Ok(age.as_secs() > OFFICIAL_REPO_CACHE_TTL_SECS)
}

/// Mirrors to fetch the official repository *databases* from.
///
/// `OFFICIAL_MIRRORLIST_SERVERS` (a `;`-separated server list, same shape as
/// `MIRRORLIST_SERVERS_X86_64`) takes precedence over the mirrorlist file.
///
/// Separate from what workers are given because the two want different things:
/// this fetches three small `.db` files hourly and wants a mirror that is
/// close and reliable, while a worker bulk-downloads packages and may sit on
/// entirely different hardware. Sharing one setting is a fine default and a
/// poor requirement.
fn official_mirror_servers(path: &Path) -> Result<Vec<String>, Error> {
    if let Ok(servers) = std::env::var("OFFICIAL_MIRRORLIST_SERVERS") {
        let configured: Vec<String> = servers
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string)
            .collect();
        if !configured.is_empty() {
            return Ok(configured);
        }
    }
    mirror_servers(path)
}

fn mirror_servers(path: &Path) -> Result<Vec<String>, Error> {
    let content = fs::read_to_string(path)?;
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
    Ok(Url::parse(&format!("{base}{separator}{repo_name}.db"))?)
}

fn cache_file_name(repo_name: &str) -> String {
    format!("{repo_name}.db.tar.gz")
}

/// Index every entry in `archive_paths` that answers to one of `wanted`.
///
/// One pass per archive, keeping only the names asked about, so the cost
/// scales with the dependency list rather than with the repository. Asking
/// each archive about each name in turn -- which is what this replaced --
/// re-decompressed `extra.db` (~15k entries, ~9 MB) once per dependency, so a
/// package with a hundred-odd dependencies paid for hundreds of full passes.
///
/// Missing archive paths are skipped: a platform with nothing built yet has no
/// database, which is an empty index rather than an error.
fn index_archives(
    archive_paths: impl IntoIterator<Item = PathBuf>,
    wanted: &HashSet<&str>,
) -> Result<SatisfyIndex, Error> {
    let mut index = SatisfyIndex::new();
    if wanted.is_empty() {
        return Ok(index);
    }

    for archive_path in archive_paths {
        if !archive_path.exists() {
            continue;
        }
        index_archive(&archive_path, wanted, &mut index)?;
    }
    Ok(index)
}

/// Wrap a repo-database decoding failure, keeping the original as the source.
fn repo_db_error(e: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::RepoDb(Box::new(e))
}

fn index_archive(
    archive_path: &Path,
    wanted: &HashSet<&str>,
    index: &mut SatisfyIndex,
) -> Result<(), Error> {
    let mut reader = TarballReader::try_from(archive_path).map_err(repo_db_error)?;
    for entry in reader.entries().map_err(repo_db_error)? {
        let mut entry = entry.map_err(repo_db_error)?;
        if entry.path().file_name().and_then(|name| name.to_str()) != Some("desc") {
            continue;
        }

        let content =
            String::from_utf8(entry.content().map_err(repo_db_error)?).map_err(repo_db_error)?;
        index_desc(&content, wanted, index);
    }
    Ok(())
}

/// Read one `desc` entry into `index`.
///
/// Just the pacman-database half: pulling the fields out of the `%SECTION%`
/// format. What counts as a match, and how one ranks, is
/// [`SatisfyIndex::insert_package`]'s. A database old enough to omit `%BASE%`
/// falls back to `%NAME%`.
fn index_desc(content: &str, wanted: &HashSet<&str>, index: &mut SatisfyIndex) {
    let sections = parse_desc_sections(content);
    let first = |key: &str| {
        sections
            .get(key)
            .and_then(|values| values.first())
            .map(String::as_str)
    };

    let Some(name) = first("NAME") else {
        return;
    };
    index.insert_package(
        name,
        first("BASE").unwrap_or(name),
        first("VERSION"),
        sections.get("PROVIDES").into_iter().flatten(),
        wanted,
    );
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
fn parse_desc_sections(content: &str) -> HashMap<String, Vec<String>> {
    fn flush(
        map: &mut HashMap<String, Vec<String>>,
        key: Option<String>,
        values: &mut Vec<String>,
    ) {
        if let Some(key) = key {
            map.insert(key, values.drain(..).filter(|v| !v.is_empty()).collect());
        }
    }

    let mut map = HashMap::new();
    let mut current_key: Option<String> = None;
    let mut current_values: Vec<String> = Vec::new();

    for line in content.lines() {
        if line.starts_with('%') && line.ends_with('%') {
            flush(&mut map, current_key.take(), &mut current_values);
            current_key = Some(line[1..line.len() - 1].to_string());
        } else if current_key.is_some() {
            current_values.push(line.to_string());
        }
    }
    flush(&mut map, current_key.take(), &mut current_values);
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::fmt::Write as _;
    use tar::{Builder, Header};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Builds a gzip-compressed tar archive (matching the real wire format of
    /// Arch repo DBs, e.g. `core.db.tar.gz`) containing a single package
    /// `desc` entry for `pkg_name`, optionally providing `provides_name`.
    fn build_repo_db_tar_gz(pkg_name: &str, provides_name: Option<&str>) -> Vec<u8> {
        let mut desc = format!("%NAME%\n{pkg_name}\n\n%VERSION%\n1.0-1\n\n");
        if let Some(provides) = provides_name {
            let _ = write!(desc, "%PROVIDES%\n{provides}\n\n");
        }

        let gz = GzEncoder::new(Vec::new(), Compression::default());
        let mut tar_builder = Builder::new(gz);
        let entry_path = format!("{pkg_name}-1.0-1/desc");
        let mut header = Header::new_gnu();
        header.set_size(desc.len() as u64);
        header.set_cksum();
        tar_builder
            .append_data(&mut header, &entry_path, desc.as_bytes())
            .unwrap();
        let gz = tar_builder.into_inner().unwrap();
        gz.finish().unwrap()
    }

    /// Regression test for the reqwest gzip auto-decompression bug: the
    /// official Arch mirrors serve `core.db`/`extra.db`/`multilib.db` as
    /// statically pre-gzipped files. If reqwest's `gzip` feature is enabled
    /// (as it is workspace-wide via other crates) and `download_to_path`
    /// doesn't explicitly disable transparent decompression, the
    /// already-decompressed (plain tar) bytes get written to disk under a
    /// `.tar.gz` filename, and `TarballReader` (which selects its
    /// decompression algorithm by file extension) fails to read them back,
    /// making every official-repo dependency lookup silently report "not
    /// found".
    ///
    /// Rather than relying on the `gzip` cargo feature actually being enabled
    /// for this crate in isolation (which depends on feature unification with
    /// other workspace crates, and so isn't reliably exercised by `cargo test
    /// -p aurcache-deps`), this test asserts directly on the outgoing
    /// request: `download_to_path` must send `Accept-Encoding: identity` to
    /// explicitly opt out of any transparent decompression, regardless of
    /// which reqwest features happen to be compiled in. The mock only
    /// responds to requests carrying that header, so this test fails
    /// (download error propagates) if the header is ever removed.
    #[tokio::test]
    async fn download_to_path_disables_transparent_decompression() {
        let server = MockServer::start().await;
        let body = build_repo_db_tar_gz("git", None);

        for repo_name in OFFICIAL_REPO_NAMES {
            Mock::given(method("GET"))
                .and(path(format!("/{repo_name}/os/x86_64/{repo_name}.db")))
                .and(header("Accept-Encoding", "identity"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_raw(body.clone(), "application/octet-stream")
                        .append_header("Content-Encoding", "x-gzip"),
                )
                .mount(&server)
                .await;
        }

        let mirrorlist_dir = tempfile::tempdir().unwrap();
        let mirrorlist_path = mirrorlist_dir.path().join("mirrorlist");
        fs::write(
            &mirrorlist_path,
            format!("Server = {}/$repo/os/$arch\n", server.uri()),
        )
        .unwrap();

        let cache_dir = tempfile::tempdir().unwrap();
        let client = AurClient::with_urls_and_paths(
            "http://unused.invalid/rpc/v5",
            mirrorlist_path,
            cache_dir.path().to_path_buf(),
        );

        let index = client
            .official_repo_index(&HashSet::from(["git", "not-a-real-package"]))
            .await
            .expect(
                "download_to_path must send Accept-Encoding: identity; \
                 otherwise the mock rejects the request (404) and the \
                 lookup fails",
            );
        assert!(
            index.best_match("git", |_| true).is_some(),
            "expected 'git' to be found in the cached official repo DBs"
        );

        // Sanity check: the cached archive on disk is genuinely gzip-compressed
        // (matching its `.tar.gz` extension), not silently auto-decompressed.
        let cached_path = cache_dir.path().join(cache_file_name("core"));
        let on_disk = fs::read(&cached_path).unwrap();
        assert_eq!(
            &on_disk[0..2],
            &[0x1f, 0x8b],
            "cached archive should be gzip-compressed on disk (gzip magic bytes)"
        );

        assert!(index.best_match("not-a-real-package", |_| true).is_none());
    }

    /// A warm cache answers on its own. The mirrorlist is only consulted to
    /// *refresh* stale databases, so a fresh instance whose mirror ranking has
    /// not run yet -- or one whose mirrors are briefly unreachable -- still
    /// resolves against what it already has.
    #[tokio::test]
    async fn a_fresh_cache_needs_no_mirrorlist() {
        let cache_dir = tempfile::tempdir().unwrap();
        for repo_name in OFFICIAL_REPO_NAMES {
            fs::write(
                cache_dir.path().join(cache_file_name(repo_name)),
                build_repo_db_tar_gz("git", None),
            )
            .unwrap();
        }

        let client = AurClient::with_urls_and_paths(
            "http://unused.invalid/rpc/v5",
            PathBuf::from("/nonexistent/mirrorlist"),
            cache_dir.path().to_path_buf(),
        );

        let index = client
            .official_repo_index(&HashSet::from(["git"]))
            .await
            .expect("a warm cache must not require a mirrorlist");
        assert!(index.best_match("git", |_| true).is_some());
    }
}
