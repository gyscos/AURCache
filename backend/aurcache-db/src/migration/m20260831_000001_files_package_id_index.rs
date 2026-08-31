//! Indexes `files.package_id`, which every read of the table filters on.
//!
//! The package page lists one package's artifacts, and the package *list* now
//! totals their sizes with a correlated subquery per row -- both scan `files`
//! by `package_id`, which was previously unindexed and meant a full table scan
//! per package on the list.

use crate::helpers::dbtype::database_type;
use sea_orm::DbBackend;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        match database_type() {
            DbBackend::Sqlite | DbBackend::Postgres => {
                db.execute_unprepared(
                    "CREATE INDEX IF NOT EXISTS idx_files_package_id ON files (package_id);",
                )
                .await?;
            }
            _ => return Err(DbErr::Migration("Unsupported database type".to_string())),
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DROP INDEX IF EXISTS idx_files_package_id;")
            .await?;
        Ok(())
    }
}
