//! Values set for a worker's settings on the server.
//!
//! One row per worker and key, holding the value as the operator wrote it. The
//! server validates it against the worker's declaration when it is saved and
//! otherwise does not interpret it; the worker parses it on delivery.
//!
//! Per worker only: there is no fleet default, so no `NULL` scope and no
//! partial index. A value outlives its key being dropped from the worker's
//! declaration -- it is shown as no longer offered rather than deleted behind
//! the operator's back -- and goes with the worker's row.
//!
//! See `design/implemented/worker-configuration.md`.

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
CREATE TABLE worker_settings
(
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    worker_id INTEGER NOT NULL
        REFERENCES workers (id) ON DELETE CASCADE,
    key TEXT NOT NULL,
    value TEXT NOT NULL,
    UNIQUE (worker_id, key)
);
",
                )
                .await?;
            }
            DbBackend::Postgres => {
                db.execute_unprepared(
                    r"
CREATE TABLE public.worker_settings
(
    id SERIAL PRIMARY KEY,
    worker_id INTEGER NOT NULL
        REFERENCES public.workers (id) ON DELETE CASCADE,
    key TEXT NOT NULL,
    value TEXT NOT NULL,
    UNIQUE (worker_id, key)
);
",
                )
                .await?;
            }
            _ => return Err(DbErr::Migration("Unsupported database type".to_string())),
        }

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        match database_type() {
            DbBackend::Sqlite => {
                db.execute_unprepared("DROP TABLE worker_settings;").await?;
            }
            DbBackend::Postgres => {
                db.execute_unprepared("DROP TABLE public.worker_settings;")
                    .await?;
            }
            _ => return Err(DbErr::Migration("Unsupported database type".to_string())),
        }

        Ok(())
    }
}
