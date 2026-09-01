//! Generalises `bulk_adds` into `operations`.
//!
//! The table was introduced for bulk package adds, but nothing about it is
//! specific to adding: it is a long operation that returns before it finishes,
//! reports progress nobody has to be watching, and survives the observer going
//! away. A restore is the same shape, so it takes the same table rather than a
//! parallel one that would need the same startup cleanup, the same offset
//! reads, and the same care about writing progress as it goes.
//!
//! `kind` distinguishes them. Existing rows are bulk adds by definition -- they
//! were written before anything else could produce one.

use crate::helpers::dbtype::database_type;
use sea_orm::DbBackend;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

const SQLITE_UP: &str = r"
alter table bulk_adds rename to operations;
alter table operations add kind TEXT NOT NULL DEFAULT 'bulk_add';
";

const POSTGRES_UP: &str = r"
ALTER TABLE bulk_adds RENAME TO operations;
ALTER TABLE operations ADD COLUMN kind TEXT NOT NULL DEFAULT 'bulk_add';
";

const SQLITE_DOWN: &str = r"
alter table operations drop column kind;
alter table operations rename to bulk_adds;
";

const POSTGRES_DOWN: &str = r"
ALTER TABLE operations DROP COLUMN kind;
ALTER TABLE operations RENAME TO bulk_adds;
";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let sql = match database_type() {
            DbBackend::Sqlite => SQLITE_UP,
            DbBackend::Postgres => POSTGRES_UP,
            _ => return Err(DbErr::Migration("Unsupported database type".to_string())),
        };
        manager.get_connection().execute_unprepared(sql).await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let sql = match database_type() {
            DbBackend::Sqlite => SQLITE_DOWN,
            DbBackend::Postgres => POSTGRES_DOWN,
            _ => return Err(DbErr::Migration("Unsupported database type".to_string())),
        };
        manager.get_connection().execute_unprepared(sql).await?;
        Ok(())
    }
}
