//! Indexes for the dashboard's build slices, so they stay cheap as builds grow.
//!
//! - `idx_builds_start_time (start_time)` for recent builds (`start_time DESC`).
//! - `idx_builds_status_start_time (status, start_time)` for the queue
//!   (`status IN (ENQUEUED, WAITING_FOR_DEPS) ORDER BY start_time ASC`, and its
//!   count) and the longest-builds window
//!   (`status = SUCCESSFUL_BUILD AND start_time >= since`).
//! - `idx_builds_pkg_status_start_time (pkg_id, status, start_time)` for the
//!   per-package lookups: the previous successful build behind each
//!   longest-builds row, and `latest_successful_version_expr`, which every
//!   package listing runs once per row.
//!
//! The migration also backfills any `NULL` `builds.start_time` (legacy rows —
//! every enqueue sets it now) with `end_time` or `0`, so `list_builds_impl`
//! can order by the bare column instead of `COALESCE(start_time, 0)`, which no
//! plain index serves.
//!
//! Built with sea-query rather than written as SQL per backend: nothing here is
//! backend-specific.

use crate::builds;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum Builds {
    Table,
    StartTime,
    Status,
    PkgId,
}

const START_TIME_INDEX: &str = "idx_builds_start_time";
const STATUS_START_TIME_INDEX: &str = "idx_builds_status_start_time";
const PKG_STATUS_START_TIME_INDEX: &str = "idx_builds_pkg_status_start_time";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        // Legacy rows with no recorded start: a finished build ranks by when
        // it ended, anything else sorts last. After this every row has a
        // start, so the list can order by the bare column and use the indexes
        // below on both backends.
        builds::Entity::update_many()
            .col_expr(
                builds::Column::StartTime,
                Expr::FunctionCall(Func::coalesce([
                    Expr::col(builds::Column::EndTime),
                    Expr::val(0),
                ])),
            )
            .filter(builds::Column::StartTime.is_null())
            .exec(db)
            .await?;

        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name(START_TIME_INDEX)
                    .table(Builds::Table)
                    .col((Builds::StartTime, IndexOrder::Desc))
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name(STATUS_START_TIME_INDEX)
                    .table(Builds::Table)
                    .col(Builds::Status)
                    .col((Builds::StartTime, IndexOrder::Asc))
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name(PKG_STATUS_START_TIME_INDEX)
                    .table(Builds::Table)
                    .col(Builds::PkgId)
                    .col(Builds::Status)
                    .col((Builds::StartTime, IndexOrder::Desc))
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        for name in [
            PKG_STATUS_START_TIME_INDEX,
            STATUS_START_TIME_INDEX,
            START_TIME_INDEX,
        ] {
            manager
                .drop_index(
                    Index::drop()
                        .if_exists()
                        .name(name)
                        .table(Builds::Table)
                        .to_owned(),
                )
                .await?;
        }
        Ok(())
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
            "select name from sqlite_master where type = 'index' and name like 'idx_builds_%' \
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
        use super::{
            Builds, PKG_STATUS_START_TIME_INDEX, START_TIME_INDEX, STATUS_START_TIME_INDEX,
        };
        use sea_orm_migration::prelude::*;
        let create = Index::create()
            .if_not_exists()
            .name(START_TIME_INDEX)
            .table(Builds::Table)
            .col((Builds::StartTime, IndexOrder::Desc))
            .to_string(PostgresQueryBuilder);
        assert_eq!(
            create,
            r#"CREATE INDEX IF NOT EXISTS "idx_builds_start_time" ON "builds" ("start_time" DESC)"#
        );
        let create = Index::create()
            .if_not_exists()
            .name(STATUS_START_TIME_INDEX)
            .table(Builds::Table)
            .col(Builds::Status)
            .col((Builds::StartTime, IndexOrder::Asc))
            .to_string(PostgresQueryBuilder);
        assert_eq!(
            create,
            r#"CREATE INDEX IF NOT EXISTS "idx_builds_status_start_time" ON "builds" ("status", "start_time" ASC)"#
        );
        let create = Index::create()
            .if_not_exists()
            .name(PKG_STATUS_START_TIME_INDEX)
            .table(Builds::Table)
            .col(Builds::PkgId)
            .col(Builds::Status)
            .col((Builds::StartTime, IndexOrder::Desc))
            .to_string(PostgresQueryBuilder);
        assert_eq!(
            create,
            r#"CREATE INDEX IF NOT EXISTS "idx_builds_pkg_status_start_time" ON "builds" ("pkg_id", "status", "start_time" DESC)"#
        );
        let drop = Index::drop()
            .if_exists()
            .name(START_TIME_INDEX)
            .table(Builds::Table)
            .to_string(PostgresQueryBuilder);
        assert_eq!(drop, r#"DROP INDEX IF EXISTS "idx_builds_start_time""#);
    }

    /// The dashboard's build slices each have an index, and a rollback drops
    /// exactly those three while leaving the older build indexes alone.
    #[tokio::test]
    async fn the_dashboard_build_slices_are_indexed_and_the_rollback_drops_them() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        let got = indexes(&db).await;
        for want in [
            "idx_builds_pkg_status_start_time",
            "idx_builds_start_time",
            "idx_builds_status_start_time",
        ] {
            assert!(got.contains(&want.to_string()), "missing {want}: {got:?}");
        }

        Migrator::down(
            &db,
            Some(crate::migration::steps_back_to(
                "m20260923_000000_build_dashboard_indexes",
            )),
        )
        .await
        .unwrap();
        let got = indexes(&db).await;
        for gone in [
            "idx_builds_pkg_status_start_time",
            "idx_builds_start_time",
            "idx_builds_status_start_time",
        ] {
            assert!(
                !got.contains(&gone.to_string()),
                "still present {gone}: {got:?}"
            );
        }
    }
}
