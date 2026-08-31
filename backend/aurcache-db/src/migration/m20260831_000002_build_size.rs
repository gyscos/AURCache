//! Records how large each build's output was.
//!
//! Written alongside the built version, from the artifact bytes the ingest
//! already holds, so it costs no extra I/O and lands only on the success path:
//! a build that failed or was cancelled produced nothing to measure, and NULL
//! says that where a `0` would claim it produced an empty package.
//!
//! Per build, so it is per platform. A package's total covers every platform it
//! builds for, so the two agree for a single-platform package and the package's
//! total is the sum across platforms otherwise.

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
            DbBackend::Sqlite => "alter table builds add size BIGINT;",
            DbBackend::Postgres => "ALTER TABLE builds ADD COLUMN size BIGINT;",
            _ => return Err(DbErr::Migration("Unsupported database type".to_string())),
        };
        db.execute_unprepared(sql).await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        let sql = match database_type() {
            DbBackend::Sqlite => "alter table builds drop column size;",
            DbBackend::Postgres => "ALTER TABLE builds DROP COLUMN size;",
            _ => return Err(DbErr::Migration("Unsupported database type".to_string())),
        };
        db.execute_unprepared(sql).await?;
        Ok(())
    }
}
