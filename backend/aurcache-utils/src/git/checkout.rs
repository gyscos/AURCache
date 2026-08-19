use anyhow::anyhow;
use aurcache_db::packages::GitSourceSpec;
use git2::{Direction, Oid, Repository};
use std::path::{Path, PathBuf};

/// checkout git repo at specific ref
/// parts of this are not 'Send' so they need to be scoped
pub fn checkout_repo_ref(
    git_repo: String,
    git_ref: String,
    path: PathBuf,
) -> anyhow::Result<Repository> {
    // checkout repo
    let repo = Repository::clone(git_repo.as_str(), &path)?;
    resolve_and_checkout(&repo, &git_ref)?;
    Ok(repo)
}

/// Checkout a shared git source spec into the given path.
pub fn checkout_git_source(spec: &GitSourceSpec, path: PathBuf) -> anyhow::Result<Repository> {
    checkout_repo_ref(spec.url.clone(), spec.r#ref.clone(), path)
}

/// Resolve `git_ref` against the *fetched remote* state, falling back to a
/// local resolve.
///
/// `fetch` only advances remote-tracking refs (`refs/remotes/origin/*`). The
/// local branch — and therefore local `HEAD` — stays wherever the initial clone
/// left it. Resolving locally would pin a re-used checkout to its first-cloned
/// commit forever: upstream could publish any number of new versions and every
/// subsequent resolve would still return the original commit and the original
/// `.SRCINFO`.
///
/// AUR sources are resolved with `git_ref = "HEAD"`, so they hit exactly that
/// path; the symptom is AURCache reporting an available update (that check uses
/// the live AUR RPC) while the update itself refuses with "Latest build is
/// already up to date", reading the stale pkgver off the frozen checkout.
///
/// Tags and raw commit SHAs have no remote-tracking equivalent, so a local
/// resolve remains the fallback.
fn resolve_remote_ref<'a>(
    repo: &'a Repository,
    git_ref: &str,
) -> anyhow::Result<(git2::Object<'a>, Option<git2::Reference<'a>>)> {
    let candidates: Vec<String> = if git_ref == "HEAD" {
        // `origin/HEAD` is only set if the remote advertised it; fall back to
        // the conventional default branch names (AUR uses `master`).
        vec![
            "refs/remotes/origin/HEAD".to_string(),
            "refs/remotes/origin/master".to_string(),
            "refs/remotes/origin/main".to_string(),
        ]
    } else {
        // A caller-supplied `origin/foo` is already remote-tracking.
        vec![
            format!("refs/remotes/origin/{git_ref}"),
            git_ref.to_string(),
        ]
    };

    for candidate in &candidates {
        if let Ok(resolved) = repo.revparse_ext(candidate) {
            return Ok(resolved);
        }
    }
    repo.revparse_ext(git_ref)
        .map_err(|e| anyhow!("could not resolve git ref '{git_ref}': {e}"))
}

/// Resolve `git_ref` to an object in `repo` and checkout its tree, updating HEAD.
/// Shared between a fresh clone and a re-used, freshly-fetched repo.
fn resolve_and_checkout(repo: &Repository, git_ref: &str) -> anyhow::Result<Oid> {
    // Resolve the ref to an object
    let (object, reference) = resolve_remote_ref(repo, git_ref)?;

    // Checkout the tree (updates working directory), forcing it to match
    // exactly in case a previous checkout left local modifications (e.g. a
    // dirty working dir from an interrupted build).
    let mut checkout_builder = git2::build::CheckoutBuilder::new();
    checkout_builder.force();
    repo.checkout_tree(&object, Some(&mut checkout_builder))?;

    // If it's a local branch or tag, make HEAD point to it. A remote-tracking
    // ref must not become HEAD (git would treat the checkout as being "on"
    // origin/master), so detach onto the commit instead — this is a read-only
    // source cache, nothing commits here.
    let local_ref_name = reference
        .as_ref()
        .and_then(|r| r.name().ok())
        .filter(|name| !name.starts_with("refs/remotes/"));
    match local_ref_name {
        Some(name) => repo.set_head(name)?,
        None => repo.set_head_detached(object.id())?,
    }
    Ok(object.id())
}

/// Open the repo at `path` if it already exists, otherwise clone `git_repo` into it.
/// Either way, fetch the latest state of `git_ref` from the remote and check it out,
/// returning the resolved commit id.
///
/// This allows repeated calls (e.g. from a persistent on-disk cache) to reuse the
/// existing clone and only transfer new objects via `fetch`, instead of re-cloning
/// the whole repository every time.
pub fn checkout_or_fetch_repo_ref(
    git_repo: &str,
    git_ref: &str,
    path: &Path,
) -> anyhow::Result<Oid> {
    let repo = if path.join(".git").exists() {
        let repo = Repository::open(path)?;
        // Make sure `origin` still points at the expected URL (it may have
        // changed if the package's source spec was edited).
        {
            let mut remote = match repo.find_remote("origin") {
                Ok(remote) => remote,
                Err(_) => repo.remote("origin", git_repo)?,
            };
            if remote.url().ok() != Some(git_repo) {
                repo.remote_set_url("origin", git_repo)?;
                remote = repo.find_remote("origin")?;
            }
            // Fetch using the remote's default refspecs (branches/tags) so that
            // `git_ref` can later be resolved by `revparse_ext`. Also try to fetch
            // `git_ref` directly, to cover the case of a raw commit SHA that isn't
            // reachable from any branch/tag tip.
            remote.fetch(&[] as &[&str], None, None)?;
            let _ = remote.fetch(&[git_ref], None, None);
        }
        repo
    } else {
        std::fs::create_dir_all(path)?;
        Repository::clone(git_repo, path)?
    };

    resolve_and_checkout(&repo, git_ref)
}

/// Resolve the commit that `git_ref` currently points to on the remote
/// `git_repo`, without cloning or fetching any objects (equivalent to
/// `git ls-remote <repo> <ref>`).
///
/// Uses an in-memory (non-cloned) `git2` remote connection. `git_ref` may be
/// a branch name, tag name, or `HEAD`; ambiguous short names are resolved the
/// same way the remote's advertised ref list would allow (exact ref name,
/// then `refs/heads/<name>`, then `refs/tags/<name>`).
pub fn ls_remote(git_repo: &str, git_ref: &str) -> anyhow::Result<String> {
    // A throwaway repository is required to create a remote in git2, even
    // for a purely in-memory listing; it performs no disk I/O for the actual
    // remote sources.
    let dir = tempfile::tempdir()?;
    let repo = Repository::init_bare(dir.path())?;
    let mut remote = repo.remote_anonymous(git_repo)?;
    remote.connect(Direction::Fetch)?;

    let heads = remote.list()?;
    let candidates = [
        git_ref.to_string(),
        format!("refs/heads/{git_ref}"),
        format!("refs/tags/{git_ref}"),
        "HEAD".to_string(),
    ];

    let result = candidates
        .iter()
        .find_map(|candidate| heads.iter().find(|h| h.name() == candidate))
        .map(|head| head.oid().to_string());

    remote.disconnect()?;
    drop(remote);
    drop(repo);
    dir.close()?;

    result.ok_or_else(|| anyhow!("Ref '{git_ref}' not found on remote '{git_repo}'"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::Signature;

    fn commit(repo: &Repository, pkgver: &str) {
        let workdir = repo.workdir().unwrap();
        std::fs::write(workdir.join("PKGBUILD"), format!("pkgver={pkgver}\n")).unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("PKGBUILD")).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = Signature::now("Test", "test@example.com").unwrap();
        let parents: Vec<_> = repo
            .head()
            .ok()
            .and_then(|h| h.target())
            .and_then(|oid| repo.find_commit(oid).ok())
            .into_iter()
            .collect();
        let parent_refs: Vec<_> = parents.iter().collect();
        repo.commit(Some("HEAD"), &sig, &sig, pkgver, &tree, &parent_refs)
            .unwrap();
    }

    /// Regression: a re-used checkout must follow upstream.
    ///
    /// `fetch` only moves remote-tracking refs, so resolving the *local* `HEAD`
    /// pinned the checkout to its first-cloned commit permanently. AUR sources
    /// resolve with `git_ref = "HEAD"`, so every AUR package's cached source was
    /// frozen at whatever it was when first fetched: AURCache would report an
    /// update available (from the live AUR RPC) while `package_update` read the
    /// stale pkgver off this checkout and refused with "Latest build is already
    /// up to date".
    #[test]
    fn reused_checkout_follows_upstream_head() {
        let upstream_dir = tempfile::tempdir().unwrap();
        let upstream = Repository::init(upstream_dir.path()).unwrap();
        commit(&upstream, "1.0");

        let cache = tempfile::tempdir().unwrap();
        let path = cache.path().join("pkg");
        let url = upstream_dir.path().to_string_lossy().to_string();

        let first = checkout_or_fetch_repo_ref(&url, "HEAD", &path).unwrap();
        assert!(
            std::fs::read_to_string(path.join("PKGBUILD"))
                .unwrap()
                .contains("1.0")
        );

        // The AUR maintainer pushes a new pkgver.
        commit(&upstream, "2.0");

        let second = checkout_or_fetch_repo_ref(&url, "HEAD", &path).unwrap();
        assert_ne!(first, second, "checkout did not advance to the new commit");
        assert!(
            std::fs::read_to_string(path.join("PKGBUILD"))
                .unwrap()
                .contains("2.0"),
            "working tree still holds the pre-update PKGBUILD"
        );
    }

    /// A named branch must track upstream across re-fetches too.
    #[test]
    fn reused_checkout_follows_named_branch() {
        let upstream_dir = tempfile::tempdir().unwrap();
        let upstream = Repository::init(upstream_dir.path()).unwrap();
        commit(&upstream, "1.0");
        let branch = upstream.head().unwrap().shorthand().unwrap().to_string();

        let cache = tempfile::tempdir().unwrap();
        let path = cache.path().join("pkg");
        let url = upstream_dir.path().to_string_lossy().to_string();

        let first = checkout_or_fetch_repo_ref(&url, &branch, &path).unwrap();
        commit(&upstream, "2.0");
        let second = checkout_or_fetch_repo_ref(&url, &branch, &path).unwrap();

        assert_ne!(first, second, "branch checkout did not advance");
        assert!(
            std::fs::read_to_string(path.join("PKGBUILD"))
                .unwrap()
                .contains("2.0")
        );
    }

    /// A pinned commit SHA must stay pinned even as upstream moves.
    #[test]
    fn pinned_commit_sha_does_not_move() {
        let upstream_dir = tempfile::tempdir().unwrap();
        let upstream = Repository::init(upstream_dir.path()).unwrap();
        commit(&upstream, "1.0");
        let pinned = upstream.head().unwrap().target().unwrap().to_string();

        let cache = tempfile::tempdir().unwrap();
        let path = cache.path().join("pkg");
        let url = upstream_dir.path().to_string_lossy().to_string();

        let first = checkout_or_fetch_repo_ref(&url, &pinned, &path).unwrap();
        commit(&upstream, "2.0");
        let second = checkout_or_fetch_repo_ref(&url, &pinned, &path).unwrap();

        assert_eq!(first, second, "pinned SHA must not follow upstream");
        assert!(
            std::fs::read_to_string(path.join("PKGBUILD"))
                .unwrap()
                .contains("1.0")
        );
    }
}
