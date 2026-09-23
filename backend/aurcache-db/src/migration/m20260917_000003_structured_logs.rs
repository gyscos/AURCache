//! The structured operation log, and the index of what each entry refers to.
//!
//! `log` is the entry: a stable `kind`, the severity that kind carries, the
//! sentence rendered when it was written, and the typed payload it was rendered
//! from. `scope` is the entity the entry was emitted *under* -- a build, a
//! version-check pass -- when there was one.
//!
//! `log_entity` is the index that makes "everything about package foo"
//! answerable. One row per entity an entry refers to, written from the payload
//! at insert: `role` is the payload key, `ns`/`id` the reference it held. A role
//! naming several entities gets a row each, which is why `role` is part of the
//! key rather than unique on its own.
//!
//! A side table rather than a JSON column with a Postgres GIN index, because
//! this way the filter is one ordinary query that sea-orm builds the same for
//! both backends. Everything here is tested against SQLite and nothing is
//! against Postgres, so a backend-specific query would be one whose tested
//! branch is not the branch that ships. See `design/implemented/structured-logs.md`.
//!
//! `severity` is the number its variants are ordered by, so "this severity and
//! worse" is `severity >= n`.

use crate::helpers::dbtype::database_type;
use sea_orm::DbBackend;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        let statements: Vec<&str> = match database_type() {
            DbBackend::Sqlite => vec![
                "create table log (
                     id integer not null primary key autoincrement,
                     kind TEXT not null,
                     severity integer not null,
                     message TEXT not null,
                     data TEXT not null,
                     scope TEXT,
                     timestamp INTEGER not null,
                     user TEXT
                 );",
                // The order the log is read in, newest first, with the id
                // breaking ties between entries written in the same second.
                "create index idx_log_timestamp_id on log (timestamp desc, id desc);",
                // Narrowing by severity is the common filter, and it pages in
                // the same order.
                "create index idx_log_severity on log (severity, timestamp desc, id desc);",
                "create table log_entity (
                     log_id integer not null references log (id) on delete cascade,
                     role TEXT not null,
                     ns TEXT not null,
                     id TEXT not null,
                     primary key (log_id, role, ns, id)
                 );",
                // The filter: every entry naming this entity, whatever role it
                // played there.
                "create index idx_log_entity_ref on log_entity (ns, id);",
            ],
            DbBackend::Postgres => vec![
                r#"CREATE TABLE log (
                     id SERIAL PRIMARY KEY,
                     kind TEXT NOT NULL,
                     severity INTEGER NOT NULL,
                     message TEXT NOT NULL,
                     data TEXT NOT NULL,
                     scope TEXT,
                     timestamp BIGINT NOT NULL,
                     "user" TEXT
                 );"#,
                "CREATE INDEX idx_log_timestamp_id ON log (timestamp DESC, id DESC);",
                "CREATE INDEX idx_log_severity ON log (severity, timestamp DESC, id DESC);",
                r#"CREATE TABLE log_entity (
                     log_id INTEGER NOT NULL REFERENCES log (id) ON DELETE CASCADE,
                     role TEXT NOT NULL,
                     ns TEXT NOT NULL,
                     id TEXT NOT NULL,
                     PRIMARY KEY (log_id, role, ns, id)
                 );"#,
                "CREATE INDEX idx_log_entity_ref ON log_entity (ns, id);",
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
            "DROP TABLE IF EXISTS log_entity;",
            "DROP TABLE IF EXISTS log;",
        ] {
            db.execute_unprepared(sql).await?;
        }
        Ok(())
    }
}
