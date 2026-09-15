//! Drops the stored `cpu_limit` and `memory_limit` settings.
//!
//! Both were server settings from when the server ran builds itself, and
//! nothing has read them since builds moved to workers: a limit applies where
//! the build runs, so each worker now sets its own (`WORKER_BUILD_MEMORY_MAX`,
//! `WORKER_BUILD_CPUS`). The settings are gone from the API and the UI; their
//! rows would otherwise stay in the table and travel in every dump.
//!
//! No `down`: the values were never used, so there is nothing a rollback would
//! need back.

use aurcache_common::settings::RETIRED_SETTING_KEYS;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .exec_stmt(
                Query::delete()
                    .from_table(Alias::new("settings"))
                    .and_where(
                        Expr::col(Alias::new("key")).is_in(RETIRED_SETTING_KEYS.iter().copied()),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}
