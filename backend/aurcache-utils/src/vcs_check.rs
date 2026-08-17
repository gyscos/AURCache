//! Detects VCS (`git+...`) sources in a parsed `.SRCINFO` and resolves the
//! commit each currently points to on the remote, without cloning.
//!
//! This lets version checks flag VCS packages (`-git`, `-svn`, `-hg`, `-bzr`
//! style `pkgname`s, though only `git+` sources are currently supported) as
//! out-of-date when upstream has moved, even though the AUR-published
//! `pkgver` for such packages is typically stale (it only reflects when the
//! PKGBUILD itself was last touched, not the live upstream state).
use std::collections::{HashMap, HashSet};

use alpm_srcinfo::SourceInfoV1;
use alpm_types::Source;
use alpm_types::url::{GitFragment, VcsInfo};
use sea_orm::{ActiveValue::Set, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};

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

    // Pinned to an exact commit: never changes, nothing to track.
    if matches!(fragment, Some(GitFragment::Commit(_))) {
        return None;
    }

    let git_ref = match fragment {
        Some(GitFragment::Branch(name)) => name.clone(),
        Some(GitFragment::Tag(name)) => name.clone(),
        None => "HEAD".to_string(),
        Some(GitFragment::Commit(_)) => unreachable!("filtered out above"),
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

    let mut changed = false;
    let mut seen_urls: HashSet<String> = HashSet::new();
    let mut upserts = Vec::new();

    for source in vcs_sources {
        // `resolve_commit` performs blocking network I/O via git2.
        let source_url = source.source_url.clone();
        let commit = tokio::task::spawn_blocking(move || source.resolve_commit()).await??;

        seen_urls.insert(source_url.clone());

        if existing.get(&source_url) != Some(&commit) {
            changed = true;
        }

        upserts.push(package_vcs_sources::ActiveModel {
            id: Default::default(),
            package_id: Set(package_id),
            source_url: Set(source_url),
            last_commit: Set(commit),
            updated_at: Set(now_unix()),
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

fn now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
