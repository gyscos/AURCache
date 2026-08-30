//! Download counts for repository files, buffered in memory and flushed
//! periodically.
//!
//! A package repository serves a file per `pacman -Sy`, and a write per request
//! would put a database round trip in the middle of a static file handler. So
//! the handler only increments a map, and a background task folds the map into
//! the table every so often.
//!
//! Reads add the unflushed buffer to the stored value. Without that the number
//! visibly stalls between flushes, which looks like downloads not being
//! counted -- the bug the buffering is meant to be invisible about.
//!
//! What is lost on an unclean shutdown is up to one flush interval of counts.
//! That is the trade the buffer buys, and it is the right one for a figure
//! nobody bills against: an approximate popularity number that costs nothing to
//! serve beats an exact one that costs a write per request.

use crate::helpers::time::now_secs;
use sea_orm::{ConnectionTrait, DbErr, FromQueryResult, Statement};
use std::collections::HashMap;
use std::sync::Mutex;

/// The download counter: the in-memory tally and every way to read it.
///
/// One type rather than a buffer plus a set of free functions taking both it
/// and a connection. The stored count on its own is always the wrong answer --
/// it lags by up to a flush interval -- so no method returns it. Reading is
/// `total_for_packages`, which adds what is still buffered, and there is
/// nothing else to reach for.
///
/// A `std::sync::Mutex` rather than an async one: every critical section here
/// is a map update with no await inside it, so the lock is never held across a
/// yield point.
#[derive(Debug, Default)]
pub struct DownloadCounter {
    pending: Mutex<HashMap<String, i64>>,
}

impl DownloadCounter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one download of `file_name`.
    ///
    /// Infallible on purpose: this is called from the file server, and a
    /// failure to count must never turn into a failure to serve the file.
    pub fn record(&self, file_name: &str) {
        if let Ok(mut pending) = self.pending.lock() {
            *pending.entry(file_name.to_string()).or_insert(0) += 1;
        }
    }

    /// Counts held in memory for every file the predicate accepts.
    #[must_use]
    fn pending_matching(&self, wanted: &dyn Fn(&str) -> bool) -> i64 {
        let Ok(pending) = self.pending.lock() else {
            return 0;
        };
        pending
            .iter()
            .filter(|(name, _)| wanted(name))
            .map(|(_, count)| *count)
            .sum()
    }

    /// Take everything buffered, leaving the buffer empty.
    fn drain(&self) -> HashMap<String, i64> {
        self.pending
            .lock()
            .map(|mut pending| std::mem::take(&mut *pending))
            .unwrap_or_default()
    }

    /// Every download of every file belonging to any of `pkgnames`.
    ///
    /// Stored plus buffered. Matched by parsed package name rather than by
    /// prefix -- a prefix claims `hello-world` for `hello` -- and across all
    /// versions and architectures: the question is how often people have
    /// installed this package, not how often one build of it was fetched.
    pub async fn total_for_packages<C: ConnectionTrait>(
        &self,
        db: &C,
        pkgnames: &[String],
    ) -> Result<i64, DbErr> {
        if pkgnames.is_empty() {
            return Ok(0);
        }
        let wanted = |file_name: &str| {
            pkgname_of(file_name).is_some_and(|name| pkgnames.iter().any(|p| p == name))
        };

        let stmt = Statement::from_string(
            db.get_database_backend(),
            "SELECT file_name, count FROM download_counts".to_string(),
        );
        let stored: i64 = StoredRow::find_by_statement(stmt)
            .all(db)
            .await?
            .into_iter()
            .filter(|row| wanted(&row.file_name))
            .map(|row| row.count)
            .sum();

        Ok(stored + self.pending_matching(&wanted))
    }

    /// Fold the buffer into the table.
    ///
    /// On failure the drained counts are put back rather than dropped, so a
    /// database that is briefly unavailable costs nothing but a delay. They are
    /// added back rather than assigned, because the file server has gone on
    /// counting into the buffer while this was in flight.
    pub async fn flush<C: ConnectionTrait>(&self, db: &C) -> Result<usize, DbErr> {
        let drained = self.drain();
        if drained.is_empty() {
            return Ok(0);
        }

        match write_counts(db, &drained).await {
            Ok(()) => Ok(drained.len()),
            Err(e) => {
                if let Ok(mut pending) = self.pending.lock() {
                    for (name, count) in drained {
                        *pending.entry(name).or_insert(0) += count;
                    }
                }
                Err(e)
            }
        }
    }
}

/// Add each delta to its row, creating the row if this is the file's first
/// download.
///
/// An upsert rather than read-modify-write: two servers against one Postgres
/// would otherwise each read the same value and write back the same sum,
/// losing one of the two.
async fn write_counts<C: ConnectionTrait>(
    db: &C,
    counts: &HashMap<String, i64>,
) -> Result<(), DbErr> {
    let now = now_secs();
    for (file_name, delta) in counts {
        let stmt = Statement::from_sql_and_values(
            db.get_database_backend(),
            "INSERT INTO download_counts (file_name, count, last_download) VALUES ($1, $2, $3) \
             ON CONFLICT (file_name) DO UPDATE SET \
               count = download_counts.count + $2, last_download = $3",
            [file_name.as_str().into(), (*delta).into(), now.into()],
        );
        db.execute(stmt).await?;
    }
    Ok(())
}

/// The package name a repository file belongs to.
///
/// An Arch package file is `<pkgname>-<pkgver>-<pkgrel>-<arch>.pkg.tar.<ext>`.
/// Neither pkgver nor pkgrel may contain a `-`, so the name is everything
/// before the last three fields -- which is the only way to tell `hello` from
/// `hello-world`, since a prefix match claims both.
#[must_use]
fn pkgname_of(file_name: &str) -> Option<&str> {
    let stem = file_name.split(".pkg.tar").next()?;
    // arch, pkgrel, pkgver -- three separators from the right.
    stem.rsplitn(4, '-').nth(3).filter(|name| !name.is_empty())
}

#[derive(FromQueryResult)]
struct StoredRow {
    file_name: String,
    count: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::Migrator;
    use sea_orm::{Database, DatabaseConnection};
    use sea_orm_migration::MigratorTrait;

    async fn setup() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        db
    }

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(ToString::to_string).collect()
    }

    /// What is stored, read straight from the table. The production path
    /// never does this -- reading past the buffer is the thing the counter
    /// exists to prevent -- so it lives here, where "has it been flushed yet"
    /// is the actual question.
    async fn stored(db: &DatabaseConnection, file_name: &str) -> i64 {
        #[derive(FromQueryResult)]
        struct Row {
            count: i64,
        }
        Row::find_by_statement(Statement::from_sql_and_values(
            db.get_database_backend(),
            "SELECT COALESCE(SUM(count), 0) AS count FROM download_counts WHERE file_name = $1",
            [file_name.into()],
        ))
        .one(db)
        .await
        .unwrap()
        .map_or(0, |row| row.count)
    }

    const HELLO: &str = "hello-1.0-1-x86_64.pkg.tar.zst";

    /// The whole point of reading through the buffer: a download must show up
    /// immediately, not at the next flush. Counting only the stored value would
    /// make the number stall for a flush interval, which reads as downloads
    /// going uncounted.
    #[tokio::test]
    async fn a_download_counts_before_it_is_flushed() {
        let db = setup().await;
        let counter = DownloadCounter::new();

        counter.record(HELLO);
        counter.record(HELLO);

        assert_eq!(stored(&db, HELLO).await, 0, "nothing should be written yet");
        assert_eq!(
            counter
                .total_for_packages(&db, &names(&["hello"]))
                .await
                .unwrap(),
            2
        );
    }

    /// And must not be counted twice once it is.
    #[tokio::test]
    async fn flushing_moves_counts_rather_than_copying_them() {
        let db = setup().await;
        let counter = DownloadCounter::new();

        counter.record(HELLO);
        counter.record(HELLO);
        counter.flush(&db).await.unwrap();

        assert_eq!(stored(&db, HELLO).await, 2);
        assert_eq!(
            counter
                .total_for_packages(&db, &names(&["hello"]))
                .await
                .unwrap(),
            2,
            "a flushed count was counted again from the buffer"
        );
    }

    /// A second flush adds to the row rather than replacing it -- the failure
    /// that would silently reset every count on every interval.
    #[tokio::test]
    async fn flushes_accumulate() {
        let db = setup().await;
        let counter = DownloadCounter::new();

        counter.record(HELLO);
        counter.flush(&db).await.unwrap();
        counter.record(HELLO);
        counter.record(HELLO);
        counter.flush(&db).await.unwrap();

        assert_eq!(
            counter
                .total_for_packages(&db, &names(&["hello"]))
                .await
                .unwrap(),
            3
        );
    }

    /// Asking about nothing is not an error, and reaches no table.
    #[tokio::test]
    async fn no_packages_is_no_downloads() {
        let db = setup().await;
        let counter = DownloadCounter::new();
        counter.record(HELLO);
        assert_eq!(counter.total_for_packages(&db, &[]).await.unwrap(), 0);
    }

    /// A prefix match claims `hello-world` for `hello`. The name has to be
    /// parsed, and the format is what makes that possible: pkgver and pkgrel
    /// cannot contain a `-`, so the last three fields are always
    /// version-release-arch.
    #[test]
    fn a_file_names_its_package_exactly() {
        assert_eq!(
            pkgname_of("hello-2.12.1-2-x86_64.pkg.tar.zst"),
            Some("hello")
        );
        assert_eq!(
            pkgname_of("hello-world-1.0-1-x86_64.pkg.tar.zst"),
            Some("hello-world")
        );
        assert_eq!(
            pkgname_of("lib32-glibc-2.39-1-x86_64.pkg.tar.zst"),
            Some("lib32-glibc")
        );
        // A signature belongs to the same package as the file it signs.
        assert_eq!(
            pkgname_of("hello-2.12.1-2-x86_64.pkg.tar.zst.sig"),
            Some("hello")
        );
        assert_eq!(pkgname_of("repo.db"), None);
        assert_eq!(pkgname_of("nonsense.pkg.tar.zst"), None);
    }

    /// The case the parse exists for: `hello-world` must not be counted as
    /// `hello`, and every version and architecture of `hello` must be.
    #[tokio::test]
    async fn a_packages_total_is_its_own_files_only() {
        let db = setup().await;
        let buffer = DownloadCounter::new();

        buffer.record("hello-2.12.1-2-x86_64.pkg.tar.zst");
        buffer.record("hello-2.12.1-2-aarch64.pkg.tar.zst");
        buffer.record("hello-2.13.0-1-x86_64.pkg.tar.zst");
        buffer.record("hello-world-1.0-1-x86_64.pkg.tar.zst");
        buffer.flush(&db).await.unwrap();

        let hello = names(&["hello"]);
        assert_eq!(
            buffer.total_for_packages(&db, &hello).await.unwrap(),
            3,
            "a prefix match would have counted hello-world too"
        );
        assert_eq!(
            buffer
                .total_for_packages(&db, &names(&["hello-world"]))
                .await
                .unwrap(),
            1
        );
    }

    /// Split packages produce several names, and their downloads are one
    /// figure for the package that built them.
    #[tokio::test]
    async fn a_split_packages_names_are_summed_and_read_through_the_buffer() {
        let db = setup().await;
        let buffer = DownloadCounter::new();

        buffer.record("libfoo-1.0-1-x86_64.pkg.tar.zst");
        buffer.flush(&db).await.unwrap();
        // Unflushed, so this also checks the buffer is read here too.
        buffer.record("libfoo-docs-1.0-1-x86_64.pkg.tar.zst");

        let both = names(&["libfoo", "libfoo-docs"]);
        assert_eq!(buffer.total_for_packages(&db, &both).await.unwrap(), 2);
    }

    /// Nothing buffered is not an error, and must not write anything.
    #[tokio::test]
    async fn flushing_an_empty_buffer_does_nothing() {
        let db = setup().await;
        let buffer = DownloadCounter::new();
        assert_eq!(buffer.flush(&db).await.unwrap(), 0);
    }
}
