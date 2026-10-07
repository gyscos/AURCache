//! Stores the auto-update schedule under `auto_update_schedule`, the name it
//! has everywhere else since it became a crontab rather than an interval.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum Settings {
    Table,
    Key,
}

async fn rename(manager: &SchemaManager<'_>, from: &str, to: &str) -> Result<(), DbErr> {
    manager
        .exec_stmt(
            Query::update()
                .table(Settings::Table)
                .value(Settings::Key, to)
                .and_where(Expr::col(Settings::Key).eq(from))
                .to_owned(),
        )
        .await
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        rename(manager, "auto_update_interval", "auto_update_schedule").await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        rename(manager, "auto_update_schedule", "auto_update_interval").await
    }
}
