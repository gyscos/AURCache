//! Whether an operator has asked a worker to pause: take no new builds and let
//! the ones it holds finish.
//!
//! A lifecycle state rather than a worker setting, and held on the server: the
//! claim query stops offering the worker jobs the moment it is set, with no
//! message to the worker and nothing for it to apply. Not null, because every
//! existing worker is simply not paused.
//!
//! See `design/implemented/worker-configuration.md` ("Drain", which shipped
//! as pause).

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
            DbBackend::Sqlite => "alter table workers add paused BOOLEAN NOT NULL DEFAULT 0;",
            DbBackend::Postgres => {
                "ALTER TABLE workers ADD COLUMN paused BOOLEAN NOT NULL DEFAULT FALSE;"
            }
            _ => return Err(DbErr::Migration("Unsupported database type".to_string())),
        };
        db.execute_unprepared(sql).await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        let sql = match database_type() {
            DbBackend::Sqlite => "alter table workers drop column paused;",
            DbBackend::Postgres => "ALTER TABLE workers DROP COLUMN paused;",
            _ => return Err(DbErr::Migration("Unsupported database type".to_string())),
        };
        db.execute_unprepared(sql).await?;
        Ok(())
    }
}
