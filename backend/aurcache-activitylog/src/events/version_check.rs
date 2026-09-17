//! Deciding whether a package is out of date, and queueing what is.

use crate::event::LogEvent;
use aurcache_common::api::activity::Severity;
use aurcache_common::api::log::PackageRef;
use serde::{Deserialize, Serialize};

/// The AUR no longer lists this package.
///
/// Its metadata still comes from the checkout, which continues to exist, so
/// this is worth saying rather than leaving to be inferred from a page that
/// stopped changing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AurMissing {
    pub pkg: PackageRef,
}

impl AurMissing {
    pub const KIND_STR: &'static str = "version_check.aur_missing";
}

impl LogEvent for AurMissing {
    const KIND: &'static str = Self::KIND_STR;
    const SEVERITY: Severity = Severity::Warning;
    fn message(&self) -> String {
        format!("{} is no longer in the AUR", self.pkg)
    }
}

/// The result of a check could not be written back.
///
/// The package then looks unchecked, and the next pass does the work again.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoreFailed {
    pub pkg: PackageRef,
    pub error: String,
}

impl StoreFailed {
    pub const KIND_STR: &'static str = "version_check.store_failed";
}

impl LogEvent for StoreFailed {
    const KIND: &'static str = Self::KIND_STR;
    const SEVERITY: Severity = Severity::Warning;
    fn message(&self) -> String {
        format!(
            "could not store the version check result for {}: {}",
            self.pkg, self.error
        )
    }
}

/// Two versions that could not be compared, so the check fell back to asking
/// only whether they differ.
///
/// The pair is the point: fused into a sentence they were something to read,
/// and apart they are something to filter on.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompareFallback {
    pub pkg: PackageRef,
    pub upstream_version: String,
    pub built_version: String,
}

impl CompareFallback {
    pub const KIND_STR: &'static str = "version.compare_fallback";
}

impl LogEvent for CompareFallback {
    const KIND: &'static str = Self::KIND_STR;
    const SEVERITY: Severity = Severity::Warning;
    fn message(&self) -> String {
        format!(
            "cannot compare versions for {}: upstream {:?} vs built {:?}",
            self.pkg, self.upstream_version, self.built_version
        )
    }
}

/// Packages were found to be out of date and none of them was queued.
///
/// The check worked and the builds did not happen, which looks exactly like
/// nothing being out of date unless it says so.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueueFailed {
    pub error: String,
}

impl QueueFailed {
    pub const KIND_STR: &'static str = "update.queue_failed";
}

impl LogEvent for QueueFailed {
    const KIND: &'static str = Self::KIND_STR;
    const SEVERITY: Severity = Severity::Error;
    fn message(&self) -> String {
        format!(
            "found packages out of date but could not queue their builds: {}",
            self.error
        )
    }
}
