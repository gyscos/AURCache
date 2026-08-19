use crate::helpers::dbtype::database_type;
use sea_orm::DbBackend;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        match database_type() {
            DbBackend::Sqlite => {
                db.execute_unprepared(
                    r"
CREATE TABLE workers
(
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    cert_fingerprint TEXT NOT NULL UNIQUE,
    cert_serial TEXT,
    signed_cert TEXT,
    not_after BIGINT,
    native_arches TEXT NOT NULL DEFAULT '',
    emulated_arches TEXT NOT NULL DEFAULT '',
    last_seen BIGINT,
    version TEXT
);
",
                )
                .await?;

                db.execute_unprepared(
                    r"
ALTER TABLE builds ADD worker_id INTEGER;
",
                )
                .await?;
                db.execute_unprepared(
                    r"
ALTER TABLE builds ADD lease_expires_at BIGINT;
",
                )
                .await?;
                db.execute_unprepared(
                    r"
ALTER TABLE builds ADD attempt_count INTEGER NOT NULL DEFAULT 0;
",
                )
                .await?;

                // Indexes for the worker hot-path queries: claim scans
                // ENQUEUED builds filtered by (status, platform); heartbeat and
                // the lease reaper filter/renew by worker_id. (workers
                // .cert_fingerprint — looked up on every mTLS request — is
                // already indexed via its UNIQUE constraint.)
                db.execute_unprepared(
                    "CREATE INDEX idx_builds_status_platform ON builds (status, platform);",
                )
                .await?;
                db.execute_unprepared(
                    "CREATE INDEX idx_builds_worker_id ON builds (worker_id);",
                )
                .await?;
            }
            DbBackend::Postgres => {
                db.execute_unprepared(
                    r"
CREATE TABLE public.workers
(
    id SERIAL PRIMARY KEY,
    name TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    cert_fingerprint TEXT NOT NULL UNIQUE,
    cert_serial TEXT,
    signed_cert TEXT,
    not_after BIGINT,
    native_arches TEXT NOT NULL DEFAULT '',
    emulated_arches TEXT NOT NULL DEFAULT '',
    last_seen BIGINT,
    version TEXT
);
",
                )
                .await?;

                db.execute_unprepared(
                    r"
ALTER TABLE builds ADD COLUMN worker_id INTEGER;
",
                )
                .await?;
                db.execute_unprepared(
                    r"
ALTER TABLE builds ADD COLUMN lease_expires_at BIGINT;
",
                )
                .await?;
                db.execute_unprepared(
                    r"
ALTER TABLE builds ADD COLUMN attempt_count INTEGER NOT NULL DEFAULT 0;
",
                )
                .await?;

                db.execute_unprepared(
                    "CREATE INDEX idx_builds_status_platform ON builds (status, platform);",
                )
                .await?;
                db.execute_unprepared(
                    "CREATE INDEX idx_builds_worker_id ON builds (worker_id);",
                )
                .await?;
            }
            _ => Err(DbErr::Migration("Unsupported database type".to_string()))?,
        }

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        match database_type() {
            DbBackend::Sqlite => {
                db.execute_unprepared("DROP INDEX IF EXISTS idx_builds_worker_id;")
                    .await?;
                db.execute_unprepared("DROP INDEX IF EXISTS idx_builds_status_platform;")
                    .await?;
                db.execute_unprepared("ALTER TABLE builds DROP COLUMN attempt_count;")
                    .await?;
                db.execute_unprepared("ALTER TABLE builds DROP COLUMN lease_expires_at;")
                    .await?;
                db.execute_unprepared("ALTER TABLE builds DROP COLUMN worker_id;")
                    .await?;
                db.execute_unprepared("DROP TABLE workers;").await?;
            }
            DbBackend::Postgres => {
                db.execute_unprepared("DROP INDEX IF EXISTS idx_builds_worker_id;")
                    .await?;
                db.execute_unprepared("DROP INDEX IF EXISTS idx_builds_status_platform;")
                    .await?;
                db.execute_unprepared("ALTER TABLE builds DROP COLUMN attempt_count;")
                    .await?;
                db.execute_unprepared("ALTER TABLE builds DROP COLUMN lease_expires_at;")
                    .await?;
                db.execute_unprepared("ALTER TABLE builds DROP COLUMN worker_id;")
                    .await?;
                db.execute_unprepared("DROP TABLE public.workers;").await?;
            }
            _ => Err(DbErr::Migration("Unsupported database type".to_string()))?,
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::migration::Migrator;
    use sea_orm::{ConnectionTrait, Database};
    use sea_orm_migration::MigratorTrait;

    #[tokio::test]
    async fn workers_table_and_build_lease_columns_exist() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

        // workers table with expected columns.
        for col in &[
            "id",
            "name",
            "status",
            "cert_fingerprint",
            "cert_serial",
            "signed_cert",
            "not_after",
            "native_arches",
            "emulated_arches",
            "last_seen",
            "version",
        ] {
            let sql = format!("SELECT {col} FROM workers LIMIT 0");
            db.execute_unprepared(&sql)
                .await
                .unwrap_or_else(|_| panic!("column '{col}' should exist on workers"));
        }

        // builds gained the lease columns.
        for col in &["worker_id", "lease_expires_at", "attempt_count"] {
            let sql = format!("SELECT {col} FROM builds LIMIT 0");
            db.execute_unprepared(&sql)
                .await
                .unwrap_or_else(|_| panic!("column '{col}' should exist on builds"));
        }
    }

    #[tokio::test]
    async fn build_lease_defaults_applied() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

        db.execute_unprepared("INSERT INTO packages (id, name) VALUES (1, 'testpkg');")
            .await
            .unwrap();
        db.execute_unprepared(
            "INSERT INTO builds (id, pkg_id, platform, status) VALUES (1, 1, 'x86_64', 3);",
        )
        .await
        .unwrap();

        // attempt_count defaults to 0; lease columns default to NULL.
        let row = db
            .query_one(sea_orm::Statement::from_string(
                db.get_database_backend(),
                "SELECT attempt_count, worker_id, lease_expires_at FROM builds WHERE id = 1"
                    .to_string(),
            ))
            .await
            .unwrap()
            .expect("row exists");
        let attempt_count: i32 = row.try_get("", "attempt_count").unwrap();
        let worker_id: Option<i32> = row.try_get("", "worker_id").unwrap();
        assert_eq!(attempt_count, 0);
        assert!(worker_id.is_none());
    }

    #[tokio::test]
    async fn worker_status_defaults_to_pending() {        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

        db.execute_unprepared(
            "INSERT INTO workers (id, name, cert_fingerprint) VALUES (1, 'w1', 'fp1');",
        )
        .await
        .unwrap();

        let row = db
            .query_one(sea_orm::Statement::from_string(
                db.get_database_backend(),
                "SELECT status, native_arches FROM workers WHERE id = 1".to_string(),
            ))
            .await
            .unwrap()
            .expect("row exists");
        let status: String = row.try_get("", "status").unwrap();
        let native_arches: String = row.try_get("", "native_arches").unwrap();
        assert_eq!(status, "pending");
        assert_eq!(native_arches, "");
    }

    #[tokio::test]
    async fn hot_path_indexes_exist() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

        for idx in &["idx_builds_status_platform", "idx_builds_worker_id"] {
            let row = db
                .query_one(sea_orm::Statement::from_string(
                    db.get_database_backend(),
                    format!(
                        "SELECT name FROM sqlite_master WHERE type='index' AND name='{idx}'"
                    ),
                ))
                .await
                .unwrap();
            assert!(row.is_some(), "index '{idx}' should exist");
        }
    }
}
