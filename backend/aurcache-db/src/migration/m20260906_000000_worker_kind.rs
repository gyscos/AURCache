//! Records which build strategy each worker runs.
//!
//! A fleet can mix the devtools chroot worker with the deprecated container
//! builder, and the two produce packages by different means -- a build that
//! fails on one and not the other is the first thing an operator wants to see,
//! and until now the Workers page could not tell them apart.
//!
//! Free-form text rather than an enum: a future executor should be able to name
//! itself without a migration, and the server only stores and displays it.
//! NULL for a worker that enrolled before the column existed, which the page
//! shows as unknown rather than guessing at `chroot`.

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
            DbBackend::Sqlite => "alter table workers add kind TEXT;",
            DbBackend::Postgres => "ALTER TABLE workers ADD COLUMN kind TEXT;",
            _ => return Err(DbErr::Migration("Unsupported database type".to_string())),
        };
        db.execute_unprepared(sql).await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        let sql = match database_type() {
            DbBackend::Sqlite => "alter table workers drop column kind;",
            DbBackend::Postgres => "ALTER TABLE workers DROP COLUMN kind;",
            _ => return Err(DbErr::Migration("Unsupported database type".to_string())),
        };
        db.execute_unprepared(sql).await?;
        Ok(())
    }
}
