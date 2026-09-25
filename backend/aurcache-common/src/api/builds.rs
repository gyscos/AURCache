use crate::api::waiting::WaitingReason;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Disk a build used on its worker, in bytes as stored -- after the pool's
/// compression -- when the build ended.
///
/// Each part is `None` when it was not measured, which is not the same as `0`:
/// a package that keeps no build tree has no `build_tree` figure at all. A
/// total over the parts is only meaningful when every part it adds is known.
#[derive(Deserialize, ToSchema, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DiskUsage {
    /// What the build wrote into its chroot: mostly the dependencies it
    /// installed. The base chroot it started from is not counted.
    pub chroot: Option<i64>,
    /// The build's own working space: its extracted source, what the source
    /// download left there, and the packages it made.
    pub workdir: Option<i64>,
    /// The package's source cache (`SRCDEST`), which outlives the build and
    /// is shared with its later builds.
    pub sources: Option<i64>,
    /// The package's kept build tree, for a package that keeps one.
    pub build_tree: Option<i64>,
}

impl DiskUsage {
    /// Whether nothing at all was measured.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.chroot.is_none()
            && self.workdir.is_none()
            && self.sources.is_none()
            && self.build_tree.is_none()
    }
}

#[derive(Deserialize, ToSchema, Serialize, Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "db", derive(sea_orm::FromQueryResult))]
pub struct BuildSummary {
    /// This build's number within its package, counting from 1.
    ///
    /// A build is publicly `<pkgbase>/<number>` — `hello/3` — so the row id is
    /// never exposed: it is a global sequence that says nothing about which
    /// package a build belongs to, and leaks how many builds the server has
    /// run in total.
    pub number: i32,
    pub pkg_name: String,
    pub version: String,
    pub status: i32,
    pub start_time: Option<i64>,
    pub end_time: Option<i64>,
    pub platform: String,
    /// Total size in bytes of the artifacts this build produced.
    ///
    /// `None` for a build that produced nothing to measure -- one that failed,
    /// is still running, or is queued -- and for a successful build that ran
    /// before the size was recorded. This is one platform's output; a package's
    /// total covers every platform it builds for.
    pub size: Option<i64>,
    /// High-water mark of the build's process tree memory, in bytes.
    ///
    /// Exact rather than sampled: the build runs in a cgroup of its own and
    /// this is that cgroup's `memory.peak`, covering every process in the tree.
    ///
    /// `None` where no figure was reported -- an older worker, the deprecated
    /// container builder (Docker exposes no peak on cgroup v2), or a worker
    /// whose cgroup subtree could not be prepared.
    pub peak_memory: Option<i64>,
    /// Disk the build used on its worker, part by part. `None` where no
    /// figure was reported: an older worker, or one without a storage pool.
    #[cfg_attr(feature = "db", sea_orm(skip))]
    #[serde(default)]
    pub disk_usage: Option<DiskUsage>,
    /// The worker that ran this build, by name.
    ///
    /// `None` where no worker has claimed it -- a queued build -- and for
    /// builds recorded before builds remembered which worker ran them. The
    /// name rather than the id because it is what the lists show and what an
    /// operator searches for; a renamed worker relabels its history, which is
    /// the same thing the Workers page does.
    pub worker_name: Option<String>,
    /// Size of this build's stored log, in bytes.
    ///
    /// `None` for a build whose log file does not exist -- one that produced no
    /// output, or whose log has been removed. Only the single-build detail
    /// route fills it; list endpoints would pay a metadata stat per row for a
    /// field the lists do not show. `0` is a real answer (an empty log file)
    /// and is never conflated with "no log".
    #[cfg_attr(feature = "db", sea_orm(skip))]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_size: Option<i64>,
    /// Why this build is stuck, when it is `ENQUEUED` and *no* approved worker
    /// can currently take it. `None` for everything else, including a build
    /// merely waiting behind a busy worker — see
    /// `aurcache_db::helpers::worker_jobs::waiting_reasons`.
    #[cfg_attr(feature = "db", sea_orm(skip))]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting_reason: Option<WaitingReason>,
}
