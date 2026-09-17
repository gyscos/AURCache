//! Which case of its kind an entry is.
//!
//! Kinds are consolidated where an operator would not filter on the
//! difference -- refreshing a git remote and refreshing the snapshot cache are
//! both `source.refresh_failed` -- but the difference is still worth keeping.
//! `subkind` is that detail: `git` or `snapshot`, beside the kind rather than
//! buried in the payload, so it can be filtered on without reading JSON.
//!
//! A column rather than a payload field for the same reason `log_entity` is a
//! table: everything a query needs is a column, and a query that read into
//! `data` would be `data->>'…'`, which sea-query only offers as a Postgres
//! operator.
//!
//! NULL for a kind that has only one case, which is most of them.
//!
//! A separate migration from the one that creates the table because that one
//! may already have been applied to a development database.

use crate::helpers::dbtype::database_type;
use sea_orm::DbBackend;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        let statements: [&str; 2] = match database_type() {
            DbBackend::Sqlite => [
                "alter table log add subkind TEXT;",
                // Paged in the same order as everything else, so a filter on
                // one case of a kind reads the index rather than the table.
                "create index idx_log_kind_subkind on log (kind, subkind, timestamp desc, id desc);",
            ],
            DbBackend::Postgres => [
                "ALTER TABLE log ADD COLUMN subkind TEXT;",
                "CREATE INDEX idx_log_kind_subkind ON log (kind, subkind, timestamp DESC, id DESC);",
            ],
            _ => return Err(DbErr::Migration("Unsupported database type".to_string())),
        };
        for sql in statements {
            db.execute_unprepared(sql).await?;
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        for sql in [
            "drop index if exists idx_log_kind_subkind;",
            "alter table log drop column subkind;",
        ] {
            db.execute_unprepared(sql).await?;
        }
        Ok(())
    }
}
