//! What commit each of a build's VCS sources was at, as a JSON object on the
//! build: `{"<source_url>": "<commit>"}`.
//!
//! `package_vcs_sources` records what the *version check* last saw, which is a
//! different fact and cannot stand in for this one: any build triggered outside
//! the check -- a manual rebuild, a retry, an unforced update -- leaves that
//! watermark behind, so the next check re-detects a move it has already built
//! and rebuilds a commit that is in the repository. Three of
//! `ttf-google-fonts-git`'s builds in one afternoon were that.
//!
//! On the build because the baseline has to be the last *successful* build's:
//! a build that failed at a commit says nothing about what is in the
//! repository, and treating it as the baseline would leave the package sitting
//! at a commit nothing ever produced -- the trap `package_update_inner` already
//! documents for versions.
//!
//! A column rather than a table, like `packages.split_packages` and
//! `source_data` before it. The set is only ever read and written whole, for
//! one build; nothing joins on a source or asks which builds used a commit. So
//! a relation would buy queries nobody makes, at the price of a second query on
//! the version check's hot path, an insert-many where an update does, and a
//! uniqueness constraint that a JSON object's keys give for free.
//!
//! NULL means *unknown* -- a build that predates this, or one whose sources
//! could not be resolved -- and is never read as "nothing changed".

use crate::helpers::dbtype::database_type;
use sea_orm::DbBackend;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        let sql = match database_type() {
            DbBackend::Sqlite | DbBackend::Postgres => {
                "ALTER TABLE builds ADD COLUMN vcs_sources TEXT;"
            }
            _ => return Err(DbErr::Migration("Unsupported database type".to_string())),
        };
        // SQLite has no `ADD COLUMN IF NOT EXISTS`, and a migration that has
        // already run must not fail a restart.
        if let Err(e) = db.execute_unprepared(sql).await {
            let message = e.to_string();
            if !message.contains("duplicate column") && !message.contains("already exists") {
                return Err(e);
            }
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("ALTER TABLE builds DROP COLUMN vcs_sources;")
            .await?;
        Ok(())
    }
}
