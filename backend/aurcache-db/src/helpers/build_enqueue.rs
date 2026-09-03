use crate::builds;
use crate::helpers::worker_jobs::{STATUS_ACTIVE, STATUS_ENQUEUED, STATUS_WAITING_FOR_DEPS};
use crate::prelude::Builds;
use pacman_mirrors::platforms::Platform;
use sea_orm::sea_query::{Expr, OnConflict, Query};
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
        let next_number = Expr::cust_with_values(
            "(SELECT COALESCE(MAX(number), 0) + 1 FROM builds WHERE pkg_id = ?)",
            [pkg_id],
        );

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

        let result = db.execute(db.get_database_backend().build(&insert)).await?;

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
