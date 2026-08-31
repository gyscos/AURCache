//! A record of one bulk package-add, and how far it has got.
//!
//! Adding a thousand packages takes minutes -- a `git` checkout per package --
//! so the request that starts it cannot wait for it. The row is what the caller
//! is handed instead: the work runs detached and writes its progress here, so an
//! observer can attach, leave, and come back without affecting it, and an error
//! that happened while nobody was watching is still there afterwards.
//!
//! `log` is append-only, one JSON object per line, read by line offset the same
//! way `builds.output` is. Storing outcomes as lines rather than as rows keeps
//! the reader a single query and matches how build output already works.

use crate::helpers::dbtype::database_type;
use sea_orm::DbBackend;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        // `finished_at` NULL means still running. A job whose server died is
        // closed out at startup rather than left claiming to be running.
        let sql = match database_type() {
            DbBackend::Sqlite => {
                "CREATE TABLE IF NOT EXISTS bulk_adds (\
                   id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT, \
                   created_at BIGINT NOT NULL, \
                   finished_at BIGINT, \
                   total INTEGER NOT NULL, \
                   completed INTEGER NOT NULL DEFAULT 0, \
                   failed INTEGER NOT NULL DEFAULT 0, \
                   log TEXT NOT NULL DEFAULT ''\
                 );"
            }
            DbBackend::Postgres => {
                "CREATE TABLE IF NOT EXISTS bulk_adds (\
                   id SERIAL PRIMARY KEY, \
                   created_at BIGINT NOT NULL, \
                   finished_at BIGINT, \
                   total INTEGER NOT NULL, \
                   completed INTEGER NOT NULL DEFAULT 0, \
                   failed INTEGER NOT NULL DEFAULT 0, \
                   log TEXT NOT NULL DEFAULT ''\
                 );"
            }
            _ => return Err(DbErr::Migration("Unsupported database type".to_string())),
        };
        db.execute_unprepared(sql).await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DROP TABLE IF EXISTS bulk_adds;")
            .await?;
        Ok(())
    }
}
