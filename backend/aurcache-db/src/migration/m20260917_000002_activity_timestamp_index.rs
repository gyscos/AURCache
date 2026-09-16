//! Indexes the activity log by the order it is read in.
//!
//! The log has always been listed newest first, and is now also paged, filtered
//! and counted per request. Without an index every one of those reads a table
//! that only grows -- and it grows faster now that the server records its own
//! restarts and the failures it notices.
//!
//! `(timestamp, id)` rather than `timestamp` alone because that is the whole
//! ordering: the log records whole seconds, so a bulk add writes several
//! entries within one, and the id is what keeps a page boundary between them
//! from dropping or repeating a row.

use crate::helpers::dbtype::database_type;
use sea_orm::DbBackend;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        let sql = match database_type() {
            DbBackend::Sqlite => {
                "create index if not exists idx_activity_timestamp_id \
                 on activity (timestamp desc, id desc);"
            }
            DbBackend::Postgres => {
                "CREATE INDEX IF NOT EXISTS idx_activity_timestamp_id \
                 ON activity (timestamp DESC, id DESC);"
            }
            _ => return Err(DbErr::Migration("Unsupported database type".to_string())),
        };
        db.execute_unprepared(sql).await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("drop index if exists idx_activity_timestamp_id;")
            .await?;
        Ok(())
    }
}
