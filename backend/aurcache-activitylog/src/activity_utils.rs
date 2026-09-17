use crate::activity_serializer::ActivitySerializer;
use crate::events::Event;
use crate::failure_activity::{
    PublishFailedActivity, VersionCheckFailedActivity, WorkerReapedActivity,
    WorkerSettingRejectedActivity,
};
use crate::package_add_activity::PackageAddActivity;
use crate::package_delete_activity::PackageDeleteActivity;
use crate::package_update_activity::PackageUpdateActivity;
use crate::server_start_activity::ServerStartActivity;
use crate::worker_activity::{WorkerApproveActivity, WorkerEnrollActivity, WorkerRevokeActivity};
use anyhow::anyhow;
use aurcache_db::activities;
use aurcache_db::activities::ActivityType;
use aurcache_db::prelude::{Activities, LogEntities};
use aurcache_db::{log_entities, logs};
use sea_orm::ActiveValue::Set;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, DatabaseConnection, EntityTrait, Order,
    PaginatorTrait, QueryFilter, QueryOrder, QuerySelect,
};
use serde::Serialize;

// Defined in aurcache-common so the HTTP client and the browser frontend use
// the same struct rather than a hand-mirrored copy.
pub use aurcache_common::api::activity::{Activity, ActivityPage, Severity};
use aurcache_common::api::log::EntityRef;

/// What to narrow the log to.
///
/// Both are "show me less"; neither set means the whole log.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LogFilter {
    /// Show this severity and worse. [`Severity::Info`] is everything, which is
    /// the same as no filter.
    pub severity: Option<Severity>,
    /// Only what happened since the server last started.
    pub since_boot: bool,
}

/// One entry on its way to the database.
///
/// Rendered and timestamped where it was recorded, not where it is written: a
/// backlog must not misdate what it is holding.
#[derive(Debug, Clone)]
struct Record {
    activity_type: ActivityType,
    data: String,
    user: Option<String>,
    timestamp: i64,
}

/// One structured entry on its way to the database, with the index rows it
/// implies already worked out.
///
/// The references are read from the same serialized payload that gets stored,
/// at the moment of emission, so the entry and its index cannot describe
/// different things.
#[derive(Debug, Clone)]
struct LogRecord {
    kind: &'static str,
    severity: Severity,
    message: String,
    data: String,
    scope: Option<EntityRef>,
    user: Option<String>,
    timestamp: i64,
    references: Vec<(String, EntityRef)>,
}

/// What the queue carries.
///
/// Two shapes while the curated activity log and the structured log are
/// separate tables; porting the activity events to `LogEvent` collapses them.
#[derive(Debug, Clone)]
enum Queued {
    Activity(Record),
    Log(Box<LogRecord>),
}

/// Where structured events appear in the journal.
///
/// One target for all of them rather than the emitting module, so a deployment
/// can turn the whole stream up or down in `LOG_LEVEL` without naming every
/// crate. The kind rides along as a field, and says more than a module path
/// would.
pub const EVENT_TARGET: &str = "aurcache::event";

/// The role an entry's own scope is indexed under.
///
/// A payload key, so it sits in the same namespace as the roles read out of the
/// payload; no event declares a field called this.
pub const SCOPE_ROLE: &str = "scope";

/// How many entries may be waiting to be written.
///
/// Generous: the log takes a few hundred entries on a busy day, so reaching
/// this means the database is not answering, which is a problem the log is not
/// going to fix by holding on.
const QUEUE: usize = 1024;

/// A handle for recording activity, which knows nothing about the database.
///
/// Recording is what callers do everywhere -- in a publish, in a heartbeat, in
/// a scheduler pass -- and none of those places should be writing rows or
/// deciding what to do when a write fails. So [`Self::record`] is synchronous
/// and infallible, and one task ([`ActivityStore`]) owns the connection and the
/// error handling.
///
/// Cheap to clone; every clone feeds the same writer.
#[derive(Debug, Clone)]
pub struct ActivityLog {
    tx: tokio::sync::mpsc::Sender<Queued>,
    /// What everything recorded through this handle happened *under*.
    ///
    /// Set by [`Self::scoped`], so a build's own handle files everything it
    /// emits against that build without each call site repeating it.
    scope: Option<EntityRef>,
}

impl ActivityLog {
    /// Record an event.
    ///
    /// Does not block, does not fail, does not await. Every caller is
    /// describing something that *already happened*: an approval that went
    /// through and was not logged is a gap in the record, while one reported as
    /// failed after the fact is a lie about the instance. So there is nothing
    /// useful for a caller to do about a write that did not land, and nothing
    /// is asked of them.
    pub fn record<T: Serialize + ActivitySerializer>(
        &self,
        activity: T,
        activity_type: ActivityType,
        user: Option<String>,
    ) {
        let data = match serde_json::to_string(&activity) {
            Ok(data) => data,
            Err(e) => {
                tracing::warn!("could not render an activity entry: {e}");
                return;
            }
        };
        let record = Record {
            activity_type,
            data,
            user,
            timestamp: aurcache_db::helpers::time::now_secs(),
        };
        self.send(Queued::Activity(record));
    }

    /// Record a structured event.
    ///
    /// Like [`Self::record`]: synchronous, infallible, and never blocking.
    pub fn emit(&self, event: impl Into<Event>) {
        self.emit_by(event, None);
    }

    /// Record a structured event somebody asked for.
    ///
    /// The actor matters for anything a person set in motion -- a dependency
    /// repointed through the API is not the same entry as the scheduler doing
    /// it -- and is `None` for everything the server does on its own.
    pub fn emit_by(&self, event: impl Into<Event>, user: Option<String>) {
        let event = event.into();
        let rendered = match crate::event::render(&event) {
            Ok(rendered) => rendered,
            Err(e) => {
                tracing::warn!("could not render a {} log event: {e}", event.kind());
                return;
            }
        };
        // The journal keeps its line, so `docker logs` and `journalctl` show
        // what they always did and a call site formats the sentence once. The
        // target is fixed rather than the emitting module, with the kind as a
        // field: `deps.replaced` says more about what happened than
        // `aurcache_api::package` does, and it is what a filter would rather
        // match on.
        match rendered.severity {
            Severity::Info => {
                tracing::info!(target: EVENT_TARGET, kind = rendered.kind, "{}", rendered.message);
            }
            Severity::Warning => {
                tracing::warn!(target: EVENT_TARGET, kind = rendered.kind, "{}", rendered.message);
            }
            Severity::Error => {
                tracing::error!(target: EVENT_TARGET, kind = rendered.kind, "{}", rendered.message);
            }
        }

        self.send(Queued::Log(Box::new(LogRecord {
            kind: rendered.kind,
            severity: rendered.severity,
            message: rendered.message,
            data: rendered.payload.to_string(),
            scope: self.scope.clone(),
            user,
            timestamp: aurcache_db::helpers::time::now_secs(),
            references: rendered.references,
        })));
    }

    /// A handle whose entries are all filed under one entity.
    ///
    /// `log.scoped(BuildRef { .. })` makes everything emitted through it part of
    /// that build's story, so one query returns both the entries *about* a build
    /// and those written *during* it. Scopes do not nest: the innermost handle
    /// wins, which is what a caller holding a build's handle means by it.
    #[must_use]
    pub fn scoped(&self, entity: impl Into<EntityRef>) -> Self {
        Self {
            tx: self.tx.clone(),
            scope: Some(entity.into()),
        }
    }

    fn send(&self, queued: Queued) {
        // Dropped rather than waited on: a log must never be the reason the
        // thing it is recording got slower. A full queue means the database is
        // not answering, and the journal still has the line this sits beside.
        if self.tx.try_send(queued).is_err() {
            tracing::warn!("activity log queue is full or closed; an entry was dropped");
        }
    }
}

impl ActivityLog {
    /// A handle with nothing on the other end, whose entries go nowhere.
    ///
    /// For tests, and for anything run outside a server process. Named for what
    /// it does so it cannot be reached for by accident: a deployment wiring
    /// this in would have a log that silently stayed empty.
    #[must_use]
    pub fn discarding() -> Self {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        drop(rx);
        Self { tx, scope: None }
    }
}

/// Timestamps older than this are pruned. Saturating: the clock is trusted
/// here, and a far-future `now` must prune everything rather than wrap to
/// keeping it all. Shared with the structured log store, whose prune is the
/// same line over a different table.
pub(crate) fn prune_cutoff(now: i64, keep_secs: u64) -> i64 {
    now.saturating_sub(i64::try_from(keep_secs).unwrap_or(i64::MAX))
}

/// Start the one task that writes the log, and hand back a handle to it.
///
/// The task ends when the last handle is dropped, draining what is queued
/// first.
#[must_use]
pub fn spawn(db: DatabaseConnection) -> (ActivityLog, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Queued>(QUEUE);
    let store = ActivityStore::new(db);
    let writer = tokio::spawn(async move {
        while let Some(queued) = rx.recv().await {
            match queued {
                Queued::Activity(record) => store.write(record).await,
                Queued::Log(record) => store.write_log(*record).await,
            }
        }
    });
    (ActivityLog { tx, scope: None }, writer)
}

/// The log as the database holds it: reading it, and the one place that writes
/// it.
#[derive(Debug, Clone)]
pub struct ActivityStore {
    db: DatabaseConnection,
}

impl ActivityStore {
    #[must_use]
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    /// Record straight to the database, skipping the queue.
    ///
    /// Test-only: the queue is what production writes through, and a test that
    /// reads back what it just wrote would otherwise be racing the writer. The
    /// queue has a test of its own.
    #[cfg(test)]
    pub(crate) async fn write_now<T: Serialize + ActivitySerializer>(
        &self,
        activity: T,
        activity_type: ActivityType,
        user: Option<String>,
    ) {
        self.write(Record {
            activity_type,
            data: serde_json::to_string(&activity).expect("a test fixture serializes"),
            user,
            timestamp: aurcache_db::helpers::time::now_secs(),
        })
        .await;
    }

    /// Write one structured entry and the index of what it refers to.
    ///
    /// Both in one transaction: an entry that exists but is not indexed would
    /// be invisible to every entity filter, which is worse than not having been
    /// written at all, because nothing would show it was missing.
    async fn write_log(&self, record: LogRecord) {
        if let Err(e) = self.try_write_log(record).await {
            tracing::warn!("could not write to the log: {e}");
        }
    }

    async fn try_write_log(&self, record: LogRecord) -> anyhow::Result<()> {
        use sea_orm::TransactionTrait;

        // The scope is indexed like any other reference, under a role of its
        // own, so one query answers both "about this build" and "during it".
        let mut refs: Vec<(String, EntityRef)> = record.references;
        if let Some(scope) = &record.scope {
            refs.push((SCOPE_ROLE.to_string(), scope.clone()));
        }
        // A role may legitimately name one entity twice -- a list with a
        // repeat -- and the index keys on the three together.
        refs.sort();
        refs.dedup();

        let txn = self.db.begin().await?;
        let entry = logs::ActiveModel {
            kind: Set(record.kind.to_string()),
            severity: Set(record.severity),
            message: Set(record.message),
            data: Set(record.data),
            scope: Set(record.scope.map(|scope| scope.to_string())),
            timestamp: Set(record.timestamp),
            user: Set(record.user),
            ..Default::default()
        }
        .insert(&txn)
        .await?;

        if !refs.is_empty() {
            let rows = refs
                .into_iter()
                .map(|(role, entity)| log_entities::ActiveModel {
                    log_id: Set(entry.id),
                    role: Set(role),
                    ns: Set(entity.namespace().to_string()),
                    id: Set(entity.id()),
                });
            LogEntities::insert_many(rows).exec(&txn).await?;
        }

        txn.commit().await?;
        Ok(())
    }

    /// Write one entry, reporting a failure here rather than at the call site.
    async fn write(&self, record: Record) {
        let row = activities::ActiveModel {
            timestamp: Set(record.timestamp),
            data: Set(record.data),
            user: Set(record.user),
            typ: Set(record.activity_type),
            ..Default::default()
        };
        if let Err(e) = row.save(&self.db).await {
            tracing::warn!("could not write to the activity log: {e}");
        }
    }

    /// One page of the log, newest first, with how long the filtered log is.
    ///
    /// Filtered and paged in the database rather than by fetching everything
    /// and slicing, which is what every other list in this app does. The log is
    /// the one list that cannot be fetched whole -- it only grows -- and a
    /// client-side filter over one page would only ever search the page you
    /// were already looking at.
    ///
    /// `total` counts what the filter matched, not the whole table, or the
    /// pager would promise pages that are not there.
    pub async fn page(
        &self,
        limit: u64,
        offset: u64,
        filter: &LogFilter,
    ) -> anyhow::Result<ActivityPage> {
        let condition = self.condition(filter).await?;
        let total = Activities::find()
            .filter(condition.clone())
            .count(&self.db)
            .await?;
        let entries = self.list_where(condition, Some(limit), offset).await?;
        Ok(ActivityPage { entries, total })
    }

    /// The `where` clause for a filter.
    ///
    /// Severity is a property of the entry's *kind*, so filtering by it is
    /// filtering by the set of kinds at or above it -- which is why there is no
    /// severity column to keep in step with anything.
    async fn condition(&self, filter: &LogFilter) -> anyhow::Result<Condition> {
        let mut condition = Condition::all();
        if let Some(floor) = filter.severity
            && floor != Severity::Info
        {
            condition = condition.add(activities::Column::Typ.is_in(ActivityType::at_least(floor)));
        }
        if filter.since_boot
            && let Some(booted) = self.last_start().await?
        {
            condition = condition.add(activities::Column::Timestamp.gte(booted));
        }
        Ok(condition)
    }

    /// When this instance last started, as the log records it.
    ///
    /// `None` on an instance that has not restarted since server-start entries
    /// existed, in which case "this boot" cannot mean anything and the filter
    /// does not narrow -- showing everything beats showing nothing.
    async fn last_start(&self) -> anyhow::Result<Option<i64>> {
        // Only the timestamp: the row also carries the entry's data JSON.
        Ok(Activities::find()
            .select_only()
            .column(activities::Column::Timestamp)
            .filter(activities::Column::Typ.eq(ActivityType::ServerStart))
            .order_by(activities::Column::Timestamp, Order::Desc)
            .order_by(activities::Column::Id, Order::Desc)
            .into_tuple::<(i64,)>()
            .one(&self.db)
            .await?
            .map(|(timestamp,)| timestamp))
    }

    /// Delete entries older than `keep_secs`, returning how many went.
    ///
    /// The log is the one table nothing has ever pruned, and it only grows --
    /// faster now that the server records its own restarts and what it notices
    /// going wrong. By age rather than by row count because that is how people
    /// think about a log: "the last three months", not "the last fifty
    /// thousand things".
    ///
    /// `0` keeps everything, for a deployment that would rather the log be
    /// complete than bounded.
    pub async fn prune(&self, keep_secs: u64, now: i64) -> anyhow::Result<u64> {
        if keep_secs == 0 {
            return Ok(0);
        }
        let cutoff = prune_cutoff(now, keep_secs);
        let deleted = Activities::delete_many()
            .filter(activities::Column::Timestamp.lt(cutoff))
            .exec(&self.db)
            .await?;
        Ok(deleted.rows_affected)
    }

    pub async fn list(&self, limit: Option<u64>, offset: u64) -> anyhow::Result<Vec<Activity>> {
        self.list_where(Condition::all(), limit, offset).await
    }

    async fn list_where(
        &self,
        condition: Condition,
        limit: Option<u64>,
        offset: u64,
    ) -> anyhow::Result<Vec<Activity>> {
        let activities = Activities::find()
            .filter(condition)
            .order_by(activities::Column::Timestamp, Order::Desc)
            // Ties broken by id, so a page boundary between two entries written
            // in the same second does not drop or repeat one: the log records
            // seconds, and a bulk add writes several within one.
            .order_by(activities::Column::Id, Order::Desc)
            .limit(limit)
            .offset(offset)
            .all(&self.db)
            .await?;

        Ok(activities
            .into_iter()
            .filter_map(|activity| {
                match Self::deserialize_type(activity.typ, &activity.data) {
                    Ok(serializer) => Some(Activity {
                        timestamp: activity.timestamp,
                        text: serializer.format(),
                        user: activity.user,
                        severity: activity.typ.severity(),
                        subject: serializer.subject(),
                    }),
                    Err(e) => {
                        // A row we cannot render is skipped rather than failing the whole listing.
                        tracing::warn!("Skipping unreadable activity row: {e}");
                        None
                    }
                }
            })
            .collect())
    }

    fn deserialize_type(
        activity_type: ActivityType,
        data: &str,
    ) -> anyhow::Result<Box<dyn ActivitySerializer>> {
        Ok(match activity_type {
            ActivityType::AddPackage => Box::new(serde_json::from_str::<PackageAddActivity>(data)?),
            ActivityType::RemovePackage => {
                Box::new(serde_json::from_str::<PackageDeleteActivity>(data)?)
            }
            ActivityType::UpdatePackage => {
                Box::new(serde_json::from_str::<PackageUpdateActivity>(data)?)
            }
            ActivityType::ServerStart => {
                Box::new(serde_json::from_str::<ServerStartActivity>(data)?)
            }
            ActivityType::WorkerEnroll => {
                Box::new(serde_json::from_str::<WorkerEnrollActivity>(data)?)
            }
            ActivityType::WorkerApprove => {
                Box::new(serde_json::from_str::<WorkerApproveActivity>(data)?)
            }
            ActivityType::WorkerRevoke => {
                Box::new(serde_json::from_str::<WorkerRevokeActivity>(data)?)
            }
            ActivityType::PublishFailed => {
                Box::new(serde_json::from_str::<PublishFailedActivity>(data)?)
            }
            ActivityType::WorkerReaped => {
                Box::new(serde_json::from_str::<WorkerReapedActivity>(data)?)
            }
            ActivityType::VersionCheckFailed => {
                Box::new(serde_json::from_str::<VersionCheckFailedActivity>(data)?)
            }
            ActivityType::WorkerSettingRejected => {
                Box::new(serde_json::from_str::<WorkerSettingRejectedActivity>(data)?)
            }
            // Nothing writes these types yet; render them as unreadable instead of panicking.
            ActivityType::StartBuild | ActivityType::FinishBuild => {
                return Err(anyhow!("Unsupported activity type: {activity_type:?}"));
            }
        })
    }
}

#[cfg(test)]
pub(crate) fn now_for_test() -> i64 {
    aurcache_db::helpers::time::now_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package_add_activity::PackageAddActivity;
    use aurcache_db::migration::Migrator;
    use sea_orm::Database;
    use sea_orm_migration::MigratorTrait;

    async fn log() -> ActivityStore {
        ActivityStore::new(memory_db().await)
    }

    async fn memory_db() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        db
    }

    async fn add(log: &ActivityStore, kind: ActivityType) {
        // The payload only has to deserialize as *something* the renderer
        // knows; these tests are about which rows come back, not their prose.
        let activity = PackageAddActivity {
            package: "hello".to_string(),
        };
        match kind {
            ActivityType::ServerStart => {
                log.write_now(
                    crate::server_start_activity::ServerStartActivity {
                        version: "0.1.0".to_string(),
                    },
                    kind,
                    None,
                )
                .await;
            }
            ActivityType::PublishFailed => {
                log.write_now(
                    PublishFailedActivity {
                        package: "hello".to_string(),
                        build: 1,
                        reason: "disk full".to_string(),
                    },
                    kind,
                    None,
                )
                .await;
            }
            ActivityType::WorkerReaped => {
                log.write_now(
                    WorkerReapedActivity {
                        retried: vec![1],
                        failed: vec![],
                    },
                    kind,
                    None,
                )
                .await;
            }
            _ => log.write_now(activity, kind, None).await,
        }
    }

    /// No filter is the whole log.
    #[tokio::test]
    async fn an_empty_filter_narrows_nothing() {
        let log = log().await;
        add(&log, ActivityType::AddPackage).await;
        add(&log, ActivityType::PublishFailed).await;

        let page = log.page(50, 0, &LogFilter::default()).await.unwrap();
        assert_eq!(page.total, 2);
        assert_eq!(page.entries.len(), 2);
    }

    /// A severity filter means "this and worse", and the total has to count
    /// what the filter matched -- a pager told the size of the whole table
    /// would offer pages that are not there.
    #[tokio::test]
    async fn severity_narrows_and_the_total_follows() {
        let log = log().await;
        add(&log, ActivityType::AddPackage).await;
        add(&log, ActivityType::UpdatePackage).await;
        add(&log, ActivityType::WorkerReaped).await;
        add(&log, ActivityType::PublishFailed).await;

        let warnings = log
            .page(
                50,
                0,
                &LogFilter {
                    severity: Some(Severity::Warning),
                    since_boot: false,
                },
            )
            .await
            .unwrap();
        assert_eq!(warnings.total, 2);
        assert_eq!(warnings.entries.len(), 2);
        assert!(
            warnings
                .entries
                .iter()
                .all(|e| e.severity >= Severity::Warning)
        );

        let errors = log
            .page(
                50,
                0,
                &LogFilter {
                    severity: Some(Severity::Error),
                    since_boot: false,
                },
            )
            .await
            .unwrap();
        assert_eq!(errors.total, 1);
        assert_eq!(errors.entries[0].severity, Severity::Error);
    }

    /// `Info` is every severity there is, so it must not be treated as a filter
    /// that excludes anything.
    #[tokio::test]
    async fn the_lowest_severity_is_the_whole_log() {
        let log = log().await;
        add(&log, ActivityType::AddPackage).await;
        add(&log, ActivityType::PublishFailed).await;

        let page = log
            .page(
                50,
                0,
                &LogFilter {
                    severity: Some(Severity::Info),
                    since_boot: false,
                },
            )
            .await
            .unwrap();
        assert_eq!(page.total, 2);
    }

    /// "This boot" counts back to the newest server-start entry, and includes
    /// it: the restart is the first thing that happened this boot.
    #[tokio::test]
    async fn since_boot_cuts_at_the_last_start() {
        let log = log().await;
        add(&log, ActivityType::AddPackage).await;
        add(&log, ActivityType::ServerStart).await;
        add(&log, ActivityType::UpdatePackage).await;

        let page = log
            .page(
                50,
                0,
                &LogFilter {
                    severity: None,
                    since_boot: true,
                },
            )
            .await
            .unwrap();
        // The entries share a timestamp (the clock has whole-second
        // resolution), so this asserts the boundary is inclusive rather than
        // counting rows: nothing before the marker may be excluded by being
        // written in the same second as it.
        assert!(page.total >= 2, "{:?}", page.entries);
        assert!(
            page.entries.iter().any(|e| e.text.contains("started")),
            "the marker itself belongs to the boot it starts: {:?}",
            page.entries
        );
    }

    /// An instance that has not restarted since server-start entries existed
    /// has no marker to count back to. Showing everything beats showing
    /// nothing.
    #[tokio::test]
    async fn since_boot_without_a_marker_shows_everything() {
        let log = log().await;
        add(&log, ActivityType::AddPackage).await;
        add(&log, ActivityType::UpdatePackage).await;

        let page = log
            .page(
                50,
                0,
                &LogFilter {
                    severity: None,
                    since_boot: true,
                },
            )
            .await
            .unwrap();
        assert_eq!(page.total, 2);
    }

    /// What a caller actually does: record, and carry on. The entry reaches the
    /// database without the caller ever seeing a connection, an await or an
    /// error.
    #[tokio::test]
    async fn recording_reaches_the_database_through_the_queue() {
        let db = memory_db().await;
        let (log, writer) = spawn(db.clone());

        log.record(
            PackageAddActivity {
                package: "hello".to_string(),
            },
            ActivityType::AddPackage,
            Some("alice".to_string()),
        );

        // The writer ends once the last handle is gone, draining first -- which
        // is also how the process shuts down without losing what it queued.
        drop(log);
        writer.await.unwrap();

        let page = ActivityStore::new(db)
            .page(50, 0, &LogFilter::default())
            .await
            .unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.entries[0].text, "added package hello");
        assert_eq!(page.entries[0].user.as_deref(), Some("alice"));
    }

    /// Pruning takes what has aged out and leaves the rest. The log is read
    /// newest-first, so what it keeps is what anyone was going to look at.
    #[tokio::test]
    async fn pruning_keeps_what_is_still_within_the_window() {
        let log = log().await;
        add(&log, ActivityType::AddPackage).await;
        add(&log, ActivityType::UpdatePackage).await;

        // Nothing is older than the window yet.
        let now = crate::activity_utils::now_for_test();
        assert_eq!(log.prune(3600, now).await.unwrap(), 0);
        assert_eq!(
            log.page(50, 0, &LogFilter::default()).await.unwrap().total,
            2
        );

        // An hour later they both have.
        assert_eq!(log.prune(3600, now + 7200).await.unwrap(), 2);
        assert_eq!(
            log.page(50, 0, &LogFilter::default()).await.unwrap().total,
            0
        );
    }

    /// Zero keeps everything, for a deployment that would rather the log were
    /// complete than bounded.
    #[tokio::test]
    async fn a_retention_of_zero_deletes_nothing() {
        let log = log().await;
        add(&log, ActivityType::AddPackage).await;
        assert_eq!(log.prune(0, i64::MAX).await.unwrap(), 0);
        assert_eq!(
            log.page(50, 0, &LogFilter::default()).await.unwrap().total,
            1
        );
    }

    /// Paging has to be stable across entries written in the same second, which
    /// is the ordinary case for a bulk add.
    #[tokio::test]
    async fn pages_do_not_drop_or_repeat_an_entry() {
        let log = log().await;
        for _ in 0..5 {
            add(&log, ActivityType::AddPackage).await;
        }

        let first = log.page(2, 0, &LogFilter::default()).await.unwrap();
        let second = log.page(2, 2, &LogFilter::default()).await.unwrap();
        let third = log.page(2, 4, &LogFilter::default()).await.unwrap();
        assert_eq!(first.total, 5);
        assert_eq!(
            first.entries.len() + second.entries.len() + third.entries.len(),
            5
        );
    }
}
