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
CREATE TABLE package_vcs_sources
(
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    package_id INTEGER NOT NULL
        REFERENCES packages (id) ON DELETE CASCADE,
    source_url TEXT NOT NULL,
    last_commit TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE (package_id, source_url)
);
",
                )
                .await?;
            }
            DbBackend::Postgres => {
                db.execute_unprepared(
                    r"
CREATE TABLE public.package_vcs_sources
(
    id SERIAL PRIMARY KEY,
    package_id INTEGER NOT NULL
        REFERENCES public.packages (id) ON DELETE CASCADE,
    source_url TEXT NOT NULL,
    last_commit TEXT NOT NULL,
    updated_at BIGINT NOT NULL,
    UNIQUE (package_id, source_url)
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
                db.execute_unprepared("DROP TABLE package_vcs_sources;")
                    .await?;
            }
            DbBackend::Postgres => {
                db.execute_unprepared("DROP TABLE public.package_vcs_sources;")
                    .await?;
            }
            _ => Err(DbErr::Migration("Unsupported database type".to_string()))?,
        }

        Ok(())
    }
}
