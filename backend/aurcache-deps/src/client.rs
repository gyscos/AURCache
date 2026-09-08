use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use backon::{FibonacciBuilder, Retryable};
use reqwest::Client;
use url::Url;

/// Ceiling for a generated RPC URL, under the server's 8 KiB request-line
/// limit with room for the scheme, host and path already in the base.
const MAX_RPC_URL_BYTES: usize = 7_600;

use crate::deps::deps_from_packages;
use crate::model::{
    Dependency, DependencyResolution, Error, Package, PackageResponse, PkgDeps, Resolutions,
};
use crate::repo::{default_official_mirrorlist_path, default_official_repo_cache_dir};
use crate::satisfy::{MatchKind, SatisfyIndex};

/// Client for the AUR RPC and Arch Linux official package search APIs.
///
/// Handles dependency resolution against AUR packages, official Arch
/// repositories (via a cached local copy of the repo DBs), and packages
/// already present in the local AURCache repository.
#[derive(Debug, Clone)]
pub struct AurClient {
    pub(crate) http: Client,
    pub(crate) rpc_url: String,
    pub(crate) repo_root: PathBuf,
    pub(crate) official_mirrorlist_path: PathBuf,
    pub(crate) official_repo_cache_dir: PathBuf,
}

impl Default for AurClient {
    fn default() -> Self {
        Self::new()
    }
}

/// Build a snapshot download URL for an AUR package from the RPC URL.
/// Strips `/rpc/v5` from the RPC URL to derive the base domain (if present).
pub(crate) fn snapshot_url(rpc_url: &str, pkgbase: &str) -> String {
    let base = rpc_url.trim_end_matches("/rpc/v5").trim_end_matches('/');
    format!("{base}/cgit/aur.git/snapshot/{pkgbase}.tar.gz")
}

/// What each source says about one dependency name.
///
/// The columns of the truth table in
/// [`AurClient::resolve_dependencies`], gathered separately so that combining
/// them is a pure function. [`Evidence::outcome`] is the only encoding of the
/// precedence rule anywhere in the crate: changing what beats what means
/// changing those three branches and nothing else.
#[derive(Debug, Default, Clone)]
struct Evidence {
    /// The pkgbase of a tracked package claiming the name.
    tracked: Option<String>,
    /// A repository lists the name at a version satisfying the constraint.
    published: bool,
    /// The pkgbase the AUR could build for it.
    aur: Option<String>,
}

impl Evidence {
    /// Combine the evidence. `None` means nothing anywhere provides the name.
    fn outcome(&self) -> Option<DependencyResolution> {
        if let Some(pkgbase) = &self.tracked {
            return Some(DependencyResolution::Local {
                pkgbase: pkgbase.clone(),
            });
        }
        if self.published {
            return Some(DependencyResolution::Available);
        }
        if let Some(pkgbase) = &self.aur {
            return Some(DependencyResolution::Aur {
                pkgbase: pkgbase.clone(),
            });
        }
        None
    }
}

/// The names no column has settled yet, in declared order.
///
/// Gathering a column costs something -- a disk read, a network round trip --
/// so each is asked only about what is still open. That this is safe is not a
/// second statement of the precedence rule: it *asks* [`Evidence::outcome`],
/// which returns on the first column that answers, so a column that gets
/// skipped provably could not have changed the result. Reordering the
/// gathering below can change how much work happens, never the answer.
fn undecided<'a>(evidence: &[(Dependency<'a>, Evidence)]) -> Vec<&'a str> {
    evidence
        .iter()
        .filter(|(_, found)| found.outcome().is_none())
        .map(|(dep, _)| dep.name)
        .collect()
}

impl AurClient {
    /// Construct a new client from the `AUR_RPC_URL` env var (or default).
    pub fn new() -> Self {
        let rpc_url = std::env::var("AUR_RPC_URL")
            .unwrap_or_else(|_| "https://aur.archlinux.org/rpc/v5".to_string());
        Self {
            http: Client::new(),
            repo_root: crate::repo::default_repo_root(),
            official_mirrorlist_path: default_official_mirrorlist_path(),
            official_repo_cache_dir: default_official_repo_cache_dir(),
            rpc_url,
        }
    }

    /// Construct a client with an explicit AUR RPC URL and default filesystem paths.
    pub fn with_urls(aur_url: impl Into<String>) -> Self {
        Self::with_urls_and_paths(
            aur_url,
            crate::repo::default_repo_root(),
            default_official_mirrorlist_path(),
            default_official_repo_cache_dir(),
        )
    }

    /// Construct a client with full control over the AUR RPC URL and filesystem paths.
    pub fn with_urls_and_paths(
        aur_url: impl Into<String>,
        repo_root: impl Into<PathBuf>,
        official_mirrorlist_path: impl Into<PathBuf>,
        official_repo_cache_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            http: Client::new(),
            rpc_url: aur_url.into(),
            repo_root: repo_root.into(),
            official_mirrorlist_path: official_mirrorlist_path.into(),
            official_repo_cache_dir: official_repo_cache_dir.into(),
        }
    }

    fn rpc_info_url(&self, args: &[&str]) -> Result<Url, Error> {
        let mut url = Url::parse(&format!("{}/info", self.rpc_url))?;
        for arg in args {
            url.query_pairs_mut().append_pair("arg[]", arg);
        }
        Ok(url)
    }

    /// Split an `/info` query across as many URLs as the length limit requires.
    ///
    /// The RPC takes one `arg[]` per package on a GET, and the server rejects a
    /// request line over 8 KiB with `414 Request-URI Too Large` — measured:
    /// 8185 bytes answers, 8313 does not. At roughly 25 bytes per package that
    /// caps a single request near 300 packages, which a repository of any size
    /// passes.
    ///
    /// Getting this wrong is not a partial failure: the version-check pass
    /// propagates the error, so one oversized request stops *every* package
    /// from being checked, on every pass, until the package count drops.
    ///
    /// Measured against the encoded URL rather than a package count, because
    /// names are percent-encoded — `aewm++` is four bytes longer than it looks.
    fn rpc_info_urls(&self, args: &[&str]) -> Result<Vec<Url>, Error> {
        let base = format!("{}/info", self.rpc_url);
        let mut urls = Vec::new();
        let mut current = Url::parse(&base)?;
        let mut in_current = 0usize;

        for arg in args {
            let mut candidate = current.clone();
            candidate.query_pairs_mut().append_pair("arg[]", arg);

            if in_current > 0 && candidate.as_str().len() > MAX_RPC_URL_BYTES {
                urls.push(current);
                current = Url::parse(&base)?;
                current.query_pairs_mut().append_pair("arg[]", arg);
                in_current = 1;
            } else {
                // A single argument that alone exceeds the budget still goes
                // out on its own: a doomed request beats silently dropping the
                // package from version checking.
                current = candidate;
                in_current += 1;
            }
        }

        if in_current > 0 {
            urls.push(current);
        }
        Ok(urls)
    }

    /// Resolve a list of package names to their pkgbase names via the AUR RPC.
    pub async fn resolve_bases(&self, names: &[&str]) -> Result<HashMap<String, String>, Error> {
        Ok(self
            .rpc_info_all(names)
            .await?
            .into_iter()
            .map(|pkg| (pkg.name, pkg.package_base))
            .collect())
    }

    /// Fetch the dependency lists for a pkgbase via the AUR RPC.
    pub async fn deps_of(&self, pkgbase: &str) -> Result<PkgDeps, Error> {
        let packages = self.rpc_request(&[pkgbase]).await?;
        Ok(deps_from_packages(&packages))
    }

    /// Fetch metadata for a single AUR package by name, returning `None` if not found.
    pub async fn info_of(&self, name: &str) -> Result<Option<Package>, Error> {
        let packages = self.rpc_fetch(self.rpc_info_url(&[name])?).await?;
        Ok(packages.into_iter().next())
    }

    /// Fetch metadata for multiple AUR packages in a single RPC call.
    pub async fn multi_info_of(&self, names: &[&str]) -> Result<Vec<Package>, Error> {
        let packages = self.rpc_info_all(names).await?;
        if packages.is_empty() && !names.is_empty() {
            return Err(Error::Rpc("package not found via RPC".into()));
        }
        Ok(packages)
    }

    /// Fetch an `/info` query for every name, chunked across URLs as required
    /// by `rpc_info_urls` and concatenated. Chunked because this is handed a
    /// whole dependency list, which for a large package can outgrow the
    /// server's URL limit. An individual chunk returning nothing is fine —
    /// those packages are simply not in the AUR any more. Only a request that
    /// fails outright aborts.
    async fn rpc_info_all(&self, names: &[&str]) -> Result<Vec<Package>, Error> {
        let mut packages = Vec::new();
        for url in self.rpc_info_urls(names)? {
            packages.extend(self.rpc_fetch(url).await?);
        }
        Ok(packages)
    }

    /// Search AUR packages by name or description.
    pub async fn search_by_name(&self, query: &str) -> Result<Vec<Package>, Error> {
        let url = self.rpc_search_url(query, "name-desc")?;
        self.rpc_fetch(url).await
    }

    /// Resolve dependencies to what should happen about each of them.
    ///
    /// The outcome is a pure function of three independent facts about a name:
    ///
    /// - **tracked** -- `tracked` holds a package claiming the name: AURCache
    ///   has a row for it, or an in-flight add is about to insert one. A
    ///   statement about ownership, not about anything having been built, so a
    ///   package whose last build failed still counts. Version constraints are
    ///   deliberately not consulted (see below).
    /// - **published** -- a repository database lists a package providing the
    ///   name at a version satisfying the constraint: AURCache's own
    ///   repository for the platforms being built, or the cached
    ///   `core`/`extra`/`multilib`. Note this is what the *database* says, not
    ///   that the file is still on disk, and that the official databases are
    ///   fetched for `x86_64` only.
    /// - **in the AUR** -- an AUR package carries the name, or declares it in
    ///   `provides`.
    ///
    /// | tracked | published | in the AUR | outcome |
    /// |---|---|---|---|
    /// | yes | — | — | [`DependencyResolution::Local`] |
    /// | no | yes | — | [`DependencyResolution::Available`] |
    /// | no | no | yes | [`DependencyResolution::Aur`] |
    /// | no | no | no | [`Resolutions::unresolved`] |
    ///
    /// Read down the columns and the rule is one sentence: **ownership decides
    /// first, availability second.** A package AURCache tracks is its
    /// responsibility whether or not a binary of it happens to exist; a
    /// package it does not track is someone else's, so an existing binary is
    /// used and only a missing one is built.
    ///
    /// The implementation evaluates that table left to right, stopping as soon
    /// as a column settles it. That is an evaluation order, not the
    /// definition: no stage can see what an earlier one concluded, and the
    /// answer for a name does not depend on the other names in the batch.
    ///
    /// # Why the columns are in this order
    ///
    /// Each boundary is load-bearing, so none of them may be swapped for
    /// convenience:
    ///
    /// - **tracked before published.** AURCache's own repository is part of
    ///   "published", so without this a package AURCache tracks *and* has
    ///   already built once resolves as merely available, and the dependency
    ///   edge is never recorded -- nothing then rebuilds its dependents when
    ///   it changes. This is the bug the column order exists to prevent. With
    ///   the order right, "published" is reachable for one of our own packages
    ///   only when no row exists at all -- a deleted package whose artifact
    ///   remains -- and there `Available` is correct, since there is nothing
    ///   to link to.
    /// - **published before the AUR.** A name a repository holds *and* the AUR
    ///   carries must resolve to the published package; resolving it to the
    ///   AUR would have AURCache build something that already exists.
    /// - **exact AUR name before `provides`.** A package carrying the name
    ///   outright is a better answer than one that merely declares it.
    ///
    /// Cost happens to agree with all three, which is why short-circuiting is
    /// free rather than a compromise: the first column is in memory, the
    /// second on disk, and only the third and fourth touch the network -- and
    /// the fourth has no bulk form, so it costs one request per name left.
    ///
    /// # Why `tracked` is a parameter
    ///
    /// Not merely because this crate has no database. The set of packages that
    /// can satisfy a dependency is not the set any query returns: during an
    /// add it also includes packages that add has *planned* but not yet
    /// inserted, since the whole graph is resolved before anything is written.
    /// Only the caller knows those. A version of this that fetched its own
    /// rows would still have to be handed the in-flight half, so the parameter
    /// buys correctness at no cost in coupling.
    ///
    /// It also removes an ordering hazard that used to be real: the database
    /// was once a *separate function* callers had to remember to run first,
    /// and calling this one alone reported every package AURCache had already
    /// built as merely available.
    ///
    /// # Which columns check the version constraint
    ///
    /// Only "published". A repository hit *ends* resolution -- the binary
    /// is used as it is, and nothing downstream will ever look at its version
    /// again -- so a repository holding `foo-1.0` must not answer for
    /// `foo>=2.0`. A tracked hit defers instead: the edge it produces records
    /// the constraint, and the build queue re-checks it against each new
    /// build, which is both later and better informed than anything decidable
    /// here. The AUR columns have nothing to check, since the version will be
    /// whatever the build produces.
    pub async fn resolve_dependencies(
        &self,
        deps: &[Dependency<'_>],
        tracked: &SatisfyIndex,
        platforms: &[String],
    ) -> Result<Resolutions, Error> {
        // A pkgbase can name the same dependency in `depends` and
        // `makedepends`, and its split packages multiply that again.
        let mut seen = HashSet::new();
        let mut evidence: Vec<(Dependency<'_>, Evidence)> = Vec::new();
        for dep in deps {
            if seen.insert(dep.name) {
                evidence.push((*dep, Evidence::default()));
            }
        }

        // Column 1: what the caller already tracks. In memory; costs nothing.
        for (dep, found) in &mut evidence {
            found.tracked = tracked
                .best_match(dep.name, |_| true)
                .map(|matched| matched.pkgbase.clone());
        }

        // Column 2: what the repositories publish. From disk.
        let open = undecided(&evidence);
        if !open.is_empty() {
            let wanted: HashSet<&str> = open.iter().copied().collect();
            let mut repositories = self.local_repo_index(&wanted, platforms)?;
            repositories.extend(self.official_repo_index(&wanted).await?);
            for (dep, found) in &mut evidence {
                if found.outcome().is_some() {
                    continue;
                }
                found.published = repositories
                    .best_match(dep.name, |candidate| candidate.satisfies(dep.constraint))
                    .is_some();
            }
        }

        // Column 3: the AUR, by exact package name. One request for every
        // remaining name, not one per name: `resolve_bases` chunks by URL
        // length, so this is a single call for any realistic dependency list.
        let open = undecided(&evidence);
        if !open.is_empty() {
            let exact_aur_bases = self.resolve_bases(&open).await?;
            for (dep, found) in &mut evidence {
                if found.outcome().is_some() {
                    continue;
                }
                found.aur = exact_aur_bases.get(dep.name).cloned();
            }
        }

        // Column 4: the AUR, by `provides`. No bulk form, so one request each
        // -- which is why it is asked last, of the fewest names.
        for (dep, found) in &mut evidence {
            if found.outcome().is_some() {
                continue;
            }
            found.aur = self.aur_provider_pkgbase(dep.name).await?;
        }

        let mut resolutions = Resolutions::default();
        for (dep, found) in evidence {
            match found.outcome() {
                Some(resolution) => {
                    resolutions.found.insert(dep.name.to_string(), resolution);
                }
                None => resolutions.unresolved.push(dep.name.to_string()),
            }
        }
        Ok(resolutions)
    }

    fn rpc_search_url(&self, query: &str, by: &str) -> Result<Url, Error> {
        let mut url = Url::parse(&format!("{}/search", self.rpc_url))?;
        url.path_segments_mut()
            .map_err(|_| Error::Rpc("Invalid RPC search URL".to_string()))?
            .push(query);
        url.query_pairs_mut().append_pair("by", by);
        Ok(url)
    }

    async fn rpc_fetch(&self, url: Url) -> Result<Vec<Package>, Error> {
        let resp = self.retry_get(url).await?;
        let text = resp.text().await?;
        let response: PackageResponse = serde_json::from_str(&text)?;
        if response.response_type == "error" {
            return Err(Error::Rpc(
                response
                    .error
                    .unwrap_or_else(|| "AUR RPC returned error".to_string()),
            ));
        }
        Ok(response.results)
    }

    async fn rpc_request(&self, args: &[&str]) -> Result<Vec<Package>, Error> {
        let packages = self.rpc_fetch(self.rpc_info_url(args)?).await?;
        if packages.is_empty() {
            return Err(Error::Rpc("package not found via RPC".into()));
        }
        Ok(packages)
    }

    /// Perform an HTTP GET with the shared Fibonacci retry policy, returning the
    /// response only if it has a success status.
    pub(crate) async fn retry_get<U: reqwest::IntoUrl + Clone>(
        &self,
        url: U,
    ) -> Result<reqwest::Response, Error> {
        let http = self.http.clone();
        let fetch = move || {
            let http = http.clone();
            let url = url.clone();
            async move { http.get(url).send().await }
        };
        fetch
            .retry(
                FibonacciBuilder::default()
                    .with_min_delay(Duration::from_millis(500))
                    .with_max_times(3),
            )
            .await
            .map_err(Error::Http)?
            .error_for_status()
            .map_err(Error::Http)
    }

    /// Download the raw snapshot tarball for an AUR pkgbase.
    pub async fn download_snapshot_bytes(&self, pkgbase: &str) -> Result<Vec<u8>, Error> {
        let url = snapshot_url(&self.rpc_url, pkgbase);
        let resp = self.retry_get(url).await?;
        let bytes = resp.bytes().await?.to_vec();
        Ok(bytes)
    }

    /// The AUR package base that declares `dep_name` in its `provides`.
    ///
    /// Ranked through [`SatisfyIndex`] like every other source, so an AUR
    /// package carrying the name outright beats one that merely provides it.
    /// This used to take whichever pkgbase sorted first alphabetically, which
    /// meant a virtual dependency was built from a package chosen by nothing
    /// more than its initial.
    async fn aur_provider_pkgbase(&self, dep_name: &str) -> Result<Option<String>, Error> {
        let packages = self
            .rpc_fetch(self.rpc_search_url(dep_name, "provides")?)
            .await?;

        // Indexed by hand rather than through `insert_package`, because the
        // RPC's *search* response carries only identity fields -- no
        // `provides`, unlike `info`. The query is the evidence instead: every
        // result provides `dep_name` by construction, since the server
        // filtered on exactly that. Re-deriving the claim from the response
        // body would find nothing and quietly resolve every virtual
        // dependency to nothing at all.
        let mut index = SatisfyIndex::new();
        for package in &packages {
            let kind = if package.name == dep_name {
                MatchKind::Name
            } else {
                MatchKind::Provides
            };
            index.insert(dep_name, &package.package_base, kind, None);
        }

        Ok(index
            .best_match(dep_name, |_| true)
            .map(|found| found.pkgbase.clone()))
    }
}

#[cfg(test)]
mod evidence_tests {
    use super::Evidence;
    use crate::model::DependencyResolution;

    fn evidence(tracked: bool, published: bool, aur: bool) -> Evidence {
        Evidence {
            tracked: tracked.then(|| "tracked-base".to_string()),
            published,
            aur: aur.then(|| "aur-base".to_string()),
        }
    }

    /// Every combination of the three columns, so the precedence rule is
    /// pinned by a test rather than by the order of statements that happen to
    /// gather it. No repository, no database, no network.
    #[test]
    fn the_truth_table_holds_for_every_combination() {
        let local = Some(DependencyResolution::Local {
            pkgbase: "tracked-base".to_string(),
        });
        let available = Some(DependencyResolution::Available);
        let aur = Some(DependencyResolution::Aur {
            pkgbase: "aur-base".to_string(),
        });

        // (tracked, published, aur) -> outcome
        let cases = [
            ((true, true, true), local.clone()),
            ((true, true, false), local.clone()),
            ((true, false, true), local.clone()),
            ((true, false, false), local),
            ((false, true, true), available.clone()),
            ((false, true, false), available),
            ((false, false, true), aur),
            ((false, false, false), None),
        ];

        for ((tracked, published, in_aur), expected) in cases {
            assert_eq!(
                evidence(tracked, published, in_aur).outcome(),
                expected,
                "tracked={tracked} published={published} aur={in_aur}"
            );
        }
    }

    /// The property the short-circuit in `undecided` relies on: once a column
    /// answers, no later column can change the outcome. If this ever stops
    /// holding, skipping the remaining columns stops being safe.
    #[test]
    fn later_columns_cannot_override_an_earlier_answer() {
        for published in [false, true] {
            for in_aur in [false, true] {
                assert_eq!(
                    evidence(true, published, in_aur).outcome(),
                    evidence(true, false, false).outcome(),
                    "a tracked package must decide regardless of later columns"
                );
            }
        }
        for in_aur in [false, true] {
            assert_eq!(
                evidence(false, true, in_aur).outcome(),
                evidence(false, true, false).outcome(),
                "a published package must decide regardless of the AUR"
            );
        }
    }
}

#[cfg(test)]
mod url_chunking_tests {
    use super::{AurClient, MAX_RPC_URL_BYTES};

    fn client() -> AurClient {
        AurClient::with_urls("https://aur.archlinux.org/rpc/v5")
    }

    /// A handful of packages is one request, as before.
    #[test]
    fn a_small_query_is_a_single_request() {
        let names = ["hello", "yay", "paru"];
        let urls = client().rpc_info_urls(&names).expect("urls");
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].query_pairs().count(), 3);
    }

    /// The case that broke: a repository with enough packages to outgrow the
    /// server's 8 KiB request line. Measured against the real service — 8185
    /// bytes answers, 8313 returns 414.
    #[test]
    fn a_large_query_is_split_and_every_part_fits() {
        let names: Vec<String> = (0..1000)
            .map(|i| format!("some-package-name-{i}"))
            .collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();

        let urls = client().rpc_info_urls(&refs).expect("urls");
        assert!(urls.len() > 1, "1000 packages should not be one request");

        for url in &urls {
            assert!(
                url.as_str().len() <= MAX_RPC_URL_BYTES,
                "chunk of {} bytes exceeds the budget",
                url.as_str().len()
            );
        }

        // Every package is asked about exactly once — chunking must not drop
        // or duplicate any, which would silently stop them being checked.
        let asked: Vec<String> = urls
            .iter()
            .flat_map(|u| {
                u.query_pairs()
                    .map(|(_, v)| v.to_string())
                    .collect::<Vec<_>>()
            })
            .collect();
        assert_eq!(asked, names);
    }

    /// Names are percent-encoded, so a count-based split would misjudge the
    /// length. 187 AUR packages contain `+`, which triples in the URL.
    #[test]
    fn encoded_names_are_measured_at_their_encoded_length() {
        let names: Vec<String> = (0..1000).map(|i| format!("aewm++{i}+plus+name")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();

        for url in client().rpc_info_urls(&refs).expect("urls") {
            assert!(
                url.as_str().len() <= MAX_RPC_URL_BYTES,
                "encoded chunk of {} bytes exceeds the budget",
                url.as_str().len()
            );
        }
    }

    /// No packages means no requests, rather than one pointless empty query.
    #[test]
    fn an_empty_query_makes_no_requests() {
        assert!(client().rpc_info_urls(&[]).expect("urls").is_empty());
    }
}
