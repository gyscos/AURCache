//! A failed build's chroot, kept on its worker for inspection: where it is
//! and until when. Nullable: only a failed build on a worker with keeping on
//! (`WORKER_KEEP_FAILED`) has one, and the worker removes it early when the
//! pool needs the room.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // One column per statement: SQLite alters a table one change at a time.
        manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("builds"))
                    .add_column(ColumnDef::new(Alias::new("kept_path")).text().null())
                    .to_owned(),
            )
            .await?;
        manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("builds"))
                    .add_column(
                        ColumnDef::new(Alias::new("kept_until"))
                            .big_integer()
                            .null(),
                    )
                    .to_owned(),
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        for column in ["kept_path", "kept_until"] {
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
