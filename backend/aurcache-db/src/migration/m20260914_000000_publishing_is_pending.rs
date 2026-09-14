//! A build being published still counts as the pending build of its package
//! and platform.
//!
//! `builds_pending_pkg_platform` allows one pending build per `(pkg_id,
//! platform)`: it is what makes enqueueing idempotent. A build that has been
//! built and is being put in the repository (`PUBLISHING`, 5) is not finished
//! yet, and letting a second build be queued beside it would have two builds of
//! the same package racing to publish. So the index covers it too.

use crate::helpers::dbtype::database_type;
use sea_orm::{ConnectionTrait, DbBackend};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

fn schema_prefix() -> &'static str {
    if database_type() == DbBackend::Postgres {
        "public."
    } else {
        ""
    }
}

async fn recreate(manager: &SchemaManager<'_>, statuses: &str) -> Result<(), DbErr> {
    let db = manager.get_connection();
    let schema = schema_prefix();
    db.execute_unprepared(&format!(
        "DROP INDEX IF EXISTS {schema}idx_builds_pending_pkg_platform;"
    ))
    .await?;
    db.execute_unprepared(&format!(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_builds_pending_pkg_platform \
         ON {schema}builds (pkg_id, platform) WHERE status IN ({statuses});"
    ))
    .await?;
    Ok(())
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Active, enqueued, waiting for dependencies, publishing.
        recreate(manager, "0, 3, 4, 5").await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        recreate(manager, "0, 3, 4").await
    }
}
