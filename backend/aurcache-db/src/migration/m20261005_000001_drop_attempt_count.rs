//! Drops `builds.attempt_count`, the retry counter of the requeue that put a
//! lost build back in the queue as the same row.
//!
//! Every lost build is abandoned now -- the row failed for good and a fresh row
//! queued in its place -- and the retry budget is derived from the run of
//! `timeout_retry` rows (`builds.trigger`), so nothing reads or writes the
//! counter. See `design/implemented/abort-builds.md`.
//!
//! `down` adds it back at zero: the counts it held are not recoverable, and
//! nothing that would run against that schema reads one it did not write.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum Builds {
    Table,
    AttemptCount,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Builds::Table)
                    .drop_column(Builds::AttemptCount)
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Builds::Table)
                    .add_column(
                        ColumnDef::new(Builds::AttemptCount)
                            .integer()
                            .not_null()
                            .default(0),
                    )
                    .to_owned(),
            )
            .await
    }
}
