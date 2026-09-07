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
