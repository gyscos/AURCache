//! Records how much memory each build's process tree needed at its peak.
//!
//! The question it answers is whether a package can be built on a given
//! machine at all. A build that was OOM-killed reports exit 137 and nothing
//! else; knowing that the last successful build of the same package peaked at
//! 6 GiB turns that into an actionable number.
//!
//! Per build, so it is per platform and per attempt: the same package needs
//! different amounts on different architectures, and a build that failed early
//! is not evidence about the one that succeeded.
//!
//! NULL where the worker did not report one -- an older worker, or a build that
//! ended before the first sample. Distinct from a build that used no memory,
//! which cannot happen.

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
            DbBackend::Sqlite => "alter table builds add peak_memory BIGINT;",
            DbBackend::Postgres => "ALTER TABLE builds ADD COLUMN peak_memory BIGINT;",
            _ => return Err(DbErr::Migration("Unsupported database type".to_string())),
        };
        db.execute_unprepared(sql).await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        let sql = match database_type() {
            DbBackend::Sqlite => "alter table builds drop column peak_memory;",
            DbBackend::Postgres => "ALTER TABLE builds DROP COLUMN peak_memory;",
            _ => return Err(DbErr::Migration("Unsupported database type".to_string())),
        };
        db.execute_unprepared(sql).await?;
        Ok(())
    }
}
