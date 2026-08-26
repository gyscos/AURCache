//! Store the AUR metadata the package page shows.
//!
//! It used to be fetched live on every request to `/api/package/<pkgbase>`,
//! which made that route ~128ms where the rest of it is ~2ms, and spent one of
//! the AUR's 4000 daily calls per page view.
//!
//! Nothing here adds AUR traffic. The version-check scheduler already queries
//! every AUR package on its interval, in one bulk request, and keeps only the
//! version — these columns are the rest of that same response, written down
//! instead of discarded.
//!
//! `aur_flagged_outdated` is the AUR's own "flagged out of date" marker, which
//! is not `packages.out_of_date`: that one means upstream is newer than what we
//! last built, and is computed here rather than reported by the AUR.

use crate::helpers::dbtype::database_type;
use sea_orm::DbBackend;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

/// Every column is nullable: a package that has not been version-checked since
/// this migration ran has no cached metadata yet, which is a real state and
/// distinct from "the AUR reports an empty description".
const SQLITE_UP: &str = r"
alter table packages add aur_description TEXT;
alter table packages add aur_maintainer TEXT;
alter table packages add aur_project_url TEXT;
alter table packages add aur_licenses TEXT;
alter table packages add aur_first_submitted BIGINT;
alter table packages add aur_last_modified BIGINT;
alter table packages add aur_flagged_outdated BOOLEAN;
";

const POSTGRES_UP: &str = r"
ALTER TABLE packages ADD COLUMN aur_description TEXT;
ALTER TABLE packages ADD COLUMN aur_maintainer TEXT;
ALTER TABLE packages ADD COLUMN aur_project_url TEXT;
ALTER TABLE packages ADD COLUMN aur_licenses TEXT;
ALTER TABLE packages ADD COLUMN aur_first_submitted BIGINT;
ALTER TABLE packages ADD COLUMN aur_last_modified BIGINT;
ALTER TABLE packages ADD COLUMN aur_flagged_outdated BOOLEAN;
";

const SQLITE_DOWN: &str = r"
alter table packages drop column aur_description;
alter table packages drop column aur_maintainer;
alter table packages drop column aur_project_url;
alter table packages drop column aur_licenses;
alter table packages drop column aur_first_submitted;
alter table packages drop column aur_last_modified;
alter table packages drop column aur_flagged_outdated;
";

const POSTGRES_DOWN: &str = r"
ALTER TABLE packages DROP COLUMN aur_description;
ALTER TABLE packages DROP COLUMN aur_maintainer;
ALTER TABLE packages DROP COLUMN aur_project_url;
ALTER TABLE packages DROP COLUMN aur_licenses;
ALTER TABLE packages DROP COLUMN aur_first_submitted;
ALTER TABLE packages DROP COLUMN aur_last_modified;
ALTER TABLE packages DROP COLUMN aur_flagged_outdated;
";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        let sql = match database_type() {
            DbBackend::Sqlite => SQLITE_UP,
            DbBackend::Postgres => POSTGRES_UP,
            _ => return Err(DbErr::Migration("Unsupported database type".to_string())),
        };
        db.execute_unprepared(sql).await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        let sql = match database_type() {
            DbBackend::Sqlite => SQLITE_DOWN,
            DbBackend::Postgres => POSTGRES_DOWN,
            _ => return Err(DbErr::Migration("Unsupported database type".to_string())),
        };
        db.execute_unprepared(sql).await?;
        Ok(())
    }
}
