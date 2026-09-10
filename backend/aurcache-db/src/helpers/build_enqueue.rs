use crate::builds;
use crate::helpers::worker_jobs::{STATUS_ACTIVE, STATUS_ENQUEUED, STATUS_WAITING_FOR_DEPS};
use crate::prelude::Builds;
use pacman_mirrors::platforms::Platform;
use sea_orm::sea_query::{Expr, ExprTrait, Func, OnConflict, Query};
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, ConnectionTrait, DbErr, EntityTrait,
    IntoActiveModel, QueryFilter,
};

pub struct EnqueueBuildResult {
    pub build: builds::Model,
    pub inserted: bool,
}

// See the race explanation in `enqueue_build_if_missing`.
const NUMBER_RACE_ATTEMPTS: usize = 5;

/// `(SELECT COALESCE(MAX(number), 0) + 1 FROM builds WHERE pkg_id = <pkg_id>)`.
///
/// Assembled through the query builder rather than written as a raw fragment.
/// The raw form spelled the bound value `?`, which sea-query only substitutes
/// for backends whose placeholder *is* `?`: SQLite got `pkg_id = 7`, Postgres
/// got the marker verbatim, and every enqueue there failed with `syntax error
/// at or near ")"`. Nothing caught it because the tests all run on SQLite --
/// hence the test below, which renders both backends.
fn next_build_number_expr(pkg_id: i32) -> Expr {
    Expr::from(
        Query::select()
            .expr(Func::coalesce([Expr::col(builds::Column::Number).max(), Expr::val(0)]).add(1))
            .from(builds::Entity)
            .and_where(Expr::col(builds::Column::PkgId).eq(pkg_id))
            .to_owned(),
    )
}

/// Insert a new pending build with the given `initial_status` if no pending build already exists
/// for `(pkg_id, platform)`.
///
/// `initial_status` must be one of [`STATUS_ENQUEUED`] or [`STATUS_WAITING_FOR_DEPS`].
/// The partial unique index on `builds(pkg_id, platform)` covering all pending
/// states (ACTIVE, ENQUEUED, WAITING_FOR_DEPS) ensures at most one pending row
/// per `(pkg_id, platform)` at any time.
///
/// If a pending build already exists the insert is skipped (`inserted = false`) and the existing
/// row is returned, regardless of its status.
pub async fn enqueue_build_if_missing<C: ConnectionTrait>(
    db: &C,
    pkg_id: i32,
    platform: Platform,
    version: &str,
    start_time: i64,
    initial_status: i32,
) -> Result<EnqueueBuildResult, DbErr> {
    let platform_str = platform.as_str();

    // Two conflicts can stop this insert, and they mean opposite things.
    //
    // `(pkg_id, platform)` means a pending build already exists — the intended
    // outcome, and the row we then return.
    //
    // `(pkg_id, number)` means another insert for the same package took this
    // number between our subquery and our write. Nothing is wrong with *this*
    // build; it simply needs the next number. Without the retry that race
    // surfaces as "Missing pending build row", because `DO NOTHING` swallowed
    // an insert that should have happened.
    for _ in 0..NUMBER_RACE_ATTEMPTS {
        // Read inside the statement rather than in a separate query, so the
        // window a competing insert can land in is as small as the database
        // allows.
        let next_number = next_build_number_expr(pkg_id);

        let insert = Query::insert()
            .into_table(builds::Entity)
            .columns([
                builds::Column::PkgId,
                builds::Column::Status,
                builds::Column::StartTime,
                builds::Column::Platform,
                builds::Column::Version,
                builds::Column::Number,
            ])
            .values([
                pkg_id.into(),
                initial_status.into(),
                start_time.into(),
                platform_str.to_owned().into(),
                version.to_owned().into(),
                next_number,
            ])
            .map_err(|e| DbErr::Custom(e.to_string()))?
            .on_conflict(OnConflict::new().do_nothing().to_owned())
            .to_owned();

        let result = db.execute(&insert).await?;

        let existing = Builds::find()
            .filter(builds::Column::PkgId.eq(pkg_id))
            .filter(builds::Column::Platform.eq(platform_str))
            .filter(builds::Column::Status.is_in([
                Some(STATUS_ACTIVE),
                Some(STATUS_ENQUEUED),
                Some(STATUS_WAITING_FOR_DEPS),
            ]))
            .one(db)
            .await?;

        if let Some(build) = existing {
            return Ok(EnqueueBuildResult {
                build,
                inserted: result.rows_affected() == 1,
            });
        }
        // Nothing inserted and nothing pending: the number was taken. Try again
        // with a freshly read maximum.
    }

    Err(DbErr::Custom(format!(
        "Could not assign a build number for package {pkg_id} on platform {platform_str} \
         after {NUMBER_RACE_ATTEMPTS} attempts"
    )))
}

/// Promote an existing `WAITING_FOR_DEPS` build to `ENQUEUED` so that it can be started.
///
/// Returns the updated build row if a `WAITING_FOR_DEPS` build was found and promoted,
/// or `None` if no such build exists (e.g. the build was never queued or was already promoted).
pub async fn promote_waiting_build<C: ConnectionTrait>(
    db: &C,
    pkg_id: i32,
    platform: Platform,
) -> Result<Option<builds::Model>, DbErr> {
    let Some(build) = Builds::find()
        .filter(builds::Column::PkgId.eq(pkg_id))
        .filter(builds::Column::Platform.eq(platform.as_str()))
        .filter(builds::Column::Status.eq(Some(STATUS_WAITING_FOR_DEPS)))
        .one(db)
        .await?
    else {
        return Ok(None);
    };

    let mut active = build.into_active_model();
    active.status = Set(Some(STATUS_ENQUEUED));
    let updated = active.update(db).await?;
    Ok(Some(updated))
}

/// Put an `ENQUEUED` build back to `WAITING_FOR_DEPS`, because something it
/// needs is no longer ready.
///
/// The counterpart of [`promote_waiting_build`], and the reason a package's
/// dependencies can be repointed while a build for it is already queued: the
/// queue entry was made against the old dependency, and the new one may not be
/// built yet.
///
/// A conditional `UPDATE` rather than the read-then-write `promote_waiting_build`
/// does, because the race runs the other way here. A queued build is exactly
/// what a worker is looking for, so one can be claimed (`ENQUEUED -> ACTIVE`)
/// between a read and a write -- and demoting it then would take a build a
/// worker is already running and put it back in the queue. Pinning the update to
/// `status = ENQUEUED` makes that a no-op instead.
///
/// Returns whether a build was demoted.
pub async fn demote_enqueued_build<C: ConnectionTrait>(
    db: &C,
    pkg_id: i32,
    platform: Platform,
) -> Result<bool, DbErr> {
    let res = Builds::update_many()
        .col_expr(builds::Column::Status, Expr::value(STATUS_WAITING_FOR_DEPS))
        .filter(builds::Column::PkgId.eq(pkg_id))
        .filter(builds::Column::Platform.eq(platform.as_str()))
        .filter(builds::Column::Status.eq(Some(STATUS_ENQUEUED)))
        .exec(db)
        .await?;
    Ok(res.rows_affected > 0)
}

#[cfg(test)]
mod build_number_tests {
    use super::next_build_number_expr;
    use sea_orm::DatabaseBackend;
    use sea_orm::sea_query::Query;

    /// Every supported backend has to bind the package id. A backend that
    /// renders a bare `?` is emitting a placeholder the driver will not fill.
    #[test]
    fn next_build_number_binds_the_package_id_on_every_backend() {
        let select = Query::select().expr(next_build_number_expr(7)).to_owned();

        for backend in [DatabaseBackend::Sqlite, DatabaseBackend::Postgres] {
            let sql = backend.build(&select).to_string();
            assert!(
                sql.contains('7'),
                "{backend:?} dropped the package id: {sql}"
            );
            assert!(
                !sql.contains('?'),
                "{backend:?} left an unsubstituted placeholder: {sql}"
            );
        }
    }
}
