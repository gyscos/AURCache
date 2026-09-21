//! Indexes for the log's remaining filters, so they stay cheap as it grows.
//!
//! - `kind`, in the order the log is read: the kind filter, and the lookup of
//!   the last server start that "since the last restart" counts back to, both
//!   scanned the whole table without it.
//! - The entity index gains `log_id`, so "everything about package foo" is
//!   answered from the index alone instead of reading each matching row of
//!   `log_entity` back from the table. It replaces the `(ns, id)` index, which
//!   it covers.
//!
//! Built with sea-query rather than written as SQL per backend: nothing here is
//! backend-specific.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum Log {
    Table,
    Kind,
    Timestamp,
    Id,
}

#[derive(DeriveIden)]
enum LogEntity {
    Table,
    Ns,
    Id,
    LogId,
}

const KIND_INDEX: &str = "idx_log_kind";
const ENTITY_INDEX: &str = "idx_log_entity_ref_log";
const OLD_ENTITY_INDEX: &str = "idx_log_entity_ref";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name(KIND_INDEX)
                    .table(Log::Table)
                    .col(Log::Kind)
                    .col((Log::Timestamp, IndexOrder::Desc))
                    .col((Log::Id, IndexOrder::Desc))
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name(ENTITY_INDEX)
                    .table(LogEntity::Table)
                    .col(LogEntity::Ns)
                    .col(LogEntity::Id)
                    .col(LogEntity::LogId)
                    .to_owned(),
            )
            .await?;
        manager
            .drop_index(
                Index::drop()
                    .if_exists()
                    .name(OLD_ENTITY_INDEX)
                    .table(LogEntity::Table)
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name(OLD_ENTITY_INDEX)
                    .table(LogEntity::Table)
                    .col(LogEntity::Ns)
                    .col(LogEntity::Id)
                    .to_owned(),
            )
            .await?;
        manager
            .drop_index(
                Index::drop()
                    .if_exists()
                    .name(ENTITY_INDEX)
                    .table(LogEntity::Table)
                    .to_owned(),
            )
            .await?;
        manager
            .drop_index(
                Index::drop()
                    .if_exists()
                    .name(KIND_INDEX)
                    .table(Log::Table)
                    .to_owned(),
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use crate::migration::Migrator;
    use sea_orm::{ConnectionTrait, Database, DatabaseConnection, Statement};
    use sea_orm_migration::MigratorTrait;

    async fn indexes(db: &DatabaseConnection) -> Vec<String> {
        db.query_all_raw(Statement::from_string(
            db.get_database_backend(),
            "select name from sqlite_master where type = 'index' and name like 'idx_log%' \
             order by name",
        ))
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.try_get::<String>("", "name").unwrap())
        .collect()
    }

    /// Nothing here runs against Postgres, so the statements it would be sent
    /// are checked as text: the ordering survives, and dropping an index does
    /// not name a table, which Postgres would refuse.
    #[test]
    fn the_postgres_statements_are_what_postgres_accepts() {
        use super::{KIND_INDEX, Log, LogEntity, OLD_ENTITY_INDEX};
        use sea_orm_migration::prelude::*;
        let create = Index::create()
            .if_not_exists()
            .name(KIND_INDEX)
            .table(Log::Table)
            .col(Log::Kind)
            .col((Log::Timestamp, IndexOrder::Desc))
            .col((Log::Id, IndexOrder::Desc))
            .to_string(PostgresQueryBuilder);
        assert_eq!(
            create,
            r#"CREATE INDEX IF NOT EXISTS "idx_log_kind" ON "log" ("kind", "timestamp" DESC, "id" DESC)"#
        );
        let drop = Index::drop()
            .if_exists()
            .name(OLD_ENTITY_INDEX)
            .table(LogEntity::Table)
            .to_string(PostgresQueryBuilder);
        assert_eq!(drop, r#"DROP INDEX IF EXISTS "idx_log_entity_ref""#);
    }

    /// The log's filters each have an index, the entity one covering the
    /// entry id, and the narrower one it replaced is gone -- and a rollback
    /// puts things back as they were.
    #[tokio::test]
    async fn the_log_filters_are_indexed_and_the_rollback_restores_the_old_index() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        assert_eq!(
            indexes(&db).await,
            [
                "idx_log_entity_ref_log",
                "idx_log_kind",
                "idx_log_severity",
                "idx_log_timestamp_id"
            ]
        );

        Migrator::down(&db, Some(1)).await.unwrap();
        assert_eq!(
            indexes(&db).await,
            [
                "idx_log_entity_ref",
                "idx_log_severity",
                "idx_log_timestamp_id"
            ]
        );
    }
}
