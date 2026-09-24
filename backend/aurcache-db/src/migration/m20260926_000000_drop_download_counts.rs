//! Drops the download-count feature's table: per-file fetch counts are no
//! longer recorded, so `download_counts` is dead schema.
//!
//! `down` recreates the table empty, mirroring the migration that created it;
//! the counts themselves are not recoverable, which is the point of dropping
//! the feature.

use crate::helpers::dbtype::database_type;
use sea_orm::DbBackend;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DROP TABLE IF EXISTS download_counts;")
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // The table as the migration that created it made it.
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
        manager.get_connection().execute_unprepared(sql).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::migration::Migrator;
    use sea_orm::{ConnectionTrait, Database, DatabaseConnection, Statement};
    use sea_orm_migration::MigratorTrait;

    async fn tables(db: &DatabaseConnection) -> Vec<String> {
        db.query_all_raw(Statement::from_string(
            db.get_database_backend(),
            "select name from sqlite_master where type = 'table' order by name",
        ))
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.try_get::<String>("", "name").unwrap())
        .collect()
    }

    /// Migrating up drops the download-count table; rolling back just this
    /// migration brings it back, empty.
    #[tokio::test]
    async fn migrating_up_drops_download_counts_and_rollback_restores_it() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        let got = tables(&db).await;
        assert!(
            !got.contains(&"download_counts".to_string()),
            "still present: {got:?}"
        );

        Migrator::down(
            &db,
            Some(crate::migration::steps_back_to(
                "m20260926_000000_drop_download_counts",
            )),
        )
        .await
        .unwrap();
        let got = tables(&db).await;
        assert!(
            got.contains(&"download_counts".to_string()),
            "missing: {got:?}"
        );
    }
}
