//! Gives `files.package_id` a real foreign key.
//!
//! It never had one. `dependencies` cascades on both of its package columns,
//! which is why deleting a package looked safe, but a `files` row could outlive
//! its package with nothing to say so -- and one path did exactly that, leaving
//! rows owned by ids that no longer existed. Ingest then read those rows as
//! "this artifact belongs to another package" and refused to publish, so the
//! package could never be built again. See `package::update`'s orphan collector
//! and `repo_ingest`.
//!
//! `ON DELETE CASCADE`, matching `dependencies`. The artifact on disk and its
//! `repo.db` entry are removed by `package::delete::package_delete`, which
//! deletes the `files` rows itself, before the package row, so the cascade is a
//! backstop rather than the mechanism.
//!
//! Existing violations are deleted first: a row pointing at a package that is
//! gone is the leftover this constraint exists to prevent, and the constraint
//! cannot be added while one is present.

use crate::helpers::dbtype::database_type;
use sea_orm::{ConnectionTrait, DbBackend};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        match database_type() {
            DbBackend::Postgres => {
                db.execute_unprepared(
                    "DELETE FROM files f \
                     WHERE NOT EXISTS (SELECT 1 FROM packages p WHERE p.id = f.package_id);",
                )
                .await?;
                // `ADD CONSTRAINT` has no `IF NOT EXISTS`, and a migration that
                // cannot be run twice is a migration that cannot be recovered
                // from a half-applied state.
                db.execute_unprepared(
                    "DO $$ BEGIN \
                       IF NOT EXISTS ( \
                         SELECT 1 FROM pg_constraint WHERE conname = 'files_package_id_fkey' \
                       ) THEN \
                         ALTER TABLE files ADD CONSTRAINT files_package_id_fkey \
                           FOREIGN KEY (package_id) REFERENCES packages(id) ON DELETE CASCADE; \
                       END IF; \
                     END $$;",
                )
                .await?;
            }
            DbBackend::Sqlite => {
                // SQLite cannot add a constraint to an existing table, so the
                // table is rebuilt. `package_id` becomes NOT NULL at the same
                // time, which is what Postgres has said since the columns were
                // merged out of `packages_files`; a NULL there is an artifact
                // belonging to no package, so it goes with the orphans.
                //
                // Dropping the table drops its indexes with it, hence the
                // `CREATE INDEX` at the end -- the same one
                // `m20260831_000001_files_package_id_index` made.
                db.execute_unprepared(
                    "PRAGMA foreign_keys = OFF;
                     DELETE FROM files \
                       WHERE package_id IS NULL \
                          OR package_id NOT IN (SELECT id FROM packages);
                     CREATE TABLE files_new (
                        filename TEXT NOT NULL UNIQUE,
                        id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
                        platform TEXT,
                        package_id INTEGER NOT NULL
                            REFERENCES packages(id) ON DELETE CASCADE,
                        size BIGINT
                     );
                     INSERT INTO files_new (filename, id, platform, package_id, size)
                        SELECT filename, id, platform, package_id, size FROM files;
                     DROP TABLE files;
                     ALTER TABLE files_new RENAME TO files;
                     CREATE INDEX IF NOT EXISTS idx_files_package_id ON files (package_id);
                     PRAGMA foreign_keys = ON;",
                )
                .await?;
            }
            _ => return Err(DbErr::Migration("Unsupported database type".to_string())),
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        match database_type() {
            DbBackend::Postgres => {
                db.execute_unprepared(
                    "ALTER TABLE files DROP CONSTRAINT IF EXISTS files_package_id_fkey;",
                )
                .await?;
            }
            DbBackend::Sqlite => {
                db.execute_unprepared(
                    "PRAGMA foreign_keys = OFF;
                     CREATE TABLE files_old (
                        filename TEXT NOT NULL UNIQUE,
                        id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
                        platform TEXT,
                        package_id INTEGER NULL,
                        size BIGINT
                     );
                     INSERT INTO files_old (filename, id, platform, package_id, size)
                        SELECT filename, id, platform, package_id, size FROM files;
                     DROP TABLE files;
                     ALTER TABLE files_old RENAME TO files;
                     CREATE INDEX IF NOT EXISTS idx_files_package_id ON files (package_id);
                     PRAGMA foreign_keys = ON;",
                )
                .await?;
            }
            _ => return Err(DbErr::Migration("Unsupported database type".to_string())),
        }
        Ok(())
    }
}
