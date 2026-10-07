//! Makes a package's status what its builds add up to, and drops
//! `packages.latest_build`.
//!
//! The status used to be copied onto the package at each transition, by
//! whichever path made it, and `latest_build` was meant to say which build it
//! described -- but only a package's first build and an abandoned build's retry
//! ever set it, so the checks against it compared with a stale id and a
//! cancelled or failed build could leave its package "building" for good. The
//! status is now worked out from the newest build on each platform
//! (`helpers::builds::refresh_package_status`) whenever a build changes, and
//! this works it out once for every package.
//!
//! `builds.status` was nullable, though nothing has written a NULL: any that
//! is there is read as failed, and Postgres is told the column cannot be NULL.
//! SQLite cannot add the constraint without rebuilding the table, which is not
//! worth doing for a value that is never written.
//!
//! `down` brings `latest_build` back empty: which build each package showed is
//! what was wrong with it.

use crate::helpers::builds::refresh_package_status;
use crate::packages;
use sea_orm::{ConnectionTrait, DbBackend, EntityTrait, QuerySelect};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum Packages {
    Table,
    LatestBuild,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        // Failed, as `aurcache_common::build_state::BuildState::Failed`.
        db.execute_unprepared("UPDATE builds SET status = 2 WHERE status IS NULL")
            .await?;
        if manager.get_database_backend() == DbBackend::Postgres {
            db.execute_unprepared("ALTER TABLE builds ALTER COLUMN status SET NOT NULL")
                .await?;
        }
        manager
            .alter_table(
                Table::alter()
                    .table(Packages::Table)
                    .drop_column(Packages::LatestBuild)
                    .to_owned(),
            )
            .await?;

        let ids: Vec<i32> = packages::Entity::find()
            .select_only()
            .column(packages::Column::Id)
            .into_tuple()
            .all(db)
            .await?;
        for id in ids {
            refresh_package_status(db, id).await?;
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if manager.get_database_backend() == DbBackend::Postgres {
            manager
                .get_connection()
                .execute_unprepared("ALTER TABLE builds ALTER COLUMN status DROP NOT NULL")
                .await?;
        }
        manager
            .alter_table(
                Table::alter()
                    .table(Packages::Table)
                    .add_column(ColumnDef::new(Packages::LatestBuild).integer().null())
                    .to_owned(),
            )
            .await
    }
}
