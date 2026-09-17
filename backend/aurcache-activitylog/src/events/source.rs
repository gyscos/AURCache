//! Resolving a package's source: the checkout, its `.SRCINFO`, its VCS refs.
//!
//! Every one of these leaves the package's own tracking un-updated for that
//! round while the rest of the check carries on, which is exactly the kind of
//! failure that used to be visible only in the server's journal.

use crate::event::LogEvent;
use aurcache_common::api::activity::Severity;
use aurcache_common::api::log::PackageRef;
use serde::{Deserialize, Serialize};

/// Which store a refresh was against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefreshTarget {
    /// The persistent checkout of a `git+` source.
    Git,
    /// The rendered snapshot of an AUR package's sources.
    Snapshot,
}

impl RefreshTarget {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::Snapshot => "snapshot",
        }
    }
}

/// The source checkout could not be brought up to date.
///
/// One kind for the git remote and the snapshot cache alike: both mean "this
/// package's source is as stale as it was". Which of the two is the subkind, so
/// the difference stays filterable without being two kinds nobody would ask
/// apart.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefreshFailed {
    pub pkg: PackageRef,
    pub target: RefreshTarget,
    pub error: String,
}

impl RefreshFailed {
    pub const KIND_STR: &'static str = "source.refresh_failed";
}

impl LogEvent for RefreshFailed {
    const KIND: &'static str = Self::KIND_STR;
    const SEVERITY: Severity = Severity::Warning;
    fn subkind(&self) -> Option<&'static str> {
        Some(self.target.as_str())
    }
    fn message(&self) -> String {
        format!(
            "could not refresh the {} source of {}: {}",
            self.target.as_str(),
            self.pkg,
            self.error
        )
    }
}

/// What the sourceinfo was being read for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceinfoPurpose {
    /// Reading the version the recipe declares.
    Version,
    /// Reading the `git+` sources, to see whether any has moved.
    Vcs,
}

impl SourceinfoPurpose {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Version => "version",
            Self::Vcs => "vcs",
        }
    }
}

/// The `.SRCINFO` could not be produced from the checkout.
///
/// A patch that no longer applies is the usual cause, and the build will fail
/// the same way later -- which is the point of not aborting the whole pass.
/// What it was being read *for* is the subkind, because that decides which of
/// the package's tracking stops being updated.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceinfoFailed {
    pub pkg: PackageRef,
    pub purpose: SourceinfoPurpose,
    pub error: String,
}

impl SourceinfoFailed {
    pub const KIND_STR: &'static str = "source.sourceinfo_failed";
}

impl LogEvent for SourceinfoFailed {
    const KIND: &'static str = Self::KIND_STR;
    const SEVERITY: Severity = Severity::Warning;
    fn subkind(&self) -> Option<&'static str> {
        Some(self.purpose.as_str())
    }
    fn message(&self) -> String {
        format!(
            "could not read the sourceinfo of {}: {}",
            self.pkg, self.error
        )
    }
}

/// The `git+` sources named by a package could not be resolved.
///
/// Its VCS freshness is then whatever it was, so a moved upstream will not be
/// noticed until a later round succeeds.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VcsSyncFailed {
    pub pkg: PackageRef,
    pub error: String,
}

impl VcsSyncFailed {
    pub const KIND_STR: &'static str = "vcs.sync_failed";
}

impl LogEvent for VcsSyncFailed {
    const KIND: &'static str = Self::KIND_STR;
    const SEVERITY: Severity = Severity::Warning;
    fn message(&self) -> String {
        format!(
            "could not sync the VCS sources of {}: {}",
            self.pkg, self.error
        )
    }
}
