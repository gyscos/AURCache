//! Moves build logs out of `builds.output` and into files on disk.
//!
//! The column grew by `SET output = output || $1`, which reads as an O(1)
//! append and is not one: under MVCC the whole value is detoasted,
//! decompressed, concatenated, recompressed and written as a new row version.
//! Measured on a 30 MB value, twenty appends took 3.7 s against 0.3 ms for
//! twenty appends to an empty value, and a real build flushes every 1.5 s for
//! its whole duration -- so storing a 31 MB log cost gigabytes of writes and
//! left as much again for VACUUM.
//!
//! Files append in O(new bytes) and are read back by seeking, which is also
//! what makes byte offsets usable end to end: slicing `text` by codepoints has
//! to decode UTF-8 from the start, and measured 6x slower than slicing bytes.
//!
//! One-time and destructive by design: the rows are written out and the column
//! is dropped, with no read fallback afterwards. A build whose file is missing
//! renders as "no log" rather than failing.

use aurcache_common::fs::build_log_path;
use sea_orm::{ConnectionTrait, DbBackend, Statement};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

/// Rows are copied one at a time rather than in one query: a single instance
/// here held 31 MB in one row, so selecting every log at once would hold the
/// whole history in memory to write it straight back out again.
async fn backfill(db: &SchemaManagerConnection<'_>) -> Result<usize, DbErr> {
    let backend = db.get_database_backend();

    // Joined to `packages` because logs are named after a build's public
    // identity, `<pkgbase>/<number>`, not its row id.
    let rows = db
        .query_all(Statement::from_string(
            backend,
            "SELECT b.id AS id, b.number AS number, p.name AS pkgbase \
             FROM builds b JOIN packages p ON p.id = b.pkg_id \
             WHERE b.output IS NOT NULL AND b.output <> ''",
        ))
        .await?;

    if rows.is_empty() {
        return Ok(0);
    }

    let mut written = 0usize;
    for row in rows {
        let id = row.try_get::<i32>("", "id")?;
        let number = row.try_get::<i32>("", "number")?;
        let pkgbase = row.try_get::<String>("", "pkgbase")?;

        // One row at a time: a single instance here held 31 MB in one row, so
        // selecting every log at once would hold the whole history in memory
        // just to write it straight back out.
        let Some(value) = db
            .query_one(Statement::from_sql_and_values(
                backend,
                "SELECT output FROM builds WHERE id = $1",
                [id.into()],
            ))
            .await?
        else {
            continue;
        };
        let output: Option<String> = value.try_get("", "output")?;
        let Some(output) = output else { continue };

        let path = build_log_path(&pkgbase, number);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                DbErr::Migration(format!("cannot create {}: {e}", parent.display()))
            })?;
        }
        // Not skipped when the file already exists: a half-finished migration
        // that wrote some files and failed before dropping the column should
        // converge on the column, which is still the truth until it goes away.
        std::fs::write(&path, output.as_bytes())
            .map_err(|e| DbErr::Migration(format!("cannot write {}: {e}", path.display())))?;
        written += 1;
    }

    Ok(written)
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        let written = backfill(db).await?;
        tracing::info!("moved {written} build log(s) out of the database");

        let sql = match db.get_database_backend() {
            DbBackend::Sqlite => "alter table builds drop column output;",
            DbBackend::Postgres => "ALTER TABLE builds DROP COLUMN output;",
            _ => return Err(DbErr::Migration("Unsupported database type".to_string())),
        };
        db.execute_unprepared(sql).await?;
        Ok(())
    }

    /// Restores the column and reads back whatever files are still there.
    ///
    /// Logs whose files have since been deleted cannot come back; the column is
    /// nullable and they end up NULL, which is what it already meant for a
    /// build that produced no output.
    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        let backend = db.get_database_backend();

        let sql = match backend {
            DbBackend::Sqlite => "alter table builds add output text;",
            DbBackend::Postgres => "ALTER TABLE builds ADD COLUMN output TEXT;",
            _ => return Err(DbErr::Migration("Unsupported database type".to_string())),
        };
        db.execute_unprepared(sql).await?;

        let rows = db
            .query_all(Statement::from_string(
                backend,
                "SELECT b.id AS id, b.number AS number, p.name AS pkgbase \
                 FROM builds b JOIN packages p ON p.id = b.pkg_id",
            ))
            .await?;

        for row in rows {
            let id = row.try_get::<i32>("", "id")?;
            let number = row.try_get::<i32>("", "number")?;
            let pkgbase = row.try_get::<String>("", "pkgbase")?;
            let Ok(text) = std::fs::read_to_string(build_log_path(&pkgbase, number)) else {
                continue;
            };
            db.execute(Statement::from_sql_and_values(
                backend,
                "UPDATE builds SET output = $1 WHERE id = $2",
                [text.into(), id.into()],
            ))
            .await?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::Migrator;
    use sea_orm::{Database, DatabaseConnection};
    use sea_orm_migration::MigratorTrait;

    /// Migrate up to, but not including, this migration, so a row can be
    /// planted in the schema as it stood before the move.
    ///
    /// Found by name rather than as "one before the end": this used to assume
    /// it was the last migration registered, so the next migration added after
    /// it silently ran this one too, and the test then failed inserting into a
    /// column this migration had just dropped.
    async fn db_before_this_migration() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        let this = Migration.name();
        let before = Migrator::migrations()
            .iter()
            .position(|m| m.name() == this)
            .expect("this migration is registered with the migrator");
        // Fully qualified: sea_query also defines a `try_from` on u32.
        let upto = <u32 as TryFrom<usize>>::try_from(before).unwrap();
        Migrator::up(&db, Some(upto)).await.unwrap();
        db
    }

    /// The whole point of the migration: a log in the column has to end up in a
    /// file. A backfill that quietly did nothing would drop every log an
    /// instance had, and the column is gone afterwards, so there is no second
    /// chance.
    #[tokio::test]
    async fn backfill_writes_the_column_out_to_files() {
        let root = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("AURCACHE_BUILD_LOG_PATH", root.path()) };

        let db = db_before_this_migration().await;
        db.execute_unprepared("INSERT INTO packages (id, name) VALUES (1, 'p1')")
            .await
            .unwrap();
        db.execute_unprepared(
            "INSERT INTO builds (id, pkg_id, number, platform, version, output)
             VALUES (1, 1, 1, 'x86_64', '1.0', 'first line\nsecond line\n'),
                    (2, 1, 2, 'x86_64', '1.0', NULL),
                    (3, 1, 3, 'x86_64', '1.0', '')",
        )
        .await
        .unwrap();

        Migrator::up(&db, None).await.unwrap();

        assert_eq!(
            std::fs::read_to_string(root.path().join("p1/1.log")).unwrap(),
            "first line\nsecond line\n"
        );
        // Named after the build's public identity, under the package's
        // directory, matching what the API and the CLI use.
        //
        // A build that logged nothing gets no file rather than an empty one:
        // "no log" and "an empty log" read the same to anyone looking.
        assert!(!root.path().join("p1/2.log").exists());
        assert!(!root.path().join("p1/3.log").exists());

        // And the column is gone, so nothing can keep writing to it.
        assert!(
            db.execute_unprepared("SELECT output FROM builds")
                .await
                .is_err()
        );
    }
}
