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
            DbBackend::Sqlite => {
                db.execute_unprepared(
                    r"
CREATE TABLE api_tokens
(
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    username TEXT NOT NULL UNIQUE,
    token_hash TEXT NOT NULL
);
",
                )
                .await?;
            }
            DbBackend::Postgres => {
                db.execute_unprepared(
                    r"
CREATE TABLE public.api_tokens
(
    id SERIAL PRIMARY KEY,
    username TEXT NOT NULL UNIQUE,
    token_hash TEXT NOT NULL
);
",
                )
                .await?;
            }
            _ => Err(DbErr::Migration("Unsupported database type".to_string()))?,
        }

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        match database_type() {
            DbBackend::Sqlite => {
                db.execute_unprepared("DROP TABLE api_tokens;").await?;
            }
            DbBackend::Postgres => {
                db.execute_unprepared("DROP TABLE public.api_tokens;")
                    .await?;
            }
            _ => Err(DbErr::Migration("Unsupported database type".to_string()))?,
        }

        Ok(())
    }
}
