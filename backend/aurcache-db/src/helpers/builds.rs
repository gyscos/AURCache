//! Build-row queries shared across the update, dependency and completion paths.

use crate::prelude::Builds;
use crate::{builds, packages};
use aurcache_common::builder::BuildStates;
use sea_orm::sea_query::{Alias, Expr, ExprTrait, Func, Query};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DbErr, EntityTrait, Order, QueryFilter, QueryOrder, QuerySelect,
    Select,
};

/// The most recent *successful* build row selection, newest by end time with
/// start time as the tie-break.
fn newest_success_query(pkg_id: i32) -> Select<Builds> {
    Builds::find()
        .select_only()
        .column(builds::Column::Version)
        .filter(builds::Column::PkgId.eq(pkg_id))
        .filter(builds::Column::Status.eq(Some(BuildStates::SUCCESSFUL_BUILD)))
        .order_by(builds::Column::EndTime, Order::Desc)
        .order_by(builds::Column::StartTime, Order::Desc)
        .limit(1)
}

/// The version of the most recently *successful* build of `pkg_id` on
/// `platform`.
///
/// `None` when no such build exists. Used to decide whether a dependency is
/// already satisfied by an existing build, which both the completion path
/// (`aurcache_utils::worker_complete`) and the update path
/// (`aurcache_utils::package::update`) ask — kept in one place so their
/// ordering cannot diverge.
pub async fn latest_successful_version<C: ConnectionTrait>(
    db: &C,
    pkg_id: i32,
    platform: &str,
) -> Result<Option<String>, DbErr> {
    newest_success_query(pkg_id)
        .filter(builds::Column::Platform.eq(platform))
        .into_tuple::<(String,)>()
        .one(db)
        .await
        .map(|row| row.map(|(version,)| version))
}

/// The version of the most recently *successful* build of `pkg_id` across all
/// platforms.
///
/// Same shape as [`latest_successful_version`], but the package page is not
/// scoped to one platform, so the newest success over any of them stands for
/// the package.
pub async fn latest_successful_version_any_platform<C: ConnectionTrait>(
    db: &C,
    pkg_id: i32,
) -> Result<Option<String>, DbErr> {
    newest_success_query(pkg_id)
        .into_tuple::<(String,)>()
        .one(db)
        .await
        .map(|row| row.map(|(version,)| version))
}

/// The repository version of the package row in the *enclosing* query, as a
/// correlated scalar subquery.
///
/// The same "successful builds only" rule as [`latest_successful_version_any_platform`],
/// expressed as a column so that a package listing reports it for every row
/// without one query per package.
///
/// Ordered by `COALESCE(end_time, start_time)` rather than by the two columns
/// in turn: a finished build ranks by when it ended and an unfinished one by
/// when it started, which is what "most recent" has to mean where the two are
/// mixed. `NULLIF` because `builds.version` is NOT NULL DEFAULT '': a build
/// that has been enqueued but has not determined a version yet holds an empty
/// string, which means "not known", not "the empty version".
#[must_use]
pub fn latest_successful_version_expr() -> Expr {
    Expr::from(
        Query::select()
            .expr(Func::cust(Alias::new("NULLIF")).args([
                (Expr::col((builds::Entity, builds::Column::Version))),
                Expr::val(""),
            ]))
            .from(builds::Entity)
            .and_where(
                Expr::col((builds::Entity, builds::Column::PkgId))
                    .equals((packages::Entity, packages::Column::Id)),
            )
            .and_where(
                Expr::col((builds::Entity, builds::Column::Status))
                    .eq(BuildStates::SUCCESSFUL_BUILD),
            )
            .order_by_expr(
                Func::coalesce([
                    (Expr::col((builds::Entity, builds::Column::EndTime))),
                    Expr::col((builds::Entity, builds::Column::StartTime)),
                ])
                .into(),
                Order::Desc,
            )
            .limit(1)
            .to_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::latest_successful_version_any_platform;
    use crate::migration::Migrator;
    use aurcache_common::builder::BuildStates;
    use sea_orm::{ConnectionTrait, Database, DatabaseConnection};
    use sea_orm_migration::MigratorTrait;

    async fn setup() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        db.execute_unprepared("INSERT INTO packages (id, name) VALUES (1, 'cargo-diet')")
            .await
            .unwrap();
        db
    }

    async fn build(db: &DatabaseConnection, number: i32, status: i32, version: &str, start: i64) {
        db.execute_unprepared(&format!(
            "INSERT INTO builds (pkg_id, number, status, start_time, platform, version) \
             VALUES (1, {number}, {status}, {start}, 'x86_64', '{version}')"
        ))
        .await
        .unwrap();
    }

    /// A failed build says nothing about what is in the repository.
    ///
    /// This is what made an out-of-date package unbuildable: upstream moved to
    /// 1.4.1-1, the build of it failed, and the update path -- which asked for
    /// the latest build of *any* outcome -- refused every retry with "already
    /// up to date (version 1.4.1-1)" about a version nothing had ever built.
    #[tokio::test]
    async fn a_failed_build_is_not_a_built_version() {
        let db = setup().await;
        build(&db, 6, BuildStates::FAILED_BUILD, "1.4.1-1", 100).await;

        assert_eq!(
            latest_successful_version_any_platform(&db, 1)
                .await
                .unwrap(),
            None
        );
    }

    /// And a failed *newer* attempt does not hide the older success: the
    /// repository still serves what that success produced.
    #[tokio::test]
    async fn a_later_failure_does_not_hide_an_earlier_success() {
        let db = setup().await;
        build(&db, 5, BuildStates::SUCCESSFUL_BUILD, "1.4.0-1", 100).await;
        build(&db, 6, BuildStates::FAILED_BUILD, "1.4.1-1", 200).await;

        assert_eq!(
            latest_successful_version_any_platform(&db, 1)
                .await
                .unwrap()
                .as_deref(),
            Some("1.4.0-1")
        );
    }

    /// A success at the version being asked for is what should stop a
    /// redundant rebuild, and still does.
    #[tokio::test]
    async fn a_successful_build_reports_its_version() {
        let db = setup().await;
        build(&db, 6, BuildStates::SUCCESSFUL_BUILD, "1.4.1-1", 100).await;

        assert_eq!(
            latest_successful_version_any_platform(&db, 1)
                .await
                .unwrap()
                .as_deref(),
            Some("1.4.1-1")
        );
    }
}
