//! Makes `workers.settings_declaration` required: every worker declares its
//! settings when it registers.
//!
//! A row still NULL belongs to a worker that has not registered since
//! declarations existed. It is filled with an empty declaration, which the
//! worker's next registration replaces -- workers register at every start.
//! Postgres also gets the constraint; SQLite keeps its column as it is, and
//! nothing writes a NULL into it any more.

use sea_orm::{ConnectionTrait, DbBackend};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared(
            "UPDATE workers SET settings_declaration = '[]' WHERE settings_declaration IS NULL",
        )
        .await?;
        if manager.get_database_backend() == DbBackend::Postgres {
            db.execute_unprepared(
                "ALTER TABLE workers ALTER COLUMN settings_declaration SET NOT NULL",
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
                    "ALTER TABLE workers ALTER COLUMN settings_declaration DROP NOT NULL",
                )
                .await?;
        }
        Ok(())
    }
}
