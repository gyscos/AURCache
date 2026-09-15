//! A cgroup per build, for an exact peak-memory figure.
//!
//! cgroup v2 records `memory.peak` for a cgroup and everything in it, so giving
//! each build its own answers "how much did this build need" exactly, with no
//! sampling and nothing to poll. The build is a tree -- `makechrootpkg` runs
//! `systemd-nspawn` runs `makepkg` runs one compiler per core -- and every
//! descendant inherits the cgroup its parent was placed in, so one number
//! covers all of them.
//!
//! ## Why the worker has to prepare its own hierarchy
//!
//! cgroup v2 refuses to enable a controller in `cgroup.subtree_control` while
//! processes sit directly in that cgroup (`EBUSY`), and both deployments hand
//! the worker a cgroup it is already running in. So at startup the worker moves
//! itself into a leaf and enables `memory` for its siblings -- the standard
//! delegation dance, and what systemd's own documentation tells a delegated
//! service to do.
//!
//! Both supported deployments can do this:
//!
//!  * **The container images** run privileged, which `mkarchroot` and
//!    `arch-nspawn` already require, and that makes `/sys/fs/cgroup` writable.
//!  * **A native install** runs under `aurcache-worker.service`, which sets
//!    `Delegate=yes` so systemd hands the subtree over rather than managing it.
//!
//! Where the hierarchy cannot be prepared, builds still run and simply report
//! no figure; nothing here is on the path that produces packages.

use anyhow::{Context, Result, bail};
use std::fs;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};

/// Where cgroup v2 is mounted. Fixed by convention and by systemd.
const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// The leaf the worker moves itself into so its own cgroup can host children.
const SELF_LEAF: &str = "worker";

/// The prepared cgroup the worker creates each build's cgroup under.
pub struct Hierarchy {
    base: PathBuf,
}

impl Hierarchy {
    /// Move this process into a leaf and enable the memory controller for its
    /// siblings, so a per-build cgroup can be created and measured.
    ///
    /// Idempotent: a worker that restarts into an already-prepared hierarchy
    /// finds the controller enabled and only re-parents itself.
    pub fn prepare() -> Result<Self> {
        let base = own_cgroup_dir()?;

        // Nothing can be enabled while this cgroup holds processes, so the move
        // has to come first -- including this process, which is why the leaf
        // exists at all.
        let leaf = base.join(SELF_LEAF);
        fs::create_dir_all(&leaf).with_context(|| format!("creating {}", leaf.display()))?;
        for pid in read_procs(&base.join("cgroup.procs"))? {
            // Best-effort per pid: a process that exits between the read and
            // the write is gone, which is the outcome the move wanted anyway.
            let _ = fs::write(leaf.join("cgroup.procs"), pid.to_string());
        }

        let control = base.join("cgroup.subtree_control");
        if !fs::read_to_string(&control)
            .unwrap_or_default()
            .split_whitespace()
            .any(|c| c == "memory")
        {
            fs::write(&control, "+memory").with_context(|| {
                format!(
                    "enabling the memory controller in {} -- the worker needs a \
                     writable cgroup subtree (privileged container, or a systemd \
                     unit with Delegate=yes)",
                    control.display()
                )
            })?;
        }

        Ok(Self { base })
    }

    /// Create the cgroup for one build.
    pub fn for_build(&self, build_id: i32) -> Result<BuildCgroup> {
        let dir = self.base.join(format!("build-{build_id}"));
        // A cgroup left by a previous attempt at the same build is empty by
        // now; removing it is what keeps `memory.peak` about this attempt.
        let _ = remove_cgroup_tree(&dir);
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(BuildCgroup { dir })
    }
}

/// One build's cgroup. Removed when dropped.
pub struct BuildCgroup {
    dir: PathBuf,
}

impl BuildCgroup {
    /// An open handle to `cgroup.procs`, for the child to place itself into.
    ///
    /// Opened here rather than in the child because opening a file after
    /// `fork` in a threaded process is not async-signal-safe. The child writes
    /// a single byte to this descriptor; see [`Self::join_current_process`].
    pub fn procs_handle(&self) -> Result<OwnedFd> {
        let file = fs::OpenOptions::new()
            .write(true)
            .open(self.dir.join("cgroup.procs"))
            .with_context(|| format!("opening cgroup.procs in {}", self.dir.display()))?;
        Ok(file.into())
    }

    /// Move the calling process into the cgroup behind `handle`.
    ///
    /// Called between `fork` and `exec`, where almost nothing is legal: this is
    /// one `write` of one byte to a descriptor that was already open. Writing
    /// `0` rather than a pid is what keeps it that way -- the kernel reads it
    /// as "whoever is writing" -- so there is no formatting and no allocation.
    ///
    /// # Safety
    /// Must only be called from a `pre_exec` hook, with `handle` opened before
    /// the fork.
    pub unsafe fn join_current_process(handle: &OwnedFd) -> std::io::Result<()> {
        // SAFETY: a single write to an open descriptor is async-signal-safe.
        let written = unsafe { libc::write(handle.as_raw_fd(), c"0".as_ptr().cast(), 1) };
        if written == 1 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    /// The high-water mark of this cgroup's memory, in bytes.
    ///
    /// `None` when the kernel does not report one: `memory.peak` needs Linux
    /// 5.19 or newer. Distinct from a build that used nothing, which cannot
    /// happen, so the page can show the two differently.
    #[must_use]
    pub fn peak_bytes(&self) -> Option<i64> {
        let raw = fs::read_to_string(self.dir.join("memory.peak")).ok()?;
        parse_peak(&raw)
    }

    /// Kill every process in this cgroup, including descendants.
    ///
    /// `cgroup.kill` is v2's recursive kill: it signals the whole tree — the
    /// nspawn child, `makechrootpkg`, each compiler — in one write, which is
    /// exactly the tree a build is. Only the leaf's processes are listed by
    /// `cgroup.procs`; `cgroup.kill` walks the descendants for us.
    ///
    /// This is the primary kill path for an aborted (cancelled) or timed-out
    /// build: `child.start_kill()` would only take `makechrootpkg`, and the
    /// processes it spawned (possibly in a systemd-managed scope that escapes
    /// the process group) would linger. Falls back to the outside caller.
    pub fn kill(&self) -> Result<()> {
        fs::write(self.dir.join("cgroup.kill"), "1")
            .with_context(|| format!("writing cgroup.kill in {}", self.dir.display()))
    }
}

impl Drop for BuildCgroup {
    fn drop(&mut self) {
        // Only ever empty by now -- the build's processes have been waited on.
        // A cgroup that somehow still holds one cannot be removed, and leaving
        // it is better than blocking the worker: `for_build` clears it next
        // time the same build number comes round.
        if let Err(e) = remove_cgroup_tree(&self.dir) {
            tracing::debug!("could not remove {}: {e}", self.dir.display());
        }
    }
}

/// Remove a cgroup and every cgroup beneath it, deepest first.
///
/// cgroupfs refuses `rmdir` on a cgroup that still has child cgroups, and a
/// build's has two: systemd-nspawn, kept in the build's cgroup with
/// `--keep-unit`, creates `payload` and `supervisor` inside it. Removing only
/// the top left every build's cgroup behind for good. The files in a cgroup
/// are the kernel's and go with its directory, so only directories are
/// removed -- which is also all `rmdir` can do, so nothing outside cgroupfs is
/// at risk.
fn remove_cgroup_tree(dir: &Path) -> std::io::Result<()> {
    for entry in fs::read_dir(dir)?.flatten() {
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            remove_cgroup_tree(&entry.path())?;
        }
    }
    fs::remove_dir(dir)
}

/// This process's own cgroup directory.
fn own_cgroup_dir() -> Result<PathBuf> {
    let raw = fs::read_to_string("/proc/self/cgroup").context("reading /proc/self/cgroup")?;
    let Some(rel) = parse_own_cgroup(&raw) else {
        bail!("no cgroup v2 entry in /proc/self/cgroup; this needs a unified hierarchy");
    };
    Ok(Path::new(CGROUP_ROOT).join(rel.trim_start_matches('/')))
}

/// The path from the cgroup v2 line of a `/proc/<pid>/cgroup`.
///
/// The unified hierarchy is the entry with an empty controller list, written
/// `0::<path>`. Any other line is a v1 controller and says nothing about where
/// `memory.peak` would be.
fn parse_own_cgroup(raw: &str) -> Option<&str> {
    raw.lines().find_map(|l| l.strip_prefix("0::"))
}

fn read_procs(path: &Path) -> Result<Vec<u32>> {
    let raw = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(raw.lines().filter_map(|l| l.trim().parse().ok()).collect())
}

/// `memory.peak` is a single decimal number of bytes.
fn parse_peak(raw: &str) -> Option<i64> {
    raw.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape nspawn leaves under a build's cgroup. A plain directory tree
    /// stands in for cgroupfs, where the kernel's files go with each directory.
    #[test]
    fn a_build_cgroup_is_removed_with_the_cgroups_nspawn_made_in_it() {
        let tmp = tempfile::tempdir().unwrap();
        let build = tmp.path().join("build-997");
        fs::create_dir_all(build.join("payload/nested")).unwrap();
        fs::create_dir_all(build.join("supervisor")).unwrap();

        remove_cgroup_tree(&build).unwrap();

        assert!(!build.exists());
        assert!(tmp.path().exists());
    }

    /// The unified hierarchy is the `0::` line; v1 controller lines sit
    /// alongside it on a hybrid host and mean something else entirely.
    #[test]
    fn the_unified_hierarchy_is_the_zero_line() {
        let hybrid = "12:pids:/system.slice/x.service\n1:name=systemd:/user.slice\n0::/system.slice/aurcache-worker.service\n";
        assert_eq!(
            parse_own_cgroup(hybrid).unwrap(),
            "/system.slice/aurcache-worker.service"
        );
    }

    /// A container with a private cgroup namespace sees itself at the root.
    #[test]
    fn a_container_sees_itself_at_the_root() {
        assert_eq!(parse_own_cgroup("0::/\n").unwrap(), "/");
    }

    /// A v1-only host has no `0::` line, and there is no `memory.peak` to find.
    #[test]
    fn a_v1_only_hierarchy_has_no_unified_entry() {
        assert!(parse_own_cgroup("12:pids:/x\n1:name=systemd:/y\n").is_none());
    }

    #[test]
    fn peak_is_a_plain_byte_count() {
        assert_eq!(parse_peak("1552384\n").unwrap(), 1_552_384);
        assert!(parse_peak("").is_none());
    }
}
