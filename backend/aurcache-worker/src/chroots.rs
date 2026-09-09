//! Where a build gets its chroot, and how it gives it back.
//!
//! `makechrootpkg` builds in a *copy* of a base chroot, and how that copy is
//! made -- devtools copying it itself, a btrfs snapshot, a reflink, an overlay
//! mount -- settles three other things at once: what `build_command` passes,
//! who deletes the copy when the build ends, and what the startup sweep must do
//! about one a crash left behind. Answered separately in three files they drift
//! apart, so they are answered here together, behind an interface that says
//! only "give me a chroot" and "here it is back".
//!
//! Today there is one strategy, and it is the one devtools has always used.
//! The seam exists because every alternative differs in exactly those three
//! answers and in nothing else the rest of the worker cares about.

use crate::chroot;
use anyhow::Result;
use std::path::{Path, PathBuf};

/// How a build's chroot copy is made and destroyed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    /// `makechrootpkg` makes the copy (`-c`) and deletes it (`-T`); the worker
    /// only names it.
    DevtoolsCopy,
}

impl Strategy {
    /// Whether giving a chroot back has any work to do.
    ///
    /// `false` here is why a dropped lease is not a leak today: devtools has
    /// already deleted the copy by the time the lease goes out of scope.
    const fn needs_release(self) -> bool {
        match self {
            Self::DevtoolsCopy => false,
        }
    }
}

/// The chroots on this worker: one base, and a copy per build.
pub struct Chroots {
    dir: PathBuf,
    strategy: Strategy,
}

impl Chroots {
    #[must_use]
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            strategy: Strategy::DevtoolsCopy,
        }
    }

    /// Ensure the base chroot exists and is reasonably fresh.
    pub async fn refresh(&self, pacman_conf: &Path) -> Result<PathBuf> {
        chroot::ensure_base_chroot(&self.dir, pacman_conf).await
    }

    /// Take a chroot for one build. Give it back with [`Lease::release`].
    pub async fn acquire(&self, label: &str) -> Result<Lease> {
        Ok(Lease {
            dir: self.dir.clone(),
            label: label.to_string(),
            strategy: self.strategy,
            released: false,
        })
    }

    /// Run `body` with a chroot and give it back afterwards -- on the error
    /// path too, which is the half a caller forgets.
    pub async fn with_lease<T, F>(&self, label: &str, body: F) -> Result<T>
    where
        F: AsyncFnOnce(&Lease) -> Result<T>,
    {
        let lease = self.acquire(label).await?;
        let out = body(&lease).await;
        lease.release().await;
        out
    }

    /// Reclaim copies a previous run left behind. This is the only real
    /// guarantee: `release` and `Drop` both lose to `SIGKILL`.
    pub async fn sweep(&self) -> u64 {
        chroot::remove_stale_copies(&self.dir).await
    }
}

/// One build's claim on a chroot.
pub struct Lease {
    dir: PathBuf,
    label: String,
    strategy: Strategy,
    released: bool,
}

impl Lease {
    /// The `-r` argument: the directory the copy lives in.
    #[must_use]
    pub fn chroot_dir(&self) -> &Path {
        &self.dir
    }

    /// The `-l` argument: what this build's copy is called.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Whether `makechrootpkg` makes and deletes the copy itself.
    #[must_use]
    pub fn devtools_owns_copy(&self) -> bool {
        matches!(self.strategy, Strategy::DevtoolsCopy)
    }

    /// Give the chroot back.
    ///
    /// Awaited rather than left to `Drop`, for two reasons that outlive the
    /// current strategy: teardown can fail and the failure is worth reporting,
    /// and it has to happen *after* the build's child has been reaped --
    /// unmounting under a live process fails with `EBUSY`, and drop order is a
    /// poor place to state that dependency.
    pub async fn release(mut self) {
        self.released = true;
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        // A backstop for panics and early returns, in the shape `BuildCgroup`
        // and `BuildAgent` already use: best effort, never fatal, and never the
        // path anything relies on. Silent today, because devtools has already
        // deleted the copy.
        if !self.released && self.strategy.needs_release() {
            tracing::warn!(
                "chroot lease {} dropped without release; the next sweep reclaims it",
                self.label
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lease() -> Lease {
        Lease {
            dir: PathBuf::from("/var/lib/aurcache-chroot"),
            label: "job-42".to_string(),
            strategy: Strategy::DevtoolsCopy,
            released: false,
        }
    }

    /// A lease speaks devtools -- `-r`, `-l`, and who owns the copy -- so that
    /// nothing above this module has to know what a copy actually is.
    #[test]
    fn a_lease_names_what_makechrootpkg_needs() {
        let lease = lease();
        assert_eq!(lease.chroot_dir(), Path::new("/var/lib/aurcache-chroot"));
        assert_eq!(lease.label(), "job-42");
        assert!(lease.devtools_owns_copy());
    }

    /// Nothing to give back while devtools owns the copy, which is what makes
    /// dropping a lease harmless today rather than a leaked mount.
    #[test]
    fn a_devtools_copy_needs_no_release() {
        assert!(!Strategy::DevtoolsCopy.needs_release());
    }
}
