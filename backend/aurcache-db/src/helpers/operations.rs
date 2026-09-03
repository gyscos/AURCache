//! Reading and writing the progress of a long-running operation.
//!
//! The shape both bulk add and restore need: the request that starts the work
//! returns before any of it is done, the work runs detached, and progress is
//! *recorded* rather than pushed. Recording is what makes the rest true --
//! an observer can attach late and see the run from the beginning, one that
//! disconnects misses nothing, and an error that happened while nobody was
//! watching is still there afterwards. A pushed stream would need to buffer the
//! same log behind it to manage any of that.
//!
//! Generic over the entry type. The two kinds of operation report genuinely
//! different things -- a package was added or already existed, versus a package
//! was imported or skipped or overwritten -- and flattening both into one
//! stringly outcome would lose that. Only the transport is shared: one JSON
//! object per line, read by line offset.

use crate::helpers::time::now_secs;
use crate::operations;
use crate::prelude::Operations;
use sea_orm::ActiveValue::Set;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DbErr, EntityTrait, IntoActiveModel,
    QueryFilter, QueryOrder,
};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Adding several packages in one request.
pub const KIND_BULK_ADD: &str = "bulk_add";
/// Restoring an instance from a dump.
pub const KIND_RESTORE: &str = "restore";

/// Start an operation, returning its id.
///
/// `total` is fixed here because it is known before any work happens, and a
/// progress report without a denominator cannot say how far along it is.
pub async fn create<C: ConnectionTrait>(db: &C, kind: &str, total: i32) -> Result<i32, DbErr> {
    Ok(operations::ActiveModel {
        kind: Set(kind.to_string()),
        created_at: Set(now_secs()),
        total: Set(total),
        ..Default::default()
    }
    .insert(db)
    .await?
    .id)
}

/// Append outcomes and update the counters.
///
/// Called as each item finishes rather than once at the end. An operation that
/// runs for minutes and reports nothing until it stops reports nothing for the
/// whole time anyone would want to watch it, and the write is trivial beside
/// the work each entry represents.
pub async fn append<C: ConnectionTrait, T: Serialize>(
    db: &C,
    id: i32,
    completed: i32,
    failed: i32,
    entries: &[T],
    finished: bool,
) -> Result<(), DbErr> {
    let Some(row) = Operations::find_by_id(id).one(db).await? else {
        return Ok(());
    };
    let mut log = row.log.clone();
    for entry in entries {
        // An entry that will not serialise is dropped rather than failing the
        // operation: the work is done either way, and losing one line of the
        // report is better than abandoning the rest of the run.
        if let Ok(line) = serde_json::to_string(entry) {
            log.push_str(&line);
            log.push('\n');
        }
    }

    let mut active = row.into_active_model();
    active.log = Set(log);
    active.completed = Set(completed);
    active.failed = Set(failed);
    if finished {
        active.finished_at = Set(Some(now_secs()));
    }
    active.update(db).await?;
    Ok(())
}

/// The operation, if it exists.
pub async fn get<C: ConnectionTrait>(db: &C, id: i32) -> Result<Option<operations::Model>, DbErr> {
    Operations::find_by_id(id).one(db).await
}

/// The entries after the first `after` of them.
///
/// A caller holding none asks from zero and receives the whole run. An offset
/// past the end yields nothing rather than erroring, because a poll landing
/// between two writes is the normal case. A line that will not parse is
/// skipped: it was written by a version that knew a shape this one does not,
/// and the rest of the run is still worth reading.
#[must_use]
pub fn entries_after<T: DeserializeOwned>(log: &str, after: usize) -> Vec<T> {
    log.lines()
        .skip(after)
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// Close out operations that were running when the server stopped.
///
/// The work ran in a task, so it died with the process. The row would otherwise
/// report itself running forever and an observer would poll for progress that
/// cannot come. Whatever was already done stays done -- each item is committed
/// as it goes.
pub async fn close_orphaned<C: ConnectionTrait>(db: &C) -> Result<u64, DbErr> {
    let res = Operations::update_many()
        .col_expr(operations::Column::FinishedAt, Some(now_secs()).into())
        .filter(operations::Column::FinishedAt.is_null())
        .exec(db)
        .await?;
    Ok(res.rows_affected)
}

/// Operations still running, oldest first.
///
/// `finished_at IS NULL` is the whole definition of running, and startup closes
/// any row left open by a process that died (see [`close_orphaned`]), so this
/// cannot report a job that no longer exists.
///
/// The log is deliberately not returned: a caller listing what is in flight
/// wants counters, and a bulk add's log can be long enough that sending every
/// one of them to draw a progress bar would be the expensive part.
pub async fn active<C: ConnectionTrait>(db: &C) -> Result<Vec<operations::Model>, DbErr> {
    Operations::find()
        .filter(operations::Column::FinishedAt.is_null())
        .order_by_asc(operations::Column::CreatedAt)
        .all(db)
        .await
}

#[cfg(test)]
mod tests {
    use super::entries_after;
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize, PartialEq, Eq, Debug)]
    struct Entry {
        name: String,
    }

    fn log(names: &[&str]) -> String {
        names
            .iter()
            .map(|n| {
                serde_json::to_string(&Entry {
                    name: (*n).to_string(),
                })
                .unwrap()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Attaching with nothing in hand gives the whole run, including what
    /// happened before anyone was watching.
    #[test]
    fn a_late_observer_sees_the_whole_run() {
        let seen: Vec<Entry> = entries_after(&log(&["a", "b", "c"]), 0);
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[0].name, "a");
    }

    /// Polling asks from what it holds, so it receives only what is new.
    #[test]
    fn an_offset_returns_only_what_is_new() {
        let seen: Vec<Entry> = entries_after(&log(&["a", "b"]), 1);
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].name, "b");
    }

    /// A poll landing between writes is normal, not an error.
    #[test]
    fn an_offset_past_the_end_is_empty() {
        assert!(entries_after::<Entry>("", 0).is_empty());
        assert!(entries_after::<Entry>(&log(&["a"]), 5).is_empty());
    }

    /// A line this version cannot read does not cost the ones it can -- a dump
    /// restored across versions is the whole point of the feature.
    #[test]
    fn an_unreadable_line_is_skipped_not_fatal() {
        let mixed = format!("{}\n{{\"unknown\":true}}\n{}", log(&["a"]), log(&["b"]));
        let seen: Vec<Entry> = entries_after(&mixed, 0);
        assert_eq!(seen.len(), 2);
    }
}
