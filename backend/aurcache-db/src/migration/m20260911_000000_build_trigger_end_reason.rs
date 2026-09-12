//! Records why each build was created and, for server-decided stops, why it
//! ended.
//!
//! `builds.trigger` powers the derived retry budget for abandoned builds (see
//! `design/abort-builds.md`): rather than a mutable `attempt_count` that
//! survives a row that is never reused, the reaper counts the trailing
//! consecutive `timeout_retry` rows in a package's history. `User` (0) is the
//! meaning of every build that predates the column, and the default for new
//! ones whose caller does not say otherwise.
//!
//! `builds.end_reason` is why the *server* stopped the build, when it did the
//! stopping — manual cancel or one of the two abandonment paths. Worker-reported
//! terminal outcomes keep their `CompleteReport.reason` text and leave this
//! NULL; "nothing structured recorded" and "it failed for a worker-reported
//! reason" are different answers and this column says the second.

use crate::helpers::dbtype::database_type;
use sea_orm::{ConnectionTrait, DbBackend};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        let sql = match database_type() {
            DbBackend::Sqlite => {
                "alter table builds add trigger INTEGER NOT NULL DEFAULT 0; \
                 alter table builds add end_reason INTEGER NULL;"
            }
            DbBackend::Postgres => {
                "ALTER TABLE builds ADD COLUMN trigger INTEGER NOT NULL DEFAULT 0; \
                 ALTER TABLE builds ADD COLUMN end_reason INTEGER NULL;"
            }
            _ => return Err(DbErr::Migration("Unsupported database type".to_string())),
        };
        db.execute_unprepared(sql).await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        let sql = match database_type() {
            DbBackend::Sqlite => {
                "alter table builds drop column trigger; \
                 alter table builds drop column end_reason;"
            }
            DbBackend::Postgres => {
                "ALTER TABLE builds DROP COLUMN trigger; \
                 ALTER TABLE builds DROP COLUMN end_reason;"
            }
            _ => return Err(DbErr::Migration("Unsupported database type".to_string())),
        };
        db.execute_unprepared(sql).await?;
        Ok(())
    }
}