//! Rewrites a stored auto-update schedule from the seconds-first syntax
//! (`0 0 3 * * *`, Sunday as 1) to crontab's five fields (`0 3 * * *`, Sunday
//! as 0), which is what schedules are read in from now on.
//!
//! A value with no equivalent -- a second other than 0, a restricted year --
//! is left as it is, and the auto-update job reports it as invalid with the
//! reason; so does a value this migration cannot reach, one set through
//! `AUTO_UPDATE_SCHEDULE`.
//!
//! `down` leaves values as they are: one written in the new syntax may use `H`,
//! which the old one has no way to say, and a server that cannot read its
//! schedule reports it rather than running it at the wrong time.

use aurcache_common::schedule::{Schedule, from_seconds_syntax};
use sea_orm::{ConnectionTrait, FromQueryResult};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum Settings {
    Table,
    Id,
    Key,
    Value,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        #[derive(FromQueryResult)]
        struct Row {
            id: i32,
            value: Option<String>,
        }
        let db = manager.get_connection();
        let rows = Row::find_by_statement(
            manager.get_database_backend().build(
                Query::select()
                    .columns([Settings::Id, Settings::Value])
                    .from(Settings::Table)
                    .and_where(Expr::col(Settings::Key).eq("auto_update_interval")),
            ),
        )
        .all(db)
        .await?;
        for row in rows {
            let Some(old) = row.value else { continue };
            if old.trim().is_empty() || Schedule::parse(&old, 0).is_ok() {
                continue;
            }
            let Some(new) = from_seconds_syntax(&old) else {
                continue;
            };
            db.execute(
                &Query::update()
                    .table(Settings::Table)
                    .value(Settings::Value, new)
                    .and_where(Expr::col(Settings::Id).eq(row.id))
                    .to_owned(),
            )
            .await?;
        }
        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::migration::Migrator;
    use sea_orm::{ConnectionTrait, Database, Statement};
    use sea_orm_migration::MigratorTrait;

    /// The stored schedule is translated, weekday renumbered; one with no
    /// equivalent, and other settings, are left alone.
    #[tokio::test]
    async fn a_stored_schedule_moves_to_crontab() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        Migrator::down(
            &db,
            Some(crate::migration::steps_back_to(
                "m20261006_000000_crontab_schedule",
            )),
        )
        .await
        .unwrap();
        db.execute_unprepared(
            "INSERT INTO settings (key, value, pkg_id) VALUES \
             ('auto_update_interval', '0 0 2 * * 1', -1), \
             ('auto_update_interval', '*/30 * * * * *', 7), \
             ('date_format', '0 0 2 * * 1', -1)",
        )
        .await
        .unwrap();

        Migrator::up(&db, None).await.unwrap();
        let values: Vec<String> = db
            .query_all_raw(Statement::from_string(
                db.get_database_backend(),
                "SELECT value FROM settings ORDER BY id",
            ))
            .await
            .unwrap()
            .into_iter()
            .map(|row| row.try_get("", "value").unwrap())
            .collect();
        assert_eq!(values, ["0 2 * * 0", "*/30 * * * * *", "0 0 2 * * 1"]);
    }
}
