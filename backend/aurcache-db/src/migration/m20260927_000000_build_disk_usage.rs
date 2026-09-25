//! Adds a build's disk usage on its worker, part by part: what it wrote into
//! its chroot, its working space, its package's source cache and kept build
//! tree. Nullable: builds from before this, and workers without a storage
//! pool, have no figures, which is not the same as having used nothing.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

/// The columns, in the order the API lists the parts.
const COLUMNS: [&str; 4] = [
    "disk_chroot",
    "disk_workdir",
    "disk_sources",
    "disk_build_tree",
];

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // One column per statement: SQLite alters a table one change at a time.
        for column in COLUMNS {
            manager
                .alter_table(
                    Table::alter()
                        .table(Alias::new("builds"))
                        .add_column(ColumnDef::new(Alias::new(column)).big_integer().null())
                        .to_owned(),
                )
                .await?;
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        for column in COLUMNS {
            manager
                .alter_table(
                    Table::alter()
                        .table(Alias::new("builds"))
                        .drop_column(Alias::new(column))
                        .to_owned(),
                )
                .await?;
        }
        Ok(())
    }
}
