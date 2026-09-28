//! Where a kept failure's moved build tree is, alongside the chroot and the
//! keep-until the earlier migration recorded. Nullable: only a kept failure
//! whose build kept a persistent tree has one.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("builds"))
                    .add_column(ColumnDef::new(Alias::new("kept_tree")).text().null())
                    .to_owned(),
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("builds"))
                    .drop_column(Alias::new("kept_tree"))
                    .to_owned(),
            )
            .await?;
        Ok(())
    }
}
