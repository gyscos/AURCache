//! Detects VCS (`git+...`) sources in a parsed `.SRCINFO` and resolves the
//! commit each currently points to on the remote, without cloning.
//!
//! This lets version checks flag VCS packages (`-git`, `-svn`, `-hg`, `-bzr`
//! style `pkgname`s, though only `git+` sources are currently supported) as
//! out-of-date when upstream has moved, even though the AUR-published
//! `pkgver` for such packages is typically stale (it only reflects when the
//! PKGBUILD itself was last touched, not the live upstream state).
use std::collections::{BTreeMap, HashMap, HashSet};

use alpm_srcinfo::SourceInfoV1;
use alpm_types::Source;
use alpm_types::url::{GitFragment, VcsInfo};
use sea_orm::{ActiveValue::Set, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};

use aurcache_db::helpers::builds::{latest_successful_build_vcs_sources, record_build_vcs_sources};
use aurcache_db::helpers::time::now_secs;
use aurcache_db::package_vcs_sources::{self, Entity as PackageVcsSources};

use crate::git::checkout::ls_remote;

/// A single VCS source entry extracted from a package's `.SRCINFO`.
///
/// `source_url` is the raw source string as written in `.SRCINFO` (e.g.
/// `mypkg::git+https://example.com/repo.git#branch=develop`), which serves
/// as a stable, natural per-source key: it already disambiguates multiple
/// source entries within the same package base (that's what the optional
/// `name::` prefix and `#fragment` are for), and two different sources
/// always have different `source_url`s.
#[derive(Clone)]
pub struct VcsSource {
    pub source_url: String,
    repo_url: String,
    git_ref: String,
}

impl VcsSource {
    /// Resolve the commit this source currently points to on the remote
    /// (equivalent to `git ls-remote`); does not clone or fetch objects.
    ///
    /// Blocking: performs network I/O and must be run via
    /// `tokio::task::spawn_blocking` from async code.
    pub fn resolve_commit(&self) -> anyhow::Result<String> {
        ls_remote(&self.repo_url, &self.git_ref)
    }
}

/// Extract all *trackable* `git+` VCS sources from `sourceinfo`.
///
/// Only the package base's generic `source` array is considered;
/// architecture-specific overrides (`source_x86_64=(...)` etc.) are not
/// tracked. Sources pinned to an exact `#commit=<sha>` are skipped, since
/// they're immutable by construction and can never go "out of date".
/// Non-git VCS types (`svn+`, `hg+`, `bzr+`, `fossil+`) are not yet
/// supported and are skipped.
pub fn extract_git_vcs_sources(sourceinfo: &SourceInfoV1) -> Vec<VcsSource> {
    sourceinfo
        .base
        .sources
        .iter()
        .filter_map(git_vcs_source_from)
        .collect()
}

fn git_vcs_source_from(source: &Source) -> Option<VcsSource> {
    let Source::SourceUrl { source_url, .. } = source else {
        return None;
    };

    let VcsInfo::Git { fragment, .. } = source_url.vcs_info.as_ref()? else {
        return None;
    };

    let git_ref = match fragment {
        // Pinned to an exact commit: never changes, nothing to track.
        Some(GitFragment::Commit(_)) => return None,
        Some(GitFragment::Branch(name) | GitFragment::Tag(name)) => name.clone(),
        None => "HEAD".to_string(),
    };

    Some(VcsSource {
        source_url: source.to_string(),
        repo_url: source_url.url.to_string(),
        git_ref,
    })
}

/// Resolve every trackable git-VCS source in `sourceinfo`, upsert their
/// current commit into `package_vcs_sources`, remove rows for sources no
/// longer present (e.g. after a PKGBUILD edit changed/removed a VCS source),
/// and report whether any tracked source's commit changed since last time.
///
/// A newly-seen source (no prior row) also counts as "changed", so the
/// first check after a VCS source is added/discovered is flagged out of
/// date, matching the existing `latest_version.is_none() => outdated`
/// convention used for pkgver checks.
pub async fn sync_vcs_sources(
    db: &DatabaseConnection,
    package_id: i32,
    sourceinfo: &SourceInfoV1,
) -> anyhow::Result<bool> {
    let vcs_sources = extract_git_vcs_sources(sourceinfo);

    let existing: HashMap<String, String> = PackageVcsSources::find()
        .filter(package_vcs_sources::Column::PackageId.eq(package_id))
        .all(db)
        .await?
        .into_iter()
        .map(|row| (row.source_url, row.last_commit))
        .collect();

    // What the last successful build was actually made from. This, and not the
    // watermark above, is what "out of date" has to mean: the watermark only
    // moves when the *check* looks, so any build triggered another way -- a
    // manual rebuild, a retry, an unforced update -- left it behind, and the
    // next check re-detected a move it had already built.
    let built = latest_successful_build_vcs_sources(db, package_id).await?;

    let mut changed = false;
    let mut seen_urls: HashSet<String> = HashSet::new();
    let mut upserts = Vec::new();

    for source in vcs_sources {
        // `resolve_commit` performs blocking network I/O via git2.
        let source_url = source.source_url.clone();
        let commit = tokio::task::spawn_blocking(move || source.resolve_commit()).await??;

        seen_urls.insert(source_url.clone());

        if source_moved(&built, &existing, &source_url, &commit) {
            changed = true;
        }

        upserts.push(package_vcs_sources::ActiveModel {
            package_id: Set(package_id),
            source_url: Set(source_url),
            last_commit: Set(commit),
            updated_at: Set(now_secs()),
            ..Default::default()
        });
    }

    // Remove rows for sources no longer present (e.g. patch/PKGBUILD change
    // dropped or renamed a VCS source).
    let stale: Vec<String> = existing
        .keys()
        .filter(|url| !seen_urls.contains(*url))
        .cloned()
        .collect();
    if !stale.is_empty() {
        PackageVcsSources::delete_many()
            .filter(package_vcs_sources::Column::PackageId.eq(package_id))
            .filter(package_vcs_sources::Column::SourceUrl.is_in(stale))
            .exec(db)
            .await?;
    }

    if !upserts.is_empty() {
        PackageVcsSources::insert_many(upserts)
            .on_conflict(
                sea_orm::sea_query::OnConflict::columns([
                    package_vcs_sources::Column::PackageId,
                    package_vcs_sources::Column::SourceUrl,
                ])
                .update_columns([
                    package_vcs_sources::Column::LastCommit,
                    package_vcs_sources::Column::UpdatedAt,
                ])
                .to_owned(),
            )
            .exec(db)
            .await?;
    }

    Ok(changed)
}

/// Resolve every trackable VCS source of `sourceinfo` to the commit it points
/// at right now.
///
/// One `ls-remote` per source, no clone. Errors are per source rather than
/// fatal: a remote that cannot be reached leaves that source unrecorded, which
/// reads as unknown downstream and costs at most one redundant rebuild -- where
/// failing the caller would cost the build itself.
pub async fn resolve_vcs_commits(sourceinfo: &SourceInfoV1) -> BTreeMap<String, String> {
    let mut resolved = BTreeMap::new();
    for source in extract_git_vcs_sources(sourceinfo) {
        let source_url = source.source_url.clone();
        match tokio::task::spawn_blocking(move || source.resolve_commit()).await {
            Ok(Ok(commit)) => {
                resolved.insert(source_url, commit);
            }
            Ok(Err(e)) => tracing::warn!("could not resolve {source_url}: {e}"),
            Err(e) => tracing::warn!("resolving {source_url} panicked: {e}"),
        }
    }
    resolved
}

/// Record what `build_id`'s VCS sources were at as it was queued.
///
/// Queue time rather than build time: the worker may check out something newer
/// if upstream moves while the build waits, and recording the earlier commit
/// errs toward one redundant rebuild rather than a missed one. A worker that
/// reports what it actually used overwrites this later.
pub async fn record_queued_vcs_sources(
    db: &DatabaseConnection,
    build_id: i32,
    commits: &BTreeMap<String, String>,
) -> anyhow::Result<()> {
    record_build_vcs_sources(db, build_id, commits).await?;
    Ok(())
}

/// Whether any tracked VCS source has moved since the last successful build.
///
/// `None` when the question does not apply: nothing tracked, or nothing
/// recorded to compare against -- the caller must not read either as "up to
/// date".
pub async fn vcs_sources_moved(
    db: &DatabaseConnection,
    package_id: i32,
    sourceinfo: &SourceInfoV1,
) -> anyhow::Result<Option<bool>> {
    if extract_git_vcs_sources(sourceinfo).is_empty() {
        return Ok(None);
    }
    let built = latest_successful_build_vcs_sources(db, package_id).await?;
    if built.is_empty() {
        return Ok(None);
    }
    let now = resolve_vcs_commits(sourceinfo).await;
    if now.is_empty() {
        return Ok(None);
    }
    // A source with nothing recorded for it is a source we cannot vouch for,
    // so the answer is "moved" rather than a shrug: better a rebuild than a
    // package silently pinned to a commit nobody chose.
    Ok(Some(now.iter().any(|(source_url, commit)| {
        built.get(source_url) != Some(commit)
    })))
}

/// Whether a source now at `commit` counts as moved.
///
/// `built` is what the last successful build was made from and is the honest
/// answer; `watermark` is what the version check last saw, used only where the
/// build recorded nothing -- a package built before this was recorded, or a
/// source that could not be resolved at the time. Unknown is never read as
/// unchanged, and nothing is dragged through a rebuild merely for having no
/// record yet.
fn source_moved(
    built: &BTreeMap<String, String>,
    watermark: &HashMap<String, String>,
    source_url: &str,
    commit: &str,
) -> bool {
    match built.get(source_url) {
        Some(built_commit) => built_commit != commit,
        None => watermark.get(source_url).map(String::as_str) != Some(commit),
    }
}

#[cfg(test)]
mod tests {
    use super::source_moved;
    use std::collections::{BTreeMap, HashMap};

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    /// What a build recorded, in the shape the column deserializes to.
    fn built(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    /// What we built is the baseline, even when the check's watermark agrees
    /// with the remote: the watermark only moves when the check looks, so a
    /// build triggered any other way left it behind. Three of
    /// `ttf-google-fonts-git`'s builds in one afternoon were that.
    #[test]
    fn what_was_built_outranks_what_the_check_last_saw() {
        let built = built(&[("src", "new")]);
        let watermark = map(&[("src", "old")]);

        assert!(
            !source_moved(&built, &watermark, "src", "new"),
            "the remote is where we built from, so nothing has moved"
        );
        assert!(
            source_moved(&built, &watermark, "src", "newer"),
            "and when it really has moved, it is still detected"
        );
    }

    /// Without a record the old comparison stands, so the table arriving empty
    /// does not flag every VCS package at once.
    #[test]
    fn with_nothing_recorded_the_watermark_decides() {
        let built = BTreeMap::new();
        let watermark = map(&[("src", "old")]);

        assert!(!source_moved(&built, &watermark, "src", "old"));
        assert!(source_moved(&built, &watermark, "src", "new"));
    }

    /// A source nobody has any record of is new, and new is a reason to build.
    #[test]
    fn an_unknown_source_counts_as_moved() {
        assert!(source_moved(
            &BTreeMap::new(),
            &HashMap::new(),
            "src",
            "whatever"
        ));
    }

    /// One package's sources are keyed separately: a build that recorded only
    /// one of two must not vouch for the other.
    #[test]
    fn each_source_is_judged_on_its_own_record() {
        let built = built(&[("a", "a1")]);
        let watermark = map(&[("a", "a1"), ("b", "b1")]);

        assert!(!source_moved(&built, &watermark, "a", "a1"));
        assert!(
            !source_moved(&built, &watermark, "b", "b1"),
            "b has no build record, so its watermark answers"
        );
        assert!(source_moved(&built, &watermark, "b", "b2"));
    }
}
