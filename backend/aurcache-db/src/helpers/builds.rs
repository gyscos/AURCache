//! Build-row queries shared across the update, dependency and completion paths.

use crate::builds;
use crate::prelude::Builds;
use aurcache_common::builder::BuildStates;
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
