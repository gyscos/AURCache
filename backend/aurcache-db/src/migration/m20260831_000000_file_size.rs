//! Records the on-disk size of each built package file.
//!
//! Written by `repo_ingest` from the artifact bytes it already holds in memory,
//! so recording it costs no extra I/O: it is the same number `repo_add` writes
//! into the pacman database as `%CSIZE%`, and the package page reports it
//! without touching the filesystem.
//!
//! Nullable rather than defaulted to zero, because a row written before this
//! migration has an unknown size, which is not the same as a zero-byte file.
//! `post_startup_tasks` fills those in from the filesystem; until it does, the
//! page shows the size as unknown instead of claiming the file is empty.

use crate::helpers::dbtype::database_type;
use sea_orm::DbBackend;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        // Signed because both backends' integers are signed, and a bad value
        // is better read as negative than wrapped into a plausible-looking
        // huge one.
        let sql = match database_type() {
            DbBackend::Sqlite => "alter table files add size BIGINT;",
            DbBackend::Postgres => "ALTER TABLE files ADD COLUMN size BIGINT;",
            _ => return Err(DbErr::Migration("Unsupported database type".to_string())),
        };
        db.execute_unprepared(sql).await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        let sql = match database_type() {
            DbBackend::Sqlite => "alter table files drop column size;",
            DbBackend::Postgres => "ALTER TABLE files DROP COLUMN size;",
            _ => return Err(DbErr::Migration("Unsupported database type".to_string())),
        };
        db.execute_unprepared(sql).await?;
        Ok(())
    }
}
