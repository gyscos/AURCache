//! Makes `packages.out_of_date` a boolean, which is all it has ever held.
//!
//! On Postgres the column changes type, `0` becoming false and anything else
//! true. SQLite keeps its integer column: it has no boolean type, and reads
//! `0` and `1` as one.

use sea_orm::{ConnectionTrait, DbBackend};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if manager.get_database_backend() == DbBackend::Postgres {
            manager
                .get_connection()
                .execute_unprepared(
                    "ALTER TABLE packages ALTER COLUMN out_of_date DROP DEFAULT;
                     ALTER TABLE packages ALTER COLUMN out_of_date TYPE BOOLEAN
                         USING out_of_date <> 0;
                     ALTER TABLE packages ALTER COLUMN out_of_date SET DEFAULT false;",
                )
                .await?;
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if manager.get_database_backend() == DbBackend::Postgres {
            manager
                .get_connection()
                .execute_unprepared(
                    "ALTER TABLE packages ALTER COLUMN out_of_date DROP DEFAULT;
                     ALTER TABLE packages ALTER COLUMN out_of_date TYPE INTEGER
                         USING CASE WHEN out_of_date THEN 1 ELSE 0 END;
                     ALTER TABLE packages ALTER COLUMN out_of_date SET DEFAULT 0;",
                )
                .await?;
        }
        Ok(())
    }
}
