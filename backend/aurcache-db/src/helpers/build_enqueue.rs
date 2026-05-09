use crate::builds;
use crate::prelude::Builds;
use sea_orm::sea_query::{OnConflict, Query};
use sea_orm::{ColumnTrait, ConnectionTrait, DbErr, EntityTrait, QueryFilter};

const ACTIVE_BUILD_STATUS: i32 = 0;
const ENQUEUED_BUILD_STATUS: i32 = 3;

pub struct EnqueueBuildResult {
    pub build: builds::Model,
    pub inserted: bool,
}

pub async fn enqueue_build_if_missing<C: ConnectionTrait>(
    db: &C,
    pkg_id: i32,
    platform: &str,
    version: &str,
    start_time: i64,
) -> Result<EnqueueBuildResult, DbErr> {
    // This helper relies on the partial unique index created by
    // m20260514_000000_build_enqueue_dedupe to guarantee there is at most one
    // pending build row per `(pkg_id, platform)` across ACTIVE/ENQUEUED states.
    let insert = Query::insert()
        .into_table(builds::Entity)
        .columns([
            builds::Column::PkgId,
            builds::Column::Status,
            builds::Column::StartTime,
            builds::Column::Platform,
            builds::Column::Version,
        ])
        .values([
            pkg_id.into(),
            ENQUEUED_BUILD_STATUS.into(),
            start_time.into(),
            platform.to_owned().into(),
            version.to_owned().into(),
        ])
        .map_err(|e| DbErr::Custom(e.to_string()))?
        .on_conflict(OnConflict::new().do_nothing().to_owned())
        .to_owned();

    let result = db.execute(db.get_database_backend().build(&insert)).await?;

    let build = Builds::find()
        .filter(builds::Column::PkgId.eq(pkg_id))
        .filter(builds::Column::Platform.eq(platform))
        .filter(
            builds::Column::Status
                .is_in(vec![Some(ACTIVE_BUILD_STATUS), Some(ENQUEUED_BUILD_STATUS)]),
        )
        .one(db)
        .await?
        .ok_or_else(|| {
            DbErr::Custom(format!(
                "Missing pending build row for package {pkg_id} on platform {platform}"
            ))
        })?;

    Ok(EnqueueBuildResult {
        build,
        inserted: result.rows_affected() == 1,
    })
}
