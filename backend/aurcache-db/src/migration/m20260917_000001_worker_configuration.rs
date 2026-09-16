//! Records what each worker can be configured with, and what it is running.
//!
//! `settings_declaration` is the list of settings the worker declared at its
//! last registration; `effective_config` is what each of them resolved to on
//! that machine, reported over the heartbeat. Both are JSON, and both are kept
//! on the worker row so a worker's configuration can be read while it is
//! offline.
//!
//! JSON text rather than a table of rows: neither is queried, and the server
//! deliberately does not interpret a worker's settings -- it stores what the
//! worker said and renders it. Values the *server* sets get a table of their
//! own when they arrive (`design/worker-configuration.md`).
//!
//! NULL for a worker that has not reported one, which the Workers page shows as
//! a worker version that does not report its configuration -- different from a
//! worker with no settings.

use crate::helpers::dbtype::database_type;
use sea_orm::DbBackend;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        let statements: [&str; 2] = match database_type() {
            DbBackend::Sqlite => [
                "alter table workers add settings_declaration TEXT;",
                "alter table workers add effective_config TEXT;",
            ],
            DbBackend::Postgres => [
                "ALTER TABLE workers ADD COLUMN settings_declaration TEXT;",
                "ALTER TABLE workers ADD COLUMN effective_config TEXT;",
            ],
            _ => return Err(DbErr::Migration("Unsupported database type".to_string())),
        };
        for sql in statements {
            db.execute_unprepared(sql).await?;
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        let statements: [&str; 2] = match database_type() {
            DbBackend::Sqlite => [
                "alter table workers drop column settings_declaration;",
                "alter table workers drop column effective_config;",
            ],
            DbBackend::Postgres => [
                "ALTER TABLE workers DROP COLUMN settings_declaration;",
                "ALTER TABLE workers DROP COLUMN effective_config;",
            ],
            _ => return Err(DbErr::Migration("Unsupported database type".to_string())),
        };
        for sql in statements {
            db.execute_unprepared(sql).await?;
        }
        Ok(())
    }
}
