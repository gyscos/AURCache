//! Give every build a number that is meaningful on its own.
//!
//! A build was identified by its primary key, which is a global sequence: the
//! URL, the API and the UI all showed `#417`, a number that says nothing about
//! which package it belongs to or how many times that package has been built.
//! Builds are now `<pkgbase>/<n>` publicly — `hello/3` is the third build of
//! `hello` — and the row id goes back to being an internal detail.
//!
//! The number is **stored**, not computed. A `ROW_NUMBER()` over the rows would
//! renumber every later build whenever an earlier one is removed, so a link
//! shared yesterday would point at a different build today — worse than the id
//! it replaces. Assigned once at insert, it stays put.
//!
//! `UNIQUE (pkg_id, number)` is what makes that guarantee enforceable rather
//! than hoped for; it also turns a concurrent double-assignment into a failed
//! insert the caller can retry, instead of two builds sharing a name.

use crate::helpers::dbtype::database_type;
use sea_orm::DbBackend;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

/// Existing rows are numbered per package in id order — the order they were
/// created in.
///
/// Written as a correlated `COUNT` rather than a window function so the same
/// statement runs on both backends.
const BACKFILL: &str = "
UPDATE builds SET number = (
    SELECT COUNT(*) FROM builds AS earlier
    WHERE earlier.pkg_id = builds.pkg_id AND earlier.id <= builds.id
);
";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // `DEFAULT 0` only so the column can be NOT NULL on an existing table;
        // every row is given a real number below, and the unique index means a
        // second row left at the default cannot survive.
        let add = match database_type() {
            DbBackend::Sqlite => "alter table builds add number INTEGER NOT NULL DEFAULT 0;",
            DbBackend::Postgres => {
                "ALTER TABLE builds ADD COLUMN number INTEGER NOT NULL DEFAULT 0;"
            }
            _ => return Err(DbErr::Migration("Unsupported database type".to_string())),
        };
        db.execute_unprepared(add).await?;
        db.execute_unprepared(BACKFILL).await?;
        db.execute_unprepared(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_builds_pkg_number ON builds (pkg_id, number);",
        )
        .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared("DROP INDEX IF EXISTS idx_builds_pkg_number;")
            .await?;
        let drop = match database_type() {
            DbBackend::Sqlite => "alter table builds drop column number;",
            DbBackend::Postgres => "ALTER TABLE builds DROP COLUMN number;",
            _ => return Err(DbErr::Migration("Unsupported database type".to_string())),
        };
        db.execute_unprepared(drop).await?;
        Ok(())
    }
}
