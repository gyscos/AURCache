use crate::api::waiting::WaitingReason;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

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
    /// Why this build is stuck, when it is `ENQUEUED` and *no* approved worker
    /// can currently take it. `None` for everything else, including a build
    /// merely waiting behind a busy worker — see
    /// `aurcache_db::helpers::worker_jobs::waiting_reasons`.
    #[cfg_attr(feature = "db", sea_orm(skip))]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting_reason: Option<WaitingReason>,
}
