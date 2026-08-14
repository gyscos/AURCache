use anyhow::anyhow;
use aurcache_db::packages::GitSourceSpec;
use git2::{Oid, Repository};
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

/// Resolve `git_ref` to an object in `repo` and checkout its tree, updating HEAD.
/// Shared between a fresh clone and a re-used, freshly-fetched repo.
fn resolve_and_checkout(repo: &Repository, git_ref: &str) -> anyhow::Result<Oid> {
    // Resolve the ref to an object
    let (object, reference) = repo.revparse_ext(git_ref)?;

    // Checkout the tree (updates working directory), forcing it to match
    // exactly in case a previous checkout left local modifications (e.g. a
    // dirty working dir from an interrupted build).
    let mut checkout_builder = git2::build::CheckoutBuilder::new();
    checkout_builder.force();
    repo.checkout_tree(&object, Some(&mut checkout_builder))?;

    // If it's a branch or tag, make HEAD point to it
    if let Some(reference) = reference {
        repo.set_head(
            reference
                .name()
                .map_err(|_| anyhow!("Reference name invalid"))?,
        )?;
    } else {
        // Detached HEAD for a commit hash
        repo.set_head_detached(object.id())?;
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
