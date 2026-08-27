//! A build worker, as the operator's view of the fleet sees it.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// What the workers list shows about one machine.
///
/// Deliberately not the database row. That carries `signed_cert` — the PEM
/// issued at approval — and there is no reason to hand every browser that opens
/// the page a copy of it. The worker gets its own certificate through
/// enrollment, not from this list.
#[derive(Deserialize, ToSchema, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct WorkerSummary {
    pub id: i32,
    /// Operator-facing name reported at enrollment.
    pub name: String,
    pub status: ApprovalStatus,
    /// SHA-256 fingerprint of the worker's certificate: the stable identity
    /// behind the name, which is not unique.
    pub cert_fingerprint: String,
    /// Architectures built natively.
    pub native_arches: Vec<String>,
    /// Architectures built under emulation — slower, and worth telling apart.
    pub emulated_arches: Vec<String>,
    /// Exact pkgbases this worker is provisioned for. A package named by *any*
    /// approved worker may only be built by workers that name it, so this is a
    /// restriction on the package as much as a capability of the worker.
    pub package_affinity: Vec<String>,
    /// Scheduling preference; higher wins. Zero means no preference.
    pub priority: i32,
    /// Unix seconds of the last contact, or `None` if it has never called in.
    pub last_seen: Option<i64>,
    /// Worker software version reported at enrollment or heartbeat.
    pub version: Option<String>,
}

/// Where a worker is in the approval workflow.
///
/// A closed set rather than the string the database holds: the frontend
/// switches on it, and an unrecognised status should be resolved once at the
/// edge rather than re-guessed at every use.
///
/// Named apart from [`crate::worker::WorkerStatus`], which is the bag of
/// string constants the column is written with. This is the typed reading of
/// the same field, and [`Self::from_db`] parses those constants rather than
/// re-spelling them.
#[derive(Deserialize, ToSchema, Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalStatus {
    /// Enrolled and waiting for an operator. Cannot build.
    Pending,
    /// Approved: holds a signed certificate and may take jobs.
    Approved,
    /// Refused. Its certificate no longer works, and its reservations and
    /// in-flight builds have been released.
    Revoked,
}

impl ApprovalStatus {
    /// Parse the string the database stores.
    ///
    /// Anything unrecognised is treated as revoked: the statuses that grant
    /// capability are the ones worth being sure about, so an unreadable value
    /// should deny rather than allow.
    pub fn from_db(status: &str) -> Self {
        match status {
            crate::worker::WorkerStatus::APPROVED => Self::Approved,
            crate::worker::WorkerStatus::PENDING => Self::Pending,
            _ => Self::Revoked,
        }
    }

    /// Whether this worker can currently take jobs.
    pub fn can_build(self) -> bool {
        matches!(self, Self::Approved)
    }

    /// Whether it is retired. Revoked rows are kept so build history still
    /// resolves to the machine that produced it, so the list would otherwise
    /// grow without bound.
    pub fn is_retired(self) -> bool {
        matches!(self, Self::Revoked)
    }
}

#[cfg(test)]
mod tests {
    use super::ApprovalStatus;

    #[test]
    fn statuses_parse_from_what_the_database_holds() {
        assert_eq!(ApprovalStatus::from_db("pending"), ApprovalStatus::Pending);
        assert_eq!(
            ApprovalStatus::from_db("approved"),
            ApprovalStatus::Approved
        );
        assert_eq!(ApprovalStatus::from_db("revoked"), ApprovalStatus::Revoked);
    }

    /// An unreadable status must not grant the ability to build.
    #[test]
    fn an_unknown_status_denies_rather_than_allows() {
        let unknown = ApprovalStatus::from_db("something-new");
        assert!(!unknown.can_build());
        assert_eq!(unknown, ApprovalStatus::Revoked);
    }

    #[test]
    fn only_approved_workers_can_build() {
        assert!(ApprovalStatus::Approved.can_build());
        assert!(!ApprovalStatus::Pending.can_build());
        assert!(!ApprovalStatus::Revoked.can_build());
        assert!(ApprovalStatus::Revoked.is_retired());
        assert!(!ApprovalStatus::Pending.is_retired());
    }
}
