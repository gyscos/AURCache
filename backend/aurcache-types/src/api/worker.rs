//! A build worker, as the operator's view of the fleet sees it.

#[cfg(feature = "db")]
use sea_orm::sea_query::StringLen;
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
    /// Whether it has checked in recently enough to be considered connected.
    ///
    /// Derived server-side from `last_seen` and the liveness timeout, because
    /// the timeout is the server's setting — a browser deciding for itself
    /// would call a worker dead on a deployment that allows longer gaps.
    pub online: bool,
    /// Builds it is running right now.
    pub active_builds: i32,
    /// Builds it has finished, by outcome. Together these say whether a worker
    /// is doing the job or merely holding a slot: a machine with a bad
    /// toolchain claims work and fails it, which looks identical to a healthy
    /// one until you count.
    pub successful_builds: i32,
    pub failed_builds: i32,
}

/// Where a worker is in the approval workflow.
///
/// A closed set rather than the string the database holds: the frontend
/// switches on it, and an unrecognised status should be resolved once at the
/// edge rather than re-guessed at every use.
///
/// This is the stored type as well as the wire type: the `workers.status`
/// column maps to it directly, so an unknown value fails at the edge instead of
/// spreading as a string that every reader parses for itself.
#[derive(Deserialize, ToSchema, Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(
    feature = "db",
    derive(sea_orm::DeriveActiveEnum, sea_orm::EnumIter),
    sea_orm(rs_type = "String", db_type = "String(StringLen::None)")
)]
pub enum ApprovalStatus {
    /// Enrolled and waiting for an operator. Cannot build.
    #[cfg_attr(feature = "db", sea_orm(string_value = "pending"))]
    Pending,
    /// Approved: holds a signed certificate and may take jobs.
    #[cfg_attr(feature = "db", sea_orm(string_value = "approved"))]
    Approved,
    /// Refused. Its certificate no longer works, and its reservations and
    /// in-flight builds have been released.
    #[cfg_attr(feature = "db", sea_orm(string_value = "revoked"))]
    Revoked,
}

impl ApprovalStatus {
    /// The word this status is stored and sent as.
    ///
    /// One place, so the sea-orm `string_value` attributes and the serde
    /// `rename_all` cannot drift from it unnoticed — the test below holds all
    /// three together.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Revoked => "revoked",
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

impl std::fmt::Display for ApprovalStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::ApprovalStatus;

    /// The wire form, the stored form and `as_str` are the same three words
    /// the column has always held, so existing rows keep their meaning after
    /// the type change — and the three spellings cannot drift apart.
    ///
    /// The sea-orm `string_value` attributes are the fourth, and are checked
    /// by round-tripping a worker through the database in `worker_store`.
    #[test]
    fn every_spelling_of_a_status_agrees() {
        for (status, word) in [
            (ApprovalStatus::Pending, "pending"),
            (ApprovalStatus::Approved, "approved"),
            (ApprovalStatus::Revoked, "revoked"),
        ] {
            assert_eq!(status.as_str(), word);
            assert_eq!(status.to_string(), word);
            assert_eq!(
                serde_json::to_string(&status).unwrap(),
                format!("\"{word}\"")
            );
        }
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
