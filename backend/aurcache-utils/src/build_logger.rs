//! Buffered writer for a build's `output` column.
//!
//! Appends are pushed onto an in-memory buffer and written out by a background
//! task, so a build emitting many short lines costs one `UPDATE` per flush
//! window instead of one per line. Callers that already batch their own output
//! (the worker log endpoint receives whole chunks) should use
//! [`append_build_output`] directly instead of paying for a task.

use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, Statement};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::{Mutex, Notify};
use tracing::{debug, error};

/// How long the flush task waits after the first buffered append before
/// writing, so a burst of lines collapses into a single `UPDATE`.
const FLUSH_INTERVAL: Duration = Duration::from_millis(1500);

/// Append `text` to a build's stored output.
///
/// The concatenation happens in SQL so the (potentially huge) existing output
/// is never read back into memory: this is a single O(1) `UPDATE` rather than a
/// read-modify-write.
pub async fn append_build_output(
    db: &DatabaseConnection,
    build_id: i32,
    text: &str,
) -> anyhow::Result<()> {
    let stmt = match db.get_database_backend() {
        DbBackend::Postgres => Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE builds SET output = COALESCE(output, '') || $1 WHERE id = $2",
            vec![text.into(), build_id.into()],
        ),
        backend => Statement::from_sql_and_values(
            backend,
            "UPDATE builds SET output = COALESCE(output, '') || ? WHERE id = ?",
            vec![text.into(), build_id.into()],
        ),
    };
    db.execute(stmt).await?;
    Ok(())
}

/// State shared between every [`BuildLogger`] handle and its flush task.
#[derive(Debug)]
struct Shared {
    build_id: i32,
    db: DatabaseConnection,
    buffer: Mutex<Vec<String>>,
    /// Raised when text is buffered, and once more when the last handle drops.
    wake: Notify,
    /// Set once the last handle has dropped: drain the buffer, then stop.
    shutdown: AtomicBool,
}

impl Shared {
    fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::Acquire)
    }

    /// Write out everything buffered so far. On a database error the buffer is
    /// deliberately left intact so the next flush retries it.
    async fn flush(&self) -> anyhow::Result<()> {
        let mut buffer = self.buffer.lock().await;
        if buffer.is_empty() {
            return Ok(());
        }

        append_build_output(&self.db, self.build_id, &buffer.concat()).await?;

        buffer.clear();
        debug!("Log buffer flushed!");
        Ok(())
    }

    async fn flush_or_log(&self) {
        if let Err(e) = self.flush().await {
            error!(
                "Failed to flush log buffer for build #{}: {e}",
                self.build_id
            );
        }
    }
}

/// Signals shutdown when the last [`BuildLogger`] handle drops.
///
/// Deliberately *not* held by the flush task: if the task held it, the
/// reference count could never reach zero and this `Drop` would never run.
#[derive(Debug)]
struct ShutdownOnDrop(Arc<Shared>);

impl Drop for ShutdownOnDrop {
    fn drop(&mut self) {
        self.0.shutdown.store(true, Ordering::Release);
        // `notify_one` leaves a permit behind when the task is not currently
        // parked, so this wakeup cannot be missed whatever the task is doing.
        self.0.wake.notify_one();
    }
}

/// A cloneable handle for appending to one build's output.
///
/// The background flush task lives exactly as long as the handles do: when the
/// last clone is dropped it drains whatever is left and exits.
#[derive(Debug, Clone)]
pub struct BuildLogger {
    shared: Arc<Shared>,
    /// Kept only so its `Drop` fires when the last handle goes.
    _shutdown: Arc<ShutdownOnDrop>,
}

impl BuildLogger {
    /// Create a logger and start its flush task. Must be called from within a
    /// tokio runtime.
    #[must_use]
    pub fn new(build_id: i32, db: DatabaseConnection) -> Self {
        let shared = Arc::new(Shared {
            build_id,
            db,
            buffer: Mutex::new(Vec::new()),
            wake: Notify::new(),
            shutdown: AtomicBool::new(false),
        });

        let task_shared = Arc::clone(&shared);
        tokio::spawn(async move {
            loop {
                task_shared.wake.notified().await;
                if task_shared.is_shutdown() {
                    break;
                }
                // Debounce, so a burst of appends becomes a single UPDATE.
                tokio::time::sleep(FLUSH_INTERVAL).await;
                task_shared.flush_or_log().await;
            }
            // The last handle is gone: drain the remainder and stop.
            task_shared.flush_or_log().await;
        });

        Self {
            _shutdown: Arc::new(ShutdownOnDrop(Arc::clone(&shared))),
            shared,
        }
    }

    /// Buffer `text` for the next flush.
    pub async fn append(&self, text: String) {
        debug!("{text}");
        self.shared.buffer.lock().await.push(text);
        self.shared.wake.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aurcache_db::migration::Migrator;
    use sea_orm::Database;
    use sea_orm_migration::MigratorTrait;

    async fn setup() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        db.execute_unprepared("INSERT INTO packages (id, name) VALUES (1, 'p1')")
            .await
            .unwrap();
        db.execute_unprepared(
            "INSERT INTO builds (id, pkg_id, platform, version) VALUES (1, 1, 'x86_64', '1.0')",
        )
        .await
        .unwrap();
        db
    }

    async fn output(db: &DatabaseConnection) -> Option<String> {
        use aurcache_db::prelude::Builds;
        use sea_orm::EntityTrait;
        Builds::find_by_id(1).one(db).await.unwrap().unwrap().output
    }

    /// Poll until `cond` holds. A shutdown that lands mid-debounce waits that
    /// window out before the final drain, so the bound is generously above
    /// [`FLUSH_INTERVAL`] rather than tuned to it.
    async fn eventually(mut cond: impl AsyncFnMut() -> bool) {
        for _ in 0..100 {
            if cond().await {
                return;
            }
            tokio::time::sleep(FLUSH_INTERVAL / 10).await;
        }
        panic!("condition never became true");
    }

    /// Dropping the last handle must drain the buffer and stop the task,
    /// rather than leaving an immortal task behind for every logger created.
    #[tokio::test]
    async fn dropping_the_last_handle_flushes_and_stops_the_task() {
        let db = setup().await;

        let logger = BuildLogger::new(1, db.clone());
        let shared = Arc::clone(&logger.shared);
        logger.append("hello\n".to_string()).await;
        logger.append("world\n".to_string()).await;

        // Still buffered: the flush window has not elapsed.
        assert_eq!(output(&db).await, None);

        drop(logger);

        eventually(async || output(&db).await.is_some()).await;
        assert_eq!(output(&db).await.as_deref(), Some("hello\nworld\n"));

        // Only this test's reference is left, so the task dropped its own and
        // returned instead of looping forever.
        eventually(async || Arc::strong_count(&shared) == 1).await;
    }

    /// A clone keeps the task alive; only the last handle shuts it down.
    #[tokio::test]
    async fn clone_keeps_the_task_alive() {
        let db = setup().await;

        let logger = BuildLogger::new(1, db.clone());
        let shared = Arc::clone(&logger.shared);
        let clone = logger.clone();
        drop(logger);

        clone.append("from the clone\n".to_string()).await;
        assert!(Arc::strong_count(&shared) > 1, "task must still be running");

        drop(clone);
        eventually(async || output(&db).await.is_some()).await;
        assert_eq!(output(&db).await.as_deref(), Some("from the clone\n"));
    }

    #[tokio::test]
    async fn append_build_output_concatenates() {
        let db = setup().await;
        append_build_output(&db, 1, "a").await.unwrap();
        append_build_output(&db, 1, "b").await.unwrap();
        assert_eq!(output(&db).await.as_deref(), Some("ab"));
    }
}
