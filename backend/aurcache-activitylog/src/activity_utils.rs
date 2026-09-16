use crate::activity_serializer::ActivitySerializer;
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
use aurcache_db::prelude::Activities;
use sea_orm::ActiveValue::Set;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, DatabaseConnection, EntityTrait, Order,
    PaginatorTrait, QueryFilter, QueryOrder, QuerySelect,
};
use serde::Serialize;

// Defined in aurcache-common so the HTTP client and the browser frontend use
// the same struct rather than a hand-mirrored copy.
pub use aurcache_common::api::activity::{Activity, ActivityPage, Severity};

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

#[derive(Debug, Clone)]
pub struct ActivityLog {
    db: DatabaseConnection,
}

impl ActivityLog {
    #[must_use]
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    /// Record an event, saying so in the journal rather than failing when the
    /// log cannot be written.
    ///
    /// Every caller is describing something that already happened: an approval
    /// that went through and was not logged is a gap in the record, while one
    /// reported as failed after the fact is a lie about the instance. So the
    /// write is best effort, and the only thing a caller could do with the
    /// error is what this does.
    pub async fn record<T: Serialize + ActivitySerializer>(
        &self,
        activity: T,
        activity_type: ActivityType,
        user: Option<String>,
    ) {
        if let Err(e) = self.add(activity, activity_type, user).await {
            tracing::warn!("could not write to the activity log: {e}");
        }
    }

    pub async fn add<T: Serialize + ActivitySerializer>(
        &self,
        activity: T,
        activity_type: ActivityType,
        user: Option<String>,
    ) -> anyhow::Result<()> {
        let activity = serde_json::to_string(&activity)?;
        let timestamp = aurcache_db::helpers::time::now_secs();

        activities::ActiveModel {
            timestamp: Set(timestamp),
            data: Set(activity),
            user: Set(user),
            typ: Set(activity_type),
            ..Default::default()
        }
        .save(&self.db)
        .await?;
        Ok(())
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
        Ok(Activities::find()
            .filter(activities::Column::Typ.eq(ActivityType::ServerStart))
            .order_by(activities::Column::Timestamp, Order::Desc)
            .order_by(activities::Column::Id, Order::Desc)
            .one(&self.db)
            .await?
            .map(|row| row.timestamp))
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
        let cutoff = now.saturating_sub(i64::try_from(keep_secs).unwrap_or(i64::MAX));
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

    async fn log() -> ActivityLog {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        ActivityLog::new(db)
    }

    async fn add(log: &ActivityLog, kind: ActivityType) {
        // The payload only has to deserialize as *something* the renderer
        // knows; these tests are about which rows come back, not their prose.
        let activity = PackageAddActivity {
            package: "hello".to_string(),
        };
        match kind {
            ActivityType::ServerStart => {
                log.add(
                    crate::server_start_activity::ServerStartActivity {
                        version: "0.1.0".to_string(),
                    },
                    kind,
                    None,
                )
                .await
                .unwrap();
            }
            ActivityType::PublishFailed => {
                log.add(
                    PublishFailedActivity {
                        package: "hello".to_string(),
                        build: 1,
                        reason: "disk full".to_string(),
                    },
                    kind,
                    None,
                )
                .await
                .unwrap();
            }
            ActivityType::WorkerReaped => {
                log.add(
                    WorkerReapedActivity {
                        retried: vec![1],
                        failed: vec![],
                    },
                    kind,
                    None,
                )
                .await
                .unwrap();
            }
            _ => log.add(activity, kind, None).await.unwrap(),
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
