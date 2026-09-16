//! Drops the stored `max_concurrent_builds` and `builder_image` settings.
//!
//! Both are from when the server ran builds itself. Concurrency is now the
//! worker's own (`WORKER_CONCURRENCY`, reported at registration), and the
//! builder image named a container the server no longer runs. Nothing has read
//! either since builds moved to workers; their rows would otherwise stay in the
//! table and travel in every dump.
//!
//! The keys are written out rather than taken from `RETIRED_SETTING_KEYS`: a
//! migration is a fixed act, and one that read a list which grows would quietly
//! change what an already-applied migration means.
//!
//! No `down`: the values were never used, so there is nothing a rollback would
//! need back.

use sea_orm_migration::prelude::*;

/// What this migration retires, as of when it was written.
const KEYS: [&str; 2] = ["max_concurrent_builds", "builder_image"];

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .exec_stmt(
                Query::delete()
                    .from_table(Alias::new("settings"))
                    .and_where(Expr::col(Alias::new("key")).is_in(KEYS))
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}
