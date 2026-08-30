use crate::helpers::dbtype::database_type;
use crate::migration::Migrator;
use anyhow::{anyhow, bail};
use sea_orm::{ConnectOptions, ConnectionTrait, Database, DatabaseConnection, DbBackend};
use sea_orm_migration::MigratorTrait;
use std::time::Duration;
use std::{env, fs};
use tracing::log::LevelFilter;

/// Read a required Postgres env var, with a message naming it.
fn required_env(name: &str) -> anyhow::Result<String> {
    env::var(name).map_err(|_| anyhow!("No {name} envvar specified"))
}

pub async fn init_db() -> anyhow::Result<DatabaseConnection> {
    let db: DatabaseConnection = match database_type() {
        DbBackend::Sqlite => {
            fs::create_dir_all("./db")?;

            let db_name = env::var("DB_NAME").unwrap_or_else(|_| "db.sqlite".to_string());

            let mut conn_opts = ConnectOptions::new(format!("sqlite://db/{db_name}?mode=rwc"));
            conn_opts
                .max_connections(1)
                .min_connections(1)
                .acquire_timeout(Duration::from_secs(30))
                .sqlx_logging_level(LevelFilter::Trace);
            let db = Database::connect(conn_opts).await?;
            db.execute_unprepared("
                PRAGMA journal_mode = WAL;          -- read/write concurrency; persistent on the db file
                PRAGMA synchronous = NORMAL;        -- fsync at WAL checkpoint, not every write
                PRAGMA busy_timeout = 5000;         -- wait up to 5s for the write lock before SQLITE_BUSY
                PRAGMA wal_autocheckpoint = 1000;   -- checkpoint every 1000 pages (~1MB WAL)
                PRAGMA wal_checkpoint(TRUNCATE);    -- truncate any massive WAL left by a previous run
            ").await?;
            db
        }
        DbBackend::Postgres => {
            let db_user = required_env("DB_USER")?;
            let db_pwd = required_env("DB_PWD")?;
            let db_host = required_env("DB_HOST")?;
            let db_name = env::var("DB_NAME").unwrap_or_else(|_| "postgres".to_string());

            let conn_str = format!("postgres://{db_user}:{db_pwd}@{db_host}/{db_name}");
            let mut conn_opts = ConnectOptions::new(conn_str);
            conn_opts.sqlx_logging_level(LevelFilter::Trace);
            Database::connect(conn_opts).await?
        }
        _ => bail!("Unsupported database type"),
    };

    Migrator::up(&db, None).await?;
    Ok(db)
}
