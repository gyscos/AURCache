//! Reading the structured log: filtering, paging, and deciding which of the
//! entities an entry names can still be opened.
//!
//! Every query here is ordinary SQL that sea-orm builds the same way for both
//! backends. That is deliberate: nothing in this repository is tested against
//! Postgres, so a query that branched by backend would be one whose *tested*
//! branch is not the branch that runs in production. It is also why the entity
//! filter reads a side table instead of reaching into the payload JSON.
//!
//! See `design/structured-logs.md`.

use aurcache_common::api::activity::Severity;
use aurcache_common::api::log::{EntityRef, LogEntry, LogPage, NS_BUILD, NS_PACKAGE, NS_WORKER};
use aurcache_db::prelude::{Builds, LogEntities, Logs, Packages, Workers};
use aurcache_db::{builds, log_entities, logs, packages, workers};
use sea_orm::{
    ColumnTrait, Condition, DatabaseConnection, EntityTrait, Order, PaginatorTrait, QueryFilter,
    QueryOrder, QuerySelect, QueryTrait,
};
use std::collections::{BTreeMap, BTreeSet};

/// What to narrow the log to. Every field is "show me less".
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LogFilter {
    /// This severity and worse. Stored as an ordered number, so this is one
    /// comparison rather than a list of kinds the query would have to know.
    pub severity: Option<Severity>,
    /// Only what happened since the server last started.
    pub since_boot: bool,
    /// Only this kind of entry.
    pub kind: Option<String>,
    /// Only entries naming this entity, in any role.
    pub entity: Option<EntityRef>,
    /// Narrow [`Self::entity`] to one role: the dependent, rather than any of
    /// the three packages a dependency replacement names.
    pub role: Option<String>,
}

/// Reading the log.
#[derive(Debug, Clone)]
pub struct LogStore {
    db: DatabaseConnection,
}

impl LogStore {
    #[must_use]
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    /// One page, newest first, with how long the filtered log is.
    ///
    /// # Errors
    ///
    /// Returns any database error from the count, the page, or resolving links.
    pub async fn page(
        &self,
        limit: u64,
        offset: u64,
        filter: &LogFilter,
    ) -> anyhow::Result<LogPage> {
        let condition = self.condition(filter).await?;

        // After filtering, or the pager promises pages that are not there.
        let total = Logs::find()
            .filter(condition.clone())
            .count(&self.db)
            .await?;

        let rows = Logs::find()
            .filter(condition)
            .order_by(logs::Column::Timestamp, Order::Desc)
            // Ties broken by id: the log records whole seconds, and a burst
            // writes several within one, so without this a page boundary
            // between them could drop or repeat an entry.
            .order_by(logs::Column::Id, Order::Desc)
            .limit(limit)
            .offset(offset)
            .all(&self.db)
            .await?;

        let hrefs = self.resolve(&rows).await?;
        let entries = rows
            .into_iter()
            .map(|row| {
                let id = row.id;
                render(row, hrefs.get(&id).cloned().unwrap_or_default())
            })
            .collect();

        Ok(LogPage { entries, total })
    }

    /// The `where` clause for a filter.
    async fn condition(&self, filter: &LogFilter) -> anyhow::Result<Condition> {
        let mut condition = Condition::all();

        if let Some(floor) = filter.severity
            && floor != Severity::Info
        {
            condition = condition.add(logs::Column::Severity.gte(floor));
        }

        if filter.since_boot
            && let Some(booted) = self.last_start().await?
        {
            condition = condition.add(logs::Column::Timestamp.gte(booted));
        }

        if let Some(kind) = &filter.kind {
            condition = condition.add(logs::Column::Kind.eq(kind.as_str()));
        }

        if let Some(entity) = &filter.entity {
            // A subquery rather than a join, so the page is still one row per
            // entry when an entity appears under several roles.
            let mut refs = LogEntities::find()
                .select_only()
                .column(log_entities::Column::LogId)
                .filter(log_entities::Column::Ns.eq(entity.namespace()))
                .filter(log_entities::Column::Id.eq(entity.id()));
            if let Some(role) = &filter.role {
                refs = refs.filter(log_entities::Column::Role.eq(role.as_str()));
            }
            condition = condition.add(logs::Column::Id.in_subquery(refs.into_query()));
        }

        Ok(condition)
    }

    /// When this instance last started, as the log records it.
    ///
    /// `None` before anything has recorded a start, in which case "this boot"
    /// cannot mean anything and the filter does not narrow: showing everything
    /// beats showing nothing.
    async fn last_start(&self) -> anyhow::Result<Option<i64>> {
        Ok(Logs::find()
            .filter(logs::Column::Kind.eq(crate::kinds::SERVER_START))
            .order_by(logs::Column::Timestamp, Order::Desc)
            .order_by(logs::Column::Id, Order::Desc)
            .one(&self.db)
            .await?
            .map(|row| row.timestamp))
    }

    /// Which of the entities this page names can still be opened.
    ///
    /// Decided here rather than when an entry was written, because an entry is
    /// often written by the very request that deletes what it names -- a
    /// dependency replacement removes the old package -- so a link baked in at
    /// emission would be wrong within milliseconds.
    ///
    /// One query per namespace for the whole page, not one per reference.
    async fn resolve(
        &self,
        rows: &[logs::Model],
    ) -> anyhow::Result<BTreeMap<i32, BTreeMap<String, Vec<Option<String>>>>> {
        let per_row: Vec<(i32, Vec<(String, EntityRef)>)> = rows
            .iter()
            .map(|row| {
                let payload = serde_json::from_str(&row.data).unwrap_or(serde_json::Value::Null);
                (row.id, crate::event::references(&payload))
            })
            .collect();

        let alive = self
            .alive(
                per_row
                    .iter()
                    .flat_map(|(_, refs)| refs.iter().map(|(_, e)| e)),
            )
            .await?;

        Ok(per_row
            .into_iter()
            .map(|(id, refs)| {
                let mut by_role: BTreeMap<String, Vec<Option<String>>> = BTreeMap::new();
                for (role, entity) in refs {
                    let href = alive.contains(&entity).then(|| route_of(&entity));
                    by_role.entry(role).or_default().push(href);
                }
                (id, by_role)
            })
            .collect())
    }

    /// Which of these references still have something behind them.
    async fn alive<'a>(
        &self,
        refs: impl Iterator<Item = &'a EntityRef>,
    ) -> anyhow::Result<BTreeSet<EntityRef>> {
        let mut packages = BTreeSet::new();
        let mut workers = BTreeSet::new();
        let mut build_bases = BTreeSet::new();
        for entity in refs {
            match entity {
                EntityRef::Package(p) => {
                    packages.insert(p.0.clone());
                }
                EntityRef::Worker(w) => {
                    workers.insert(w.0.clone());
                }
                EntityRef::Build(b) => {
                    build_bases.insert(b.pkgbase.clone());
                }
            }
        }

        let mut alive = BTreeSet::new();

        if !packages.is_empty() {
            for name in Packages::find()
                .select_only()
                .column(packages::Column::Name)
                .filter(packages::Column::Name.is_in(packages))
                .into_tuple::<String>()
                .all(&self.db)
                .await?
            {
                alive.insert(EntityRef::Package(name.into()));
            }
        }

        if !workers.is_empty() {
            for name in Workers::find()
                .select_only()
                .column(workers::Column::Name)
                .filter(workers::Column::Name.is_in(workers))
                .into_tuple::<String>()
                .all(&self.db)
                .await?
            {
                alive.insert(EntityRef::Worker(name.into()));
            }
        }

        // A build is reached through its package, so both have to exist: the
        // page is `/package/<pkgbase>/build/<number>`.
        if !build_bases.is_empty() {
            for (name, number) in Packages::find()
                .select_only()
                .column(packages::Column::Name)
                .column(builds::Column::Number)
                .inner_join(Builds)
                .filter(packages::Column::Name.is_in(build_bases))
                .into_tuple::<(String, i32)>()
                .all(&self.db)
                .await?
            {
                alive.insert(EntityRef::Build(aurcache_common::api::log::BuildRef {
                    pkgbase: name,
                    number,
                }));
            }
        }

        Ok(alive)
    }

    /// Delete entries older than `keep_secs`, returning how many went.
    ///
    /// The index rows go with them: `log_entity.log_id` cascades.
    ///
    /// # Errors
    ///
    /// Returns any database error from the delete.
    pub async fn prune(&self, keep_secs: u64, now: i64) -> anyhow::Result<u64> {
        if keep_secs == 0 {
            return Ok(0);
        }
        let cutoff = now.saturating_sub(i64::try_from(keep_secs).unwrap_or(i64::MAX));
        let deleted = Logs::delete_many()
            .filter(logs::Column::Timestamp.lt(cutoff))
            .exec(&self.db)
            .await?;
        Ok(deleted.rows_affected)
    }
}

/// Where an entity's page is.
///
/// The one place the reference vocabulary meets the frontend's routes, so a
/// route change is a change here and nowhere else.
fn route_of(entity: &EntityRef) -> String {
    match entity {
        EntityRef::Package(p) => format!("/package/{}", p.0),
        EntityRef::Worker(w) => format!("/worker/{}", w.0),
        EntityRef::Build(b) => format!("/package/{}/build/{}", b.pkgbase, b.number),
    }
}

/// A stored row as the API returns it.
///
/// A payload that no longer parses becomes `null` rather than costing the entry:
/// `message` was rendered when it was written and still reads, which is the
/// whole reason it is stored.
fn render(row: logs::Model, hrefs: BTreeMap<String, Vec<Option<String>>>) -> LogEntry {
    let data = serde_json::from_str(&row.data).unwrap_or_else(|e| {
        tracing::warn!("log entry {} has an unreadable payload: {e}", row.id);
        serde_json::Value::Null
    });
    LogEntry {
        id: row.id,
        kind: row.kind,
        severity: row.severity,
        message: row.message,
        data,
        scope: row.scope.and_then(|raw| raw.parse().ok()),
        timestamp: row.timestamp,
        user: row.user,
        hrefs,
    }
}

/// Namespaces, re-exported so a caller building a filter does not have to reach
/// into the common crate for them.
pub const NAMESPACES: [&str; 3] = [NS_PACKAGE, NS_WORKER, NS_BUILD];
