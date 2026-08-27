//! Records how many times each built package file has been downloaded.
//!
//! Keyed by file name rather than by package. The counter is written from the
//! repository file server, which knows the path it just served and nothing
//! else; resolving that to a package would mean a query per download, which is
//! precisely what the buffering in front of this table exists to avoid. The
//! join to a package happens when the count is read, which is rare.
//!
//! The row is created on first download, so a file nobody has fetched has no
//! row rather than a zero -- "never downloaded" and "downloaded zero times"
//! are the same thing here, and the absent row is cheaper.

use crate::helpers::dbtype::database_type;
use sea_orm::DbBackend;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // `count` is signed because both backends' INTEGER is, and a count
        // that could go negative through a bug is better than one that wraps.
        let sql = match database_type() {
            DbBackend::Sqlite => {
                "CREATE TABLE IF NOT EXISTS download_counts (\
                   file_name TEXT NOT NULL PRIMARY KEY, \
                   count INTEGER NOT NULL DEFAULT 0, \
                   last_download INTEGER\
                 );"
            }
            DbBackend::Postgres => {
                "CREATE TABLE IF NOT EXISTS download_counts (\
                   file_name TEXT NOT NULL PRIMARY KEY, \
                   count BIGINT NOT NULL DEFAULT 0, \
                   last_download BIGINT\
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
            .execute_unprepared("DROP TABLE IF EXISTS download_counts;")
            .await?;
        Ok(())
    }
}
