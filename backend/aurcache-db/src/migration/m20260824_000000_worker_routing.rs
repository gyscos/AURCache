//! Adds worker routing columns: package affinity, priority, and reported
//! concurrency.
//!
//! Every default is chosen so the new routing rules are inert until someone
//! configures them: an empty affinity list reserves nothing, priority 0 blocks
//! nobody, and concurrency 1 is the most conservative capacity assumption for a
//! worker that predates the field.

use crate::helpers::dbtype::database_type;
use sea_orm::DbBackend;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

/// `ALTER TABLE workers ADD …` fragments, identical for both backends apart
/// from Postgres wanting the `COLUMN` keyword.
const COLUMNS: [&str; 3] = [
    "package_affinity TEXT NOT NULL DEFAULT ''",
    "priority INTEGER NOT NULL DEFAULT 0",
    "concurrency INTEGER NOT NULL DEFAULT 1",
];

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        let keyword = match database_type() {
            DbBackend::Sqlite => "",
            DbBackend::Postgres => "COLUMN ",
            _ => return Err(DbErr::Migration("Unsupported database type".to_string())),
        };

        for col in COLUMNS {
            db.execute_unprepared(&format!("ALTER TABLE workers ADD {keyword}{col};"))
                .await?;
        }

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        match database_type() {
            DbBackend::Sqlite | DbBackend::Postgres => {
                for col in ["concurrency", "priority", "package_affinity"] {
                    db.execute_unprepared(&format!("ALTER TABLE workers DROP COLUMN {col};"))
                        .await?;
                }
            }
            _ => return Err(DbErr::Migration("Unsupported database type".to_string())),
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::migration::Migrator;
    use sea_orm::{ConnectionTrait, Database};
    use sea_orm_migration::MigratorTrait;

    #[tokio::test]
    async fn routing_columns_exist_with_inert_defaults() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

        db.execute_unprepared(
            "INSERT INTO workers (id, name, cert_fingerprint) VALUES (1, 'w1', 'fp1');",
        )
        .await
        .unwrap();

        let row = db
            .query_one(sea_orm::Statement::from_string(
                db.get_database_backend(),
                "SELECT package_affinity, priority, concurrency FROM workers WHERE id = 1"
                    .to_string(),
            ))
            .await
            .unwrap()
            .expect("row exists");

        // Defaults must leave routing behaviour unchanged for an old worker.
        let affinity: String = row.try_get("", "package_affinity").unwrap();
        let priority: i32 = row.try_get("", "priority").unwrap();
        let concurrency: i32 = row.try_get("", "concurrency").unwrap();
        assert_eq!(affinity, "");
        assert_eq!(priority, 0);
        assert_eq!(concurrency, 1);
    }
}
