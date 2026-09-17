//! Resolving a package's source: the checkout, its `.SRCINFO`, its VCS refs.
//!
//! Every one of these leaves the package's own tracking un-updated for that
//! round while the rest of the check carries on, which is exactly the kind of
//! failure that used to be visible only in the server's journal.

use crate::event::LogEvent;
use aurcache_common::api::activity::Severity;
use aurcache_common::api::log::PackageRef;
use serde::{Deserialize, Serialize};

/// The source checkout could not be brought up to date.
///
/// One kind for the git remote and the snapshot cache alike: both mean "this
/// package's source is as stale as it was", and the reason says which.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefreshFailed {
    pub pkg: PackageRef,
    pub error: String,
}

impl RefreshFailed {
    pub const KIND_STR: &'static str = "source.refresh_failed";
}

impl LogEvent for RefreshFailed {
    const KIND: &'static str = Self::KIND_STR;
    const SEVERITY: Severity = Severity::Warning;
    fn message(&self) -> String {
        format!(
            "could not refresh the source of {}: {}",
            self.pkg, self.error
        )
    }
}

/// The `.SRCINFO` could not be produced from the checkout.
///
/// A patch that no longer applies is the usual cause, and the build will fail
/// the same way later -- which is the point of not aborting the whole pass.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceinfoFailed {
    pub pkg: PackageRef,
    pub error: String,
}

impl SourceinfoFailed {
    pub const KIND_STR: &'static str = "source.sourceinfo_failed";
}

impl LogEvent for SourceinfoFailed {
    const KIND: &'static str = Self::KIND_STR;
    const SEVERITY: Severity = Severity::Warning;
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
