//! Drops `packages.source_type`: it only ever repeated the `type` tag of
//! `source_data`, which every writer set alongside it, so a reader of one
//! could disagree with a reader of the other only through a bug.
//!
//! `down` adds the column back and fills it from `source_data`, through the
//! query builder so it needs no per-backend SQL.

use sea_orm::{ConnectionTrait, FromQueryResult};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum Packages {
    Table,
    Id,
    SourceType,
    SourceData,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Packages::Table)
                    .drop_column(Packages::SourceType)
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Packages::Table)
                    .add_column(
                        ColumnDef::new(Packages::SourceType)
                            .text()
                            .not_null()
                            .default("aur"),
                    )
                    .to_owned(),
            )
            .await?;

        #[derive(FromQueryResult)]
        struct Row {
            id: i32,
            source_data: String,
        }
        let db = manager.get_connection();
        let backend = manager.get_database_backend();
        let rows = Row::find_by_statement(
            backend.build(
                Query::select()
                    .columns([Packages::Id, Packages::SourceData])
                    .from(Packages::Table),
            ),
        )
        .all(db)
        .await?;
        for row in rows {
            // The tag `SourceData` is serialized with, which is the value the
            // column held: `aur`, `git` or `upload`.
            let kind = serde_json::from_str::<serde_json::Value>(&row.source_data)
                .ok()
                .and_then(|data| data.get("type")?.as_str().map(str::to_string))
                .unwrap_or_else(|| "aur".to_string());
            db.execute(
                &Query::update()
                    .table(Packages::Table)
                    .value(Packages::SourceType, kind)
                    .and_where(Expr::col(Packages::Id).eq(row.id))
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
    use sea_orm::{ConnectionTrait, Database, Statement};
    use sea_orm_migration::MigratorTrait;

    /// Down and up again keep what `source_data` says: the column comes back
    /// holding each package's type, and goes again.
    #[tokio::test]
    async fn the_column_comes_back_from_source_data() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        db.execute_unprepared(
            "INSERT INTO packages (name, status, out_of_date, build_flags, platforms, \
             source_data, directly_requested) VALUES \
             ('hello', 0, 0, '', 'x86_64', '{\"type\":\"aur\",\"name\":\"hello\"}', 1), \
             ('tool', 0, 0, '', 'x86_64', \
              '{\"type\":\"git\",\"url\":\"https://example.com/t.git\",\"ref\":\"main\",\"subfolder\":\"\"}', 1)",
        )
        .await
        .unwrap();

        Migrator::down(
            &db,
            Some(crate::migration::steps_back_to(
                "m20261005_000000_drop_source_type",
            )),
        )
        .await
        .unwrap();
        let kinds: Vec<String> = db
            .query_all_raw(Statement::from_string(
                db.get_database_backend(),
                "SELECT source_type FROM packages ORDER BY name",
            ))
            .await
            .unwrap()
            .into_iter()
            .map(|row| row.try_get("", "source_type").unwrap())
            .collect();
        assert_eq!(kinds, ["aur", "git"]);

        Migrator::up(&db, None).await.unwrap();
    }
}
