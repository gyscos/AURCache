//! Database helpers for the remote-worker job lifecycle: atomic claim, lease
//! renewal via heartbeat, and requeue/fail of dropped builds.

use crate::helpers::time::now_secs;
use crate::prelude::{Builds, Packages, Workers};
use crate::{builds, packages, workers};
use aurcache_common::api::worker::ApprovalStatus;
use aurcache_common::builder::BuildStates;
use sea_orm::{
    ColumnTrait, ConnectionTrait, DbErr, EntityTrait, FromQueryResult, QueryFilter, QuerySelect,
};

use std::collections::{HashMap, HashSet};

// Build states, as the integers the `builds.status` column holds. Derived from
// the `BuildState` enum rather than written out: these were four literals that
// happened to agree with it, and nothing would have noticed if they stopped.
pub const STATUS_ACTIVE: i32 = BuildStates::ACTIVE_BUILD;
pub const STATUS_SUCCESS: i32 = BuildStates::SUCCESSFUL_BUILD;
pub const STATUS_FAILED: i32 = BuildStates::FAILED_BUILD;
pub const STATUS_ENQUEUED: i32 = BuildStates::ENQUEUED_BUILD;
pub const STATUS_WAITING_FOR_DEPS: i32 = BuildStates::WAITING_FOR_DEPS;

/// One approved worker's routing-relevant configuration, plus its live state.
#[derive(Debug)]
struct WorkerCap {
    id: i32,
    name: String,
    priority: i32,
    concurrency: i32,
    native: Vec<String>,
    emulated: Vec<String>,
    last_seen: Option<i64>,
    /// Builds this worker currently holds in the ACTIVE state.
    active: i32,
}

/// A snapshot of the approved fleet, taken once per claim.
///
/// Deliberately *not* a long-lived cache. The fleet splits into a stable half
/// (affinity, priority, concurrency, arches — changes only on
/// re-register/approve/revoke) and a volatile half (`last_seen`, active build
/// counts — changes on every heartbeat and every claim). [`Fleet::available`]
/// needs the volatile half, so a round trip happens on every claim regardless;
/// caching the stable half separately would save nothing while adding
/// invalidation hooks, each a chance to route a job to a worker that cannot
/// build it.
#[derive(Debug, Default)]
struct Fleet {
    workers: Vec<WorkerCap>,
    /// Arches at least one approved worker builds *natively*; these are
    /// reserved from emulated claims.
    native_arches: HashSet<String>,
    /// pkgbase -> the approved workers that named it. Because affinity entries
    /// are exact names, this is a direct inversion of the worker rows: one pass,
    /// no matching, and no dependency on which builds are queued.
    affinity: HashMap<String, HashSet<i32>>,
}

/// The `workers` columns the router actually needs, as a named struct rather
/// than a wide tuple.
#[derive(Debug, FromQueryResult)]
struct WorkerRow {
    id: i32,
    name: String,
    priority: i32,
    concurrency: i32,
    native_arches: String,
    emulated_arches: String,
    package_affinity: String,
    last_seen: Option<i64>,
}

/// Split a stored comma-separated list column into its entries.
fn split_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .collect()
}

impl Fleet {
    /// Load every approved worker plus its current ACTIVE build count.
    async fn load<C: ConnectionTrait>(db: &C) -> Result<Self, DbErr> {
        let rows: Vec<WorkerRow> = Workers::find()
            .select_only()
            .column(workers::Column::Id)
            .column(workers::Column::Name)
            .column(workers::Column::Priority)
            .column(workers::Column::Concurrency)
            .column(workers::Column::NativeArches)
            .column(workers::Column::EmulatedArches)
            .column(workers::Column::PackageAffinity)
            .column(workers::Column::LastSeen)
            .filter(workers::Column::Status.eq(ApprovalStatus::Approved))
            .into_model()
            .all(db)
            .await?;

        let counts: Vec<(Option<i32>, i64)> = Builds::find()
            .select_only()
            .column(builds::Column::WorkerId)
            .column_as(builds::Column::Id.count(), "cnt")
            .filter(builds::Column::Status.eq(STATUS_ACTIVE))
            .group_by(builds::Column::WorkerId)
            .into_tuple()
            .all(db)
            .await?;
        let active: HashMap<i32, i32> = counts
            .into_iter()
            .filter_map(|(id, n)| id.map(|id| (id, i32::try_from(n).unwrap_or(i32::MAX))))
            .collect();

        let mut fleet = Self::default();
        for row in rows {
            let native = split_list(&row.native_arches);
            fleet.native_arches.extend(native.iter().cloned());
            for pkg in split_list(&row.package_affinity) {
                fleet.affinity.entry(pkg).or_default().insert(row.id);
            }
            fleet.workers.push(WorkerCap {
                id: row.id,
                name: row.name,
                priority: row.priority,
                concurrency: row.concurrency,
                native,
                emulated: split_list(&row.emulated_arches),
                last_seen: row.last_seen,
                active: active.get(&row.id).copied().unwrap_or(0),
            });
        }
        Ok(fleet)
    }

    /// Names of the approved workers that declared affinity for a package,
    /// sorted so the explanation is stable between requests.
    fn affinity_holders(&self, pkg: &str) -> Vec<String> {
        let Some(ids) = self.affinity.get(pkg) else {
            return Vec::new();
        };
        let mut names: Vec<String> = self
            .workers
            .iter()
            .filter(|w| ids.contains(&w.id))
            .map(|w| w.name.clone())
            .collect();
        names.sort();
        names
    }

    fn worker(&self, id: i32) -> Option<&WorkerCap> {
        self.workers.iter().find(|w| w.id == id)
    }

    /// True when some approved worker has claimed affinity for this package, in
    /// which case only those workers may build it.
    fn reserved(&self, pkg: &str) -> bool {
        self.affinity.get(pkg).is_some_and(|ws| !ws.is_empty())
    }

    fn affine(&self, worker_id: i32, pkg: &str) -> bool {
        self.affinity
            .get(pkg)
            .is_some_and(|ws| ws.contains(&worker_id))
    }

    /// Native arches first; an emulated arch is only claimable when *no*
    /// approved worker builds it natively, so foreign-arch work stays reserved
    /// for the native worker rather than crawling through emulation elsewhere.
    fn arch_ok(&self, w: &WorkerCap, platform: &str) -> bool {
        w.native.iter().any(|a| a == platform)
            || (w.emulated.iter().any(|a| a == platform) && !self.native_arches.contains(platform))
    }

    /// Whether this worker may build this job at all — a hard filter combining
    /// architecture routing with package affinity.
    fn capable(&self, w: &WorkerCap, platform: &str, pkg: &str) -> bool {
        self.arch_ok(w, platform) && (!self.reserved(pkg) || self.affine(w.id, pkg))
    }

    /// Whether this worker could pick a job up *right now*: recently seen and
    /// not already at its concurrency limit.
    fn available(&self, w: &WorkerCap, now: i64, liveness_timeout: i64) -> bool {
        w.last_seen.is_some_and(|t| t >= now - liveness_timeout) && w.active < w.concurrency
    }

    /// Whether a strictly higher-priority worker could take this job right now.
    ///
    /// Only *strictly* higher blocks, so equal-priority workers never hold each
    /// other back and an all-default fleet never blocks at all.
    fn blocked(
        &self,
        me: &WorkerCap,
        platform: &str,
        pkg: &str,
        now: i64,
        liveness_timeout: i64,
    ) -> bool {
        self.workers.iter().any(|w| {
            w.id != me.id
                && w.priority > me.priority
                && self.capable(w, platform, pkg)
                && self.available(w, now, liveness_timeout)
        })
    }
}

/// Atomically claim the next buildable job for `worker_id`.
///
/// Routing has three layers (see `design/worker-routing.md`):
///
/// 1. **Affinity** (hard) — a package named by any approved worker may only be
///    built by workers that name it. Ignores liveness: handing an affine job to
///    a worker without the credential fails, where waiting merely waits.
/// 2. **Architecture** (hard) — native arches first, foreign arches reserved for
///    a worker that builds them natively.
/// 3. **Priority** (soft) — a worker declines a job while a strictly
///    higher-priority worker is live and under its concurrency limit, until the
///    job has waited `spill_delay_secs`.
///
/// Arches, affinity and priority all come from the worker's stored row rather
/// than from the request, because each worker's decision depends on what *other*
/// workers declared: they must be read from one consistent source.
///
/// The final transition is a conditional `UPDATE ... WHERE status = ENQUEUED`,
/// so two workers racing for the same build see exactly one `rows_affected == 1`.
pub async fn claim_job<C: ConnectionTrait>(
    db: &C,
    worker_id: i32,
    lease_ttl_secs: i64,
    spill_delay_secs: i64,
    liveness_timeout_secs: i64,
) -> Result<Option<builds::Model>, DbErr> {
    let fleet = Fleet::load(db).await?;
    // Not approved (or vanished mid-request): nothing is claimable.
    let Some(me) = fleet.worker(worker_id) else {
        return Ok(None);
    };

    let candidates: Vec<builds::Model> = Builds::find()
        .filter(builds::Column::Status.eq(STATUS_ENQUEUED))
        .all(db)
        .await?;
    if candidates.is_empty() {
        return Ok(None);
    }

    // Resolve pkgbase names for the queued builds in one query.
    let pkg_ids: Vec<i32> = candidates.iter().map(|b| b.pkg_id).collect();
    let names: HashMap<i32, String> = Packages::find()
        .select_only()
        .column(packages::Column::Id)
        .column(packages::Column::Name)
        .filter(packages::Column::Id.is_in(pkg_ids))
        .into_tuple::<(i32, String)>()
        .all(db)
        .await?
        .into_iter()
        .collect();

    let now = now_secs();
    let mut ranked: Vec<(bool, bool, i64, i32)> = Vec::new();
    for build in &candidates {
        let Some(pkg) = names.get(&build.pkg_id) else {
            continue;
        };
        let platform = build.platform.as_str();
        if !fleet.capable(me, platform, pkg) {
            continue;
        }
        let age = now - build.start_time.unwrap_or(now);
        if age < spill_delay_secs && fleet.blocked(me, platform, pkg, now, liveness_timeout_secs) {
            continue;
        }
        // Keys are negated because `false < true`: affine jobs first, then
        // native ones, then oldest. A worker that *is* affine for a package
        // should take that job ahead of one anybody could have taken, since it
        // may be the only worker that can.
        ranked.push((
            !fleet.affine(me.id, pkg),
            !me.native.iter().any(|a| a == platform),
            build.start_time.unwrap_or(0),
            build.id,
        ));
    }
    ranked.sort_unstable();

    for (_, _, _, id) in ranked {
        let res = Builds::update_many()
            .col_expr(builds::Column::Status, STATUS_ACTIVE.into())
            .col_expr(builds::Column::WorkerId, worker_id.into())
            .col_expr(
                builds::Column::LeaseExpiresAt,
                (now + lease_ttl_secs).into(),
            )
            .col_expr(builds::Column::StartTime, now.into())
            .filter(builds::Column::Id.eq(id))
            .filter(builds::Column::Status.eq(STATUS_ENQUEUED))
            .exec(db)
            .await?;
        if res.rows_affected == 1 {
            // The package row keeps its own copy of the status, and that is
            // what the packages list shows. Without this it goes on saying
            // "enqueued" -- or "waiting for deps" -- for the whole build,
            // while the builds list beside it says "building": the same fact
            // reported two ways, one of them wrong. `worker_complete` writes
            // the terminal status at the other end of the build; this is the
            // start of it.
            //
            // Only after the claim above has actually won. That update is the
            // compare-and-swap deciding which worker gets the build, so doing
            // this first would announce a build that another worker took.
            if let Some(pkg_id) = candidates.iter().find(|b| b.id == id).map(|b| b.pkg_id) {
                Packages::update_many()
                    .col_expr(packages::Column::Status, STATUS_ACTIVE.into())
                    .filter(packages::Column::Id.eq(pkg_id))
                    .exec(db)
                    .await?;
            }
            return Builds::find_by_id(id).one(db).await;
        }
    }
    Ok(None)
}

/// Result of processing a heartbeat: builds that were reconciled away from the
/// worker because it stopped reporting them while still alive.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct HeartbeatOutcome {
    /// Ids whose lease was renewed.
    pub renewed: Vec<i32>,
    /// Ids that were owned + active but absent from the report; requeued.
    pub dropped: Vec<i32>,
}

/// Renew leases for the builds a worker reports as active, and reconcile any
/// build still owned + active by this worker that it *didn't* report (the
/// worker silently dropped it) by requeueing it immediately.
pub async fn heartbeat<C: ConnectionTrait>(
    db: &C,
    worker_id: i32,
    active_build_ids: &[i32],
    lease_ttl_secs: i64,
    max_attempts: i32,
) -> Result<HeartbeatOutcome, DbErr> {
    let now = now_secs();
    let mut outcome = HeartbeatOutcome::default();

    // Every build this worker currently owns in the ACTIVE state.
    let owned: Vec<builds::Model> = Builds::find()
        .filter(builds::Column::WorkerId.eq(worker_id))
        .filter(builds::Column::Status.eq(STATUS_ACTIVE))
        .all(db)
        .await?;

    for build in owned {
        let id = build.id;
        if active_build_ids.contains(&id) {
            Builds::update_many()
                .col_expr(
                    builds::Column::LeaseExpiresAt,
                    (now + lease_ttl_secs).into(),
                )
                .filter(builds::Column::Id.eq(id))
                .filter(builds::Column::WorkerId.eq(worker_id))
                .filter(builds::Column::Status.eq(STATUS_ACTIVE))
                .exec(db)
                .await?;
            outcome.renewed.push(id);
        } else if requeue_or_fail(db, &build, max_attempts).await? != RequeueOutcome::Unchanged {
            outcome.dropped.push(id);
        }
    }
    Ok(outcome)
}

/// What a [`requeue_or_fail`] attempt actually did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequeueOutcome {
    /// `ACTIVE -> ENQUEUED`: the build gets another attempt.
    Requeued,
    /// `ACTIVE -> FAILED`: the `max_attempts` budget was exhausted.
    Failed,
    /// The row moved on between the caller's read and this write (the worker
    /// completed it, renewed its lease, or another pass already reclaimed it).
    /// Nothing was written.
    Unchanged,
}

/// Requeue a build after its worker was lost, or fail it once the retry budget
/// is exhausted. Transitions `ACTIVE -> ENQUEUED` (clearing the lease/owner and
/// bumping `attempt_count`), or `-> FAILED` after `max_attempts` requeues.
///
/// The write is an optimistic compare-and-swap pinned to the exact row revision
/// the caller observed (`status`, `worker_id`, `lease_expires_at`,
/// `attempt_count`), mirroring the CAS guards in `record_built_version` /
/// `complete_success`. Callers necessarily select a snapshot of candidates and
/// act on it afterwards; without this pin, a build that completed in that gap
/// would be forced back to `ENQUEUED` after its package status was already
/// updated and its dependents promoted. Any concurrent change instead yields
/// [`RequeueOutcome::Unchanged`] and the row is left alone.
pub async fn requeue_or_fail<C: ConnectionTrait>(
    db: &C,
    observed: &builds::Model,
    max_attempts: i32,
) -> Result<RequeueOutcome, DbErr> {
    let requeue = observed.attempt_count < max_attempts;

    let mut update = Builds::update_many()
        .col_expr(builds::Column::WorkerId, Option::<i32>::None.into())
        .col_expr(builds::Column::LeaseExpiresAt, Option::<i64>::None.into());
    update = if requeue {
        update
            .col_expr(builds::Column::Status, STATUS_ENQUEUED.into())
            .col_expr(
                builds::Column::AttemptCount,
                (observed.attempt_count + 1).into(),
            )
    } else {
        update
            .col_expr(builds::Column::Status, STATUS_FAILED.into())
            .col_expr(builds::Column::EndTime, Some(now_secs()).into())
    };

    let res = update
        .filter(builds::Column::Id.eq(observed.id))
        .filter(builds::Column::Status.eq(STATUS_ACTIVE))
        .filter(match observed.worker_id {
            Some(w) => builds::Column::WorkerId.eq(w),
            None => builds::Column::WorkerId.is_null(),
        })
        .filter(match observed.lease_expires_at {
            Some(l) => builds::Column::LeaseExpiresAt.eq(l),
            None => builds::Column::LeaseExpiresAt.is_null(),
        })
        .filter(builds::Column::AttemptCount.eq(observed.attempt_count))
        .exec(db)
        .await?;

    if res.rows_affected == 0 {
        return Ok(RequeueOutcome::Unchanged);
    }
    Ok(if requeue {
        RequeueOutcome::Requeued
    } else {
        RequeueOutcome::Failed
    })
}

/// Why an `ENQUEUED` build is not being picked up by anyone.
///
/// Only ever set for builds that *no approved worker can currently take*.
/// Ordinary queueing behind a busy worker is not a reason and yields `None`;
/// otherwise every queued build would carry a scary-looking explanation.
/// Re-exported from `aurcache-common`, where it lives so the API and a
/// frontend can share it.
pub use aurcache_common::api::waiting::WaitingReason;

/// Explain every `ENQUEUED` build that no approved worker can currently claim.
///
/// Without this an affinity-reserved build that is stalled because its worker is
/// offline looks exactly like a build waiting behind a busy queue — which is the
/// single most likely way package affinity wastes someone's afternoon.
pub async fn waiting_reasons<C: ConnectionTrait>(
    db: &C,
    liveness_timeout_secs: i64,
) -> Result<HashMap<i32, WaitingReason>, DbErr> {
    let queued: Vec<builds::Model> = Builds::find()
        .filter(builds::Column::Status.eq(STATUS_ENQUEUED))
        .all(db)
        .await?;
    if queued.is_empty() {
        return Ok(HashMap::new());
    }

    let fleet = Fleet::load(db).await?;
    let names: HashMap<i32, String> = Packages::find()
        .select_only()
        .column(packages::Column::Id)
        .column(packages::Column::Name)
        .filter(packages::Column::Id.is_in(queued.iter().map(|b| b.pkg_id)))
        .into_tuple::<(i32, String)>()
        .all(db)
        .await?
        .into_iter()
        .collect();

    let now = now_secs();
    let mut out = HashMap::new();
    for build in &queued {
        let Some(pkg) = names.get(&build.pkg_id) else {
            continue;
        };
        let platform = build.platform.as_str();
        let capable: Vec<&WorkerCap> = fleet
            .workers
            .iter()
            .filter(|w| fleet.capable(w, platform, pkg))
            .collect();

        let reason = if capable.is_empty() {
            if fleet.reserved(pkg) {
                // Reserved, and nothing that can take it: name the holders so an
                // operator knows which machine to bring back or revoke.
                WaitingReason::Affinity {
                    workers: fleet.affinity_holders(pkg),
                }
            } else {
                WaitingReason::Arch {
                    arch: platform.to_string(),
                }
            }
        } else if capable.iter().any(|w| {
            w.last_seen
                .is_some_and(|t| t >= now - liveness_timeout_secs)
        }) {
            // Someone capable is alive; this is just a queue.
            continue;
        } else if fleet.reserved(pkg) {
            WaitingReason::Affinity {
                workers: fleet.affinity_holders(pkg),
            }
        } else {
            WaitingReason::Offline
        };
        out.insert(build.id, reason);
    }
    Ok(out)
}

/// Requeue every build a worker still holds in the `ACTIVE` state.
///
/// Called when a worker is revoked: from that moment the auth guard refuses it,
/// so it can never report those builds complete. Without this they would sit
/// `ACTIVE` until the lease reaper noticed, up to `LEASE_TTL`. Returns the ids
/// that actually moved.
pub async fn requeue_worker_builds<C: ConnectionTrait>(
    db: &C,
    worker_id: i32,
    max_attempts: i32,
) -> Result<Vec<i32>, DbErr> {
    let owned: Vec<builds::Model> = Builds::find()
        .filter(builds::Column::WorkerId.eq(worker_id))
        .filter(builds::Column::Status.eq(STATUS_ACTIVE))
        .all(db)
        .await?;

    let mut moved = Vec::new();
    for build in owned {
        // Same optimistic CAS as the reaper, so a build that completed between
        // the select above and this write is left alone.
        if requeue_or_fail(db, &build, max_attempts).await? != RequeueOutcome::Unchanged {
            moved.push(build.id);
        }
    }
    Ok(moved)
}

/// Result of a reaper pass: which owned+active builds were requeued vs. given up.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReapOutcome {
    /// Builds re-enqueued for another attempt (budget remaining).
    pub requeued: Vec<i32>,
    /// Builds terminally failed (budget exhausted or backstop timeout).
    pub failed: Vec<i32>,
}

/// Reaper pass: reclaim `ACTIVE` builds whose owning worker went silent, and
/// backstop builds that have run implausibly long.
///
/// A build is reaped when either:
/// * its lease has expired (`lease_expires_at < now`, or `NULL` — no live
///   lease) — the worker missed enough heartbeats that we treat it as lost; or
/// * it has been `ACTIVE` past the backstop deadline
///   (`start_time + max_build_age < now`) even if a lease still appears fresh —
///   covers a hung build whose worker keeps heartbeating.
///
/// Each reaped build goes through [`requeue_or_fail`], so the `attempt_count`
/// budget bounds retries before a terminal `FAILED`.
///
/// `now` and `max_build_age` (`MAX_BUILD_DURATION + grace`, in seconds) are
/// passed in so callers stay testable and can source them from settings.
pub async fn reap_expired_builds<C: ConnectionTrait>(
    db: &C,
    now: i64,
    max_attempts: i32,
    max_build_age: i64,
) -> Result<ReapOutcome, DbErr> {
    let backstop_before = now - max_build_age;
    let candidates: Vec<builds::Model> = Builds::find()
        .filter(builds::Column::Status.eq(STATUS_ACTIVE))
        .filter(
            sea_orm::Condition::any()
                .add(builds::Column::LeaseExpiresAt.lt(now))
                .add(builds::Column::LeaseExpiresAt.is_null())
                .add(builds::Column::StartTime.lt(backstop_before)),
        )
        .all(db)
        .await?;

    let mut outcome = ReapOutcome::default();
    for build in candidates {
        // `requeue_or_fail` pins the CAS to the revision observed here, so a
        // build a concurrent heartbeat renewed or a completion resolved in the
        // gap since the select above is skipped rather than clobbered.
        match requeue_or_fail(db, &build, max_attempts).await? {
            RequeueOutcome::Requeued => outcome.requeued.push(build.id),
            RequeueOutcome::Failed => outcome.failed.push(build.id),
            RequeueOutcome::Unchanged => {}
        }
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::Migrator;
    use sea_orm::{Database, DatabaseConnection};
    use sea_orm_migration::MigratorTrait;

    const LEASE: i64 = 60;
    const SPILL: i64 = 60;
    const LIVENESS: i64 = 60;

    async fn setup() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        db
    }

    /// An approved worker row. Defaults are a live, idle, native-x86_64 worker
    /// with no affinity and no priority — i.e. one that neither reserves
    /// anything nor blocks anyone.
    struct W {
        id: i32,
        native: &'static str,
        emulated: &'static str,
        affinity: &'static str,
        priority: i32,
        concurrency: i32,
        status: ApprovalStatus,
        /// `None` means "never seen", which reads as not live.
        last_seen: Option<i64>,
    }

    impl Default for W {
        fn default() -> Self {
            Self {
                id: 1,
                native: "x86_64",
                emulated: "",
                affinity: "",
                priority: 0,
                concurrency: 1,
                status: ApprovalStatus::Approved,
                last_seen: Some(now_secs()),
            }
        }
    }

    async fn worker(db: &DatabaseConnection, w: W) {
        let last_seen = w
            .last_seen
            .map_or_else(|| "NULL".to_string(), |t| t.to_string());
        db.execute_unprepared(&format!(
            "INSERT INTO workers (id, name, status, cert_fingerprint, native_arches, \
             emulated_arches, package_affinity, priority, concurrency, last_seen) \
             VALUES ({}, 'w{}', '{}', 'fp{}', '{}', '{}', '{}', {}, {}, {last_seen})",
            w.id, w.id, w.status, w.id, w.native, w.emulated, w.affinity, w.priority, w.concurrency
        ))
        .await
        .unwrap();
    }

    /// Enqueue a build of package `name` at `start` (epoch seconds).
    async fn enqueue_pkg(db: &DatabaseConnection, id: i32, name: &str, platform: &str, start: i64) {
        // A partial unique index forbids two pending builds for the same
        // (pkg_id, platform); give every build its own package.
        db.execute_unprepared(&format!(
            "INSERT INTO packages (id, name) VALUES ({id}, '{name}')"
        ))
        .await
        .unwrap();
        db.execute_unprepared(&format!(
            "INSERT INTO builds (id, pkg_id, status, start_time, platform, version, attempt_count) \
             VALUES ({id}, {id}, {STATUS_ENQUEUED}, {start}, '{platform}', '1.0', 0)"
        ))
        .await
        .unwrap();
    }

    async fn enqueue(db: &DatabaseConnection, id: i32, platform: &str, start: i64) {
        enqueue_pkg(db, id, &format!("p{id}"), platform, start).await;
    }

    /// A package's status, without loading the rest of the row: the fixtures
    /// here insert only `(id, name)`, so the other columns are NULL and
    /// decoding a full model fails on them.
    async fn pkg_status(db: &DatabaseConnection, id: i32) -> Option<i32> {
        Packages::find_by_id(id)
            .select_only()
            .column(packages::Column::Status)
            .into_tuple::<Option<i32>>()
            .one(db)
            .await
            .unwrap()
            .unwrap()
    }

    async fn claim(db: &DatabaseConnection, worker_id: i32) -> Option<builds::Model> {
        claim_job(db, worker_id, LEASE, SPILL, LIVENESS)
            .await
            .unwrap()
    }

    /// The package's own status has to follow the build's, because that is the
    /// one the packages list renders. It used to be written on enqueue and
    /// again on completion, with nothing in between -- so for the whole
    /// duration of a build the package claimed to be waiting for it to start.
    #[tokio::test]
    async fn claiming_a_build_marks_its_package_as_building() {
        let db = setup().await;
        worker(&db, W::default()).await;
        enqueue(&db, 10, "x86_64", 100).await;
        db.execute_unprepared(&format!(
            "UPDATE packages SET status = {STATUS_ENQUEUED} WHERE id = 10"
        ))
        .await
        .unwrap();

        let claimed = claim(&db, 1).await.unwrap();
        assert_eq!(claimed.status, Some(STATUS_ACTIVE));

        assert_eq!(
            pkg_status(&db, 10).await,
            Some(STATUS_ACTIVE),
            "the package still reports the status it had before the build started"
        );
    }

    /// Only the package whose build was actually claimed. A claim moves one
    /// build; announcing every queued package as building would be a different
    /// wrong answer.
    #[tokio::test]
    async fn claiming_leaves_other_packages_alone() {
        let db = setup().await;
        worker(&db, W::default()).await;
        enqueue(&db, 10, "x86_64", 100).await;
        enqueue(&db, 11, "x86_64", 200).await;
        db.execute_unprepared(&format!(
            "UPDATE packages SET status = {STATUS_ENQUEUED} WHERE id IN (10, 11)"
        ))
        .await
        .unwrap();

        // The older job is the one that gets taken.
        let claimed = claim(&db, 1).await.unwrap();
        assert_eq!(claimed.pkg_id, 10);

        assert_eq!(pkg_status(&db, 11).await, Some(STATUS_ENQUEUED));
    }

    // ---------------------------------------------------------------- arches

    #[tokio::test]
    async fn claims_oldest_matching_arch() {
        let db = setup().await;
        worker(&db, W::default()).await;
        enqueue(&db, 10, "x86_64", 200).await;
        enqueue(&db, 11, "x86_64", 100).await;
        enqueue(&db, 12, "aarch64", 50).await;

        let claimed = claim(&db, 1).await.unwrap();
        // Oldest x86_64 job wins; the older aarch64 job is not buildable.
        assert_eq!(claimed.id, 11);
        assert_eq!(claimed.status, Some(STATUS_ACTIVE));
        assert_eq!(claimed.worker_id, Some(1));
        assert!(claimed.lease_expires_at.is_some());
    }

    #[tokio::test]
    async fn claim_is_atomic_no_double_handout() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 1,
                ..W::default()
            },
        )
        .await;
        worker(
            &db,
            W {
                id: 2,
                ..W::default()
            },
        )
        .await;
        enqueue(&db, 20, "x86_64", 100).await;

        assert!(claim(&db, 1).await.is_some());
        assert!(claim(&db, 2).await.is_none());
    }

    #[tokio::test]
    async fn unknown_or_unapproved_worker_claims_nothing() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 1,
                status: ApprovalStatus::Revoked,
                ..W::default()
            },
        )
        .await;
        enqueue(&db, 21, "x86_64", 100).await;

        assert!(claim(&db, 1).await.is_none(), "revoked worker claimed");
        assert!(claim(&db, 99).await.is_none(), "unknown worker claimed");
    }

    #[tokio::test]
    async fn native_preferred_over_emulated() {
        let db = setup().await;
        worker(
            &db,
            W {
                emulated: "aarch64",
                ..W::default()
            },
        )
        .await;
        enqueue(&db, 30, "aarch64", 50).await; // older, emulatable
        enqueue(&db, 31, "x86_64", 100).await; // newer, native

        // No native aarch64 worker exists, so it *may* emulate — but native
        // work comes first.
        assert_eq!(claim(&db, 1).await.unwrap().id, 31);
    }

    #[tokio::test]
    async fn foreign_arch_reserved_for_native_worker() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 1,
                emulated: "aarch64",
                ..W::default()
            },
        )
        .await;
        worker(
            &db,
            W {
                id: 2,
                native: "aarch64",
                ..W::default()
            },
        )
        .await;
        enqueue(&db, 40, "aarch64", 50).await;

        // The emulating worker must not grab the reserved job...
        assert!(claim(&db, 1).await.is_none());
        // ...the native one does.
        assert_eq!(claim(&db, 2).await.unwrap().id, 40);
    }

    #[tokio::test]
    async fn emulated_claim_when_no_native_worker() {
        let db = setup().await;
        worker(
            &db,
            W {
                emulated: "armv7h",
                ..W::default()
            },
        )
        .await;
        enqueue(&db, 50, "armv7h", 50).await;

        assert_eq!(claim(&db, 1).await.unwrap().id, 50);
    }

    // -------------------------------------------------------------- affinity

    #[tokio::test]
    async fn affinity_reserves_package_to_declaring_worker() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 1,
                ..W::default()
            },
        )
        .await;
        worker(
            &db,
            W {
                id: 2,
                affinity: "unreal-engine",
                ..W::default()
            },
        )
        .await;
        enqueue_pkg(&db, 60, "unreal-engine", "x86_64", 100).await;

        assert!(claim(&db, 1).await.is_none(), "non-affine worker claimed");
        assert_eq!(claim(&db, 2).await.unwrap().id, 60);
    }

    #[tokio::test]
    async fn package_nobody_claims_is_unaffected_by_affinity_lists() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 1,
                ..W::default()
            },
        )
        .await;
        worker(
            &db,
            W {
                id: 2,
                affinity: "unreal-engine",
                ..W::default()
            },
        )
        .await;
        enqueue_pkg(&db, 61, "hello", "x86_64", 100).await;

        assert_eq!(claim(&db, 1).await.unwrap().id, 61);
    }

    /// Exact matching only: no wildcards, and no accidental prefix reservation.
    #[tokio::test]
    async fn affinity_matches_package_names_exactly() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 1,
                ..W::default()
            },
        )
        .await;
        worker(
            &db,
            W {
                id: 2,
                affinity: "unreal-engine",
                ..W::default()
            },
        )
        .await;
        enqueue_pkg(&db, 62, "unreal-engine-bin", "x86_64", 100).await;
        enqueue_pkg(&db, 63, "unreal", "x86_64", 200).await;

        // Neither neighbouring name is reserved.
        assert_eq!(claim(&db, 1).await.unwrap().id, 62);
        assert_eq!(claim(&db, 1).await.unwrap().id, 63);
    }

    /// An affine worker should take its affine job ahead of an older job that
    /// anybody could have taken — it may be the only worker that can.
    #[tokio::test]
    async fn affine_worker_prefers_its_affine_job() {
        let db = setup().await;
        worker(
            &db,
            W {
                affinity: "unreal-engine",
                ..W::default()
            },
        )
        .await;
        enqueue_pkg(&db, 70, "hello", "x86_64", 100).await; // older, general
        enqueue_pkg(&db, 71, "unreal-engine", "x86_64", 200).await; // newer, affine

        assert_eq!(claim(&db, 1).await.unwrap().id, 71);
    }

    /// Liveness is deliberately not part of the affinity rule: handing the job
    /// elsewhere fails, where waiting merely waits.
    #[tokio::test]
    async fn offline_approved_affine_worker_still_reserves() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 1,
                ..W::default()
            },
        )
        .await;
        worker(
            &db,
            W {
                id: 2,
                affinity: "unreal-engine",
                last_seen: Some(now_secs() - 86_400),
                ..W::default()
            },
        )
        .await;
        enqueue_pkg(&db, 72, "unreal-engine", "x86_64", 100).await;

        assert!(claim(&db, 1).await.is_none());
    }

    /// Revoking is the documented escape hatch for a reservation held by a
    /// machine that is never coming back.
    #[tokio::test]
    async fn revoked_affine_worker_does_not_reserve() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 1,
                ..W::default()
            },
        )
        .await;
        worker(
            &db,
            W {
                id: 2,
                affinity: "unreal-engine",
                status: ApprovalStatus::Revoked,
                ..W::default()
            },
        )
        .await;
        enqueue_pkg(&db, 73, "unreal-engine", "x86_64", 100).await;

        assert_eq!(claim(&db, 1).await.unwrap().id, 73);
    }

    #[tokio::test]
    async fn affinity_and_arch_reservation_compose() {
        let db = setup().await;
        // Affine but wrong arch.
        worker(
            &db,
            W {
                id: 1,
                native: "x86_64",
                affinity: "unreal-engine",
                ..W::default()
            },
        )
        .await;
        // Right arch but not affine.
        worker(
            &db,
            W {
                id: 2,
                native: "aarch64",
                ..W::default()
            },
        )
        .await;
        // Both.
        worker(
            &db,
            W {
                id: 3,
                native: "aarch64",
                affinity: "unreal-engine",
                ..W::default()
            },
        )
        .await;
        enqueue_pkg(&db, 74, "unreal-engine", "aarch64", 100).await;

        assert!(claim(&db, 1).await.is_none(), "wrong arch claimed");
        assert!(claim(&db, 2).await.is_none(), "non-affine claimed");
        assert_eq!(claim(&db, 3).await.unwrap().id, 74);
    }

    // -------------------------------------------------------------- priority

    /// A fresh job, and a live idle faster worker exists: hold back.
    #[tokio::test]
    async fn lower_priority_worker_is_blocked_while_faster_one_is_available() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 1,
                priority: 10,
                ..W::default()
            },
        )
        .await;
        worker(
            &db,
            W {
                id: 2,
                ..W::default()
            },
        )
        .await;
        enqueue(&db, 80, "x86_64", now_secs()).await;

        assert!(claim(&db, 2).await.is_none(), "fallback jumped the queue");
        assert_eq!(claim(&db, 1).await.unwrap().id, 80);
    }

    /// Saturated fast worker: the fallback takes over immediately, with no
    /// delay — the case a fixed hold-back would have penalised.
    #[tokio::test]
    async fn spills_immediately_when_faster_worker_is_full() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 1,
                priority: 10,
                concurrency: 1,
                ..W::default()
            },
        )
        .await;
        worker(
            &db,
            W {
                id: 2,
                ..W::default()
            },
        )
        .await;
        enqueue(&db, 81, "x86_64", now_secs()).await;
        enqueue(&db, 82, "x86_64", now_secs()).await;

        // Worker 1 fills its single slot...
        assert!(claim(&db, 1).await.is_some());
        // ...so worker 2 is free to take the other job right away.
        assert!(claim(&db, 2).await.is_some());
    }

    #[tokio::test]
    async fn spills_immediately_when_faster_worker_is_stale() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 1,
                priority: 10,
                last_seen: Some(now_secs() - 3600),
                ..W::default()
            },
        )
        .await;
        worker(
            &db,
            W {
                id: 2,
                ..W::default()
            },
        )
        .await;
        enqueue(&db, 83, "x86_64", now_secs()).await;

        assert_eq!(claim(&db, 2).await.unwrap().id, 83);
    }

    /// The backstop: a faster worker that looks healthy but never claims must
    /// not stall a job forever.
    #[tokio::test]
    async fn spills_once_the_job_is_older_than_the_spill_delay() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 1,
                priority: 10,
                ..W::default()
            },
        )
        .await;
        worker(
            &db,
            W {
                id: 2,
                ..W::default()
            },
        )
        .await;
        enqueue(&db, 84, "x86_64", now_secs() - SPILL - 1).await;

        assert_eq!(claim(&db, 2).await.unwrap().id, 84);
    }

    #[tokio::test]
    async fn equal_priority_workers_do_not_block_each_other() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 1,
                ..W::default()
            },
        )
        .await;
        worker(
            &db,
            W {
                id: 2,
                ..W::default()
            },
        )
        .await;
        enqueue(&db, 85, "x86_64", now_secs()).await;

        assert_eq!(claim(&db, 2).await.unwrap().id, 85);
    }

    /// A faster worker that cannot build the job at all must not hold it back.
    #[tokio::test]
    async fn incapable_faster_worker_does_not_block() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 1,
                native: "aarch64",
                priority: 10,
                ..W::default()
            },
        )
        .await;
        worker(
            &db,
            W {
                id: 2,
                ..W::default()
            },
        )
        .await;
        enqueue(&db, 86, "x86_64", now_secs()).await;

        assert_eq!(claim(&db, 2).await.unwrap().id, 86);
    }

    /// Priority must not override affinity: a faster worker without the
    /// affinity cannot take the job, and must not block the affine one either.
    #[tokio::test]
    async fn priority_does_not_override_affinity() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 1,
                priority: 10,
                ..W::default()
            },
        )
        .await;
        worker(
            &db,
            W {
                id: 2,
                affinity: "unreal-engine",
                ..W::default()
            },
        )
        .await;
        enqueue_pkg(&db, 87, "unreal-engine", "x86_64", now_secs()).await;

        assert!(claim(&db, 1).await.is_none());
        assert_eq!(claim(&db, 2).await.unwrap().id, 87);
    }

    // -------------------------------------------------- waiting reasons

    #[tokio::test]
    async fn no_reason_when_a_live_worker_could_take_the_job() {
        let db = setup().await;
        worker(&db, W::default()).await;
        enqueue(&db, 100, "x86_64", now_secs()).await;

        // Ordinary queueing is not a "reason" — otherwise every queued build
        // would carry an alarming explanation.
        assert!(waiting_reasons(&db, LIVENESS).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn explains_a_build_reserved_to_an_offline_worker() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 1,
                ..W::default()
            },
        )
        .await;
        worker(
            &db,
            W {
                id: 2,
                affinity: "unreal-engine",
                last_seen: Some(now_secs() - 86_400),
                ..W::default()
            },
        )
        .await;
        enqueue_pkg(&db, 101, "unreal-engine", "x86_64", now_secs()).await;

        let reasons = waiting_reasons(&db, LIVENESS).await.unwrap();
        assert_eq!(
            reasons.get(&101),
            Some(&WaitingReason::Affinity {
                workers: vec!["w2".to_string()]
            })
        );
    }

    /// Closes a pre-existing blind spot: a foreign-arch build with no worker
    /// for that arch stalls silently today.
    #[tokio::test]
    async fn explains_an_arch_no_worker_can_build() {
        let db = setup().await;
        worker(&db, W::default()).await;
        enqueue(&db, 102, "aarch64", now_secs()).await;

        let reasons = waiting_reasons(&db, LIVENESS).await.unwrap();
        assert_eq!(
            reasons.get(&102),
            Some(&WaitingReason::Arch {
                arch: "aarch64".to_string()
            })
        );
    }

    #[tokio::test]
    async fn explains_a_fleet_that_is_entirely_offline() {
        let db = setup().await;
        worker(
            &db,
            W {
                last_seen: Some(now_secs() - 86_400),
                ..W::default()
            },
        )
        .await;
        enqueue(&db, 103, "x86_64", now_secs()).await;

        let reasons = waiting_reasons(&db, LIVENESS).await.unwrap();
        assert_eq!(reasons.get(&103), Some(&WaitingReason::Offline));
    }

    // ------------------------------------------------- leases and reaping

    #[tokio::test]
    async fn heartbeat_renews_reported_and_requeues_dropped() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 5,
                ..W::default()
            },
        )
        .await;
        enqueue(&db, 30, "x86_64", 100).await;
        enqueue(&db, 31, "x86_64", 100).await;
        claim_job(&db, 5, 60, SPILL, LIVENESS).await.unwrap();
        claim_job(&db, 5, 60, SPILL, LIVENESS).await.unwrap();

        // Report only build 30 as active -> 31 was dropped and should requeue.
        let out = heartbeat(&db, 5, &[30], 60, 3).await.unwrap();
        assert_eq!(out.renewed, vec![30]);
        assert_eq!(out.dropped, vec![31]);

        let b31 = Builds::find_by_id(31).one(&db).await.unwrap().unwrap();
        assert_eq!(b31.status, Some(STATUS_ENQUEUED));
        assert_eq!(b31.worker_id, None);
        assert_eq!(b31.attempt_count, 1);
    }

    #[tokio::test]
    async fn requeue_budget_eventually_fails() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 9,
                ..W::default()
            },
        )
        .await;
        enqueue(&db, 40, "x86_64", 100).await;
        claim_job(&db, 9, 60, SPILL, LIVENESS).await.unwrap();

        // attempts 0 -> requeue (count 1), claim again, etc.
        for expected in [
            RequeueOutcome::Requeued,
            RequeueOutcome::Requeued,
            RequeueOutcome::Requeued,
            RequeueOutcome::Failed,
        ] {
            // ensure it's active + owned before requeue
            claim_job(&db, 9, 60, SPILL, LIVENESS).await.unwrap();
            let observed = Builds::find_by_id(40).one(&db).await.unwrap().unwrap();
            let outcome = requeue_or_fail(&db, &observed, 3).await.unwrap();
            assert_eq!(outcome, expected);
        }
        let b = Builds::find_by_id(40).one(&db).await.unwrap().unwrap();
        assert_eq!(b.status, Some(STATUS_FAILED));
    }

    #[tokio::test]
    async fn reaper_requeues_expired_lease_only() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 3,
                ..W::default()
            },
        )
        .await;
        enqueue(&db, 60, "x86_64", 100).await;
        enqueue(&db, 61, "x86_64", 100).await;
        // Claim both with a 60s lease so lease_expires_at = claim_now + 60.
        claim_job(&db, 3, 60, SPILL, LIVENESS).await.unwrap();
        claim_job(&db, 3, 60, SPILL, LIVENESS).await.unwrap();

        // "now" far in the future: both leases are expired -> both requeued.
        let far = now_secs() + 10_000;
        let out = reap_expired_builds(&db, far, 3, 100_000).await.unwrap();
        assert_eq!(out.requeued.len(), 2);
        assert!(out.failed.is_empty());
        for id in [60, 61] {
            let b = Builds::find_by_id(id).one(&db).await.unwrap().unwrap();
            assert_eq!(b.status, Some(STATUS_ENQUEUED));
            assert_eq!(b.worker_id, None);
            assert_eq!(b.attempt_count, 1);
        }
    }

    /// A build that completes between the reaper's candidate select and its
    /// per-row write must not be dragged back out of its terminal state.
    #[tokio::test]
    async fn requeue_skips_build_resolved_after_observation() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 7,
                ..W::default()
            },
        )
        .await;
        enqueue(&db, 90, "x86_64", 100).await;
        claim_job(&db, 7, 60, SPILL, LIVENESS).await.unwrap();

        // What the reaper would have seen in its candidate select.
        let observed = Builds::find_by_id(90).one(&db).await.unwrap().unwrap();

        // ...meanwhile the owning worker completes the build.
        Builds::update_many()
            .col_expr(builds::Column::Status, STATUS_SUCCESS.into())
            .col_expr(builds::Column::WorkerId, Option::<i32>::None.into())
            .col_expr(builds::Column::LeaseExpiresAt, Option::<i64>::None.into())
            .filter(builds::Column::Id.eq(90))
            .exec(&db)
            .await
            .unwrap();

        // The stale observation must not clobber the SUCCESS row.
        let outcome = requeue_or_fail(&db, &observed, 3).await.unwrap();
        assert_eq!(outcome, RequeueOutcome::Unchanged);

        let b = Builds::find_by_id(90).one(&db).await.unwrap().unwrap();
        assert_eq!(b.status, Some(STATUS_SUCCESS));
        assert_eq!(b.attempt_count, 0);
    }

    /// Same guard, but for a lease renewed by a heartbeat in the gap.
    #[tokio::test]
    async fn requeue_skips_build_whose_lease_was_renewed() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 8,
                ..W::default()
            },
        )
        .await;
        enqueue(&db, 91, "x86_64", 100).await;
        claim_job(&db, 8, 60, SPILL, LIVENESS).await.unwrap();
        let observed = Builds::find_by_id(91).one(&db).await.unwrap().unwrap();

        // Worker heartbeats: lease pushed out, build still ACTIVE and owned.
        heartbeat(&db, 8, &[91], 600, 3).await.unwrap();

        let outcome = requeue_or_fail(&db, &observed, 3).await.unwrap();
        assert_eq!(outcome, RequeueOutcome::Unchanged);

        let b = Builds::find_by_id(91).one(&db).await.unwrap().unwrap();
        assert_eq!(b.status, Some(STATUS_ACTIVE));
        assert_eq!(b.worker_id, Some(8));
    }

    #[tokio::test]
    async fn reaper_leaves_fresh_leases_alone() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 4,
                ..W::default()
            },
        )
        .await;
        enqueue(&db, 70, "x86_64", 100).await;
        claim_job(&db, 4, 600, SPILL, LIVENESS).await.unwrap();

        // "now" is right after the claim: lease is fresh, build is young.
        let out = reap_expired_builds(&db, now_secs(), 3, 100_000)
            .await
            .unwrap();
        assert!(out.requeued.is_empty());
        assert!(out.failed.is_empty());
        let b = Builds::find_by_id(70).one(&db).await.unwrap().unwrap();
        assert_eq!(b.status, Some(STATUS_ACTIVE));
    }

    #[tokio::test]
    async fn reaper_backstop_fires_despite_fresh_lease() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 6,
                ..W::default()
            },
        )
        .await;
        // start_time = 100 (long ago); claim gives a fresh far-future lease.
        enqueue(&db, 80, "x86_64", 100).await;
        claim_job(&db, 6, 1_000_000, SPILL, LIVENESS).await.unwrap();

        // Backstop: max_build_age small so start_time(=claim now) is "too old"
        // relative to a `now` well past it, even though the lease is fresh.
        let now = now_secs() + 10_000;
        let out = reap_expired_builds(&db, now, 3, 1).await.unwrap();
        assert_eq!(out.requeued, vec![80]);
    }

    #[tokio::test]
    async fn reaper_gives_up_after_budget() {
        let db = setup().await;
        worker(
            &db,
            W {
                id: 2,
                ..W::default()
            },
        )
        .await;
        enqueue(&db, 90, "x86_64", 100).await;
        claim_job(&db, 2, 60, SPILL, LIVENESS).await.unwrap();
        // Pre-exhaust the budget.
        db.execute_unprepared("UPDATE builds SET attempt_count = 3 WHERE id = 90")
            .await
            .unwrap();

        let far = now_secs() + 10_000;
        let out = reap_expired_builds(&db, far, 3, 100_000).await.unwrap();
        assert_eq!(out.failed, vec![90]);
        assert!(out.requeued.is_empty());
        let b = Builds::find_by_id(90).one(&db).await.unwrap().unwrap();
        assert_eq!(b.status, Some(STATUS_FAILED));
    }
}
