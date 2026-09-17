//! Build-row queries shared across the update, dependency and completion paths.

use crate::prelude::Builds;
use crate::{builds, packages};
use aurcache_common::builder::BuildStates;
use sea_orm::sea_query::{Alias, Expr, ExprTrait, Func, Query};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DbErr, EntityTrait, Order, QueryFilter, QueryOrder, QuerySelect,
    Select,
};
use std::collections::BTreeMap;

/// The most recent *successful* build row selection, newest by end time with
/// start time as the tie-break, projecting one column.
///
/// The column is a parameter rather than a second `select_only` at the call
/// site: stacking `select_only` reads as resetting the projection, and the
/// next reader *will* "fix" it into returning two columns for a one-tuple.
fn newest_success_query(pkg_id: i32, column: builds::Column) -> Select<Builds> {
    Builds::find()
        .select_only()
        .column(column)
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
    newest_success_query(pkg_id, builds::Column::Version)
        .filter(builds::Column::Platform.eq(platform))
        .into_tuple::<(String,)>()
        .one(db)
        .await
        .map(|row| row.map(|(version,)| version))
}

/// Whether `dependee_id`'s newest successful build on `platform` satisfies
/// `constraint`.
///
/// The one answer to "is this dependency ready?". Two paths ask it — a build
/// finishing and deciding whether to promote its dependents, and a build being
/// queued and deciding whether it may start — and they used to run separate
/// queries that ordered differently: one by `end_time` alone, the other by
/// `end_time` with `start_time` as the tie-break. A build with no recorded end
/// time could therefore be picked by one and not the other, so the same
/// dependency read as ready to the queue and not ready to the builder.
pub async fn dependency_satisfied<C: ConnectionTrait>(
    db: &C,
    dependee_id: i32,
    platform: &str,
    constraint: &str,
) -> Result<bool, DbErr> {
    Ok(latest_successful_version(db, dependee_id, platform)
        .await?
        .is_some_and(|version| aurcache_deps::satisfies_constraint(&version, constraint)))
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
    newest_success_query(pkg_id, builds::Column::Version)
        .into_tuple::<(String,)>()
        .one(db)
        .await
        .map(|row| row.map(|(version,)| version))
}

/// What the most recently *successful* build of `pkg_id` was made from:
/// `source_url -> commit`, empty when nothing is recorded.
///
/// The baseline for "has upstream moved since we built it?". It has to be a
/// successful build: one that failed at a commit says nothing about what is in
/// the repository, and treating it as the baseline would leave the package
/// quietly sitting at a commit nothing ever produced -- the same trap
/// [`latest_successful_version`] exists to avoid for versions.
///
/// Empty means *unknown*, never "unchanged": a package built before this was
/// recorded, or whose sources could not be resolved, falls back to the version
/// check's own watermark rather than being declared up to date. Unparseable
/// JSON is treated the same way, since a stored string nobody can read is not
/// evidence either.
pub async fn latest_successful_build_vcs_sources<C: ConnectionTrait>(
    db: &C,
    pkg_id: i32,
) -> Result<BTreeMap<String, String>, DbErr> {
    let recorded = newest_success_query(pkg_id, builds::Column::VcsSources)
        .into_tuple::<Option<String>>()
        .one(db)
        .await?
        .flatten();
    let Some(json) = recorded else {
        return Ok(BTreeMap::new());
    };
    Ok(serde_json::from_str(&json).unwrap_or_else(|e| {
        tracing::warn!("build of package {pkg_id} has unreadable vcs_sources: {e}");
        BTreeMap::new()
    }))
}

/// Record what a build's VCS sources were at, replacing whatever was there --
/// the worker's report of what it checked out overwrites the guess made when
/// the build was queued.
///
/// An empty set clears the column rather than storing `{}`: "no sources" and
/// "sources unknown" both mean there is nothing to compare against, and one
/// spelling for that is enough.
pub async fn record_build_vcs_sources<C: ConnectionTrait>(
    db: &C,
    build_id: i32,
    commits: &BTreeMap<String, String>,
) -> Result<(), DbErr> {
    let json = if commits.is_empty() {
        None
    } else {
        Some(serde_json::to_string(commits).map_err(|e| DbErr::Custom(e.to_string()))?)
    };
    Builds::update_many()
        .col_expr(builds::Column::VcsSources, json.into())
        .filter(builds::Column::Id.eq(build_id))
        .exec(db)
        .await?;
    Ok(())
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
    use super::{
        latest_successful_build_vcs_sources, latest_successful_version_any_platform,
        record_build_vcs_sources,
    };
    use crate::migration::Migrator;
    use aurcache_common::builder::BuildStates;
    use sea_orm::{
        ColumnTrait, ConnectionTrait, Database, DatabaseConnection, EntityTrait, QueryFilter,
        QuerySelect,
    };
    use sea_orm_migration::MigratorTrait;
    use std::collections::BTreeMap;

    async fn setup() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        db.execute_unprepared("INSERT INTO packages (id, name) VALUES (1, 'cargo-diet')")
            .await
            .unwrap();
        db
    }

    /// One tracked source, spelled as `.SRCINFO` would.
    fn url() -> String {
        "git+https://example.test/repo.git".to_string()
    }

    /// A recorded set, in the shape the column holds.
    fn sources(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    /// The row id behind a build number, which is what the rows hang off.
    async fn build_id(db: &DatabaseConnection, number: i32) -> i32 {
        crate::prelude::Builds::find()
            .select_only()
            .column(crate::builds::Column::Id)
            .filter(crate::builds::Column::Number.eq(number))
            .into_tuple::<i32>()
            .one(db)
            .await
            .unwrap()
            .unwrap()
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

    /// The *successful* build is the baseline: what a failed attempt was made
    /// from is not what is in the repository.
    #[tokio::test]
    async fn the_baseline_is_what_the_last_successful_build_was_made_from() {
        let db = setup().await;
        build(&db, 5, BuildStates::SUCCESSFUL_BUILD, "r1.aaa-1", 100).await;
        build(&db, 6, BuildStates::FAILED_BUILD, "r2.bbb-1", 200).await;
        let (success, failure) = (build_id(&db, 5).await, build_id(&db, 6).await);

        record_build_vcs_sources(&db, success, &sources(&[(&url(), "aaa")]))
            .await
            .unwrap();
        record_build_vcs_sources(&db, failure, &sources(&[(&url(), "bbb")]))
            .await
            .unwrap();

        assert_eq!(
            latest_successful_build_vcs_sources(&db, 1).await.unwrap(),
            sources(&[(&url(), "aaa")]),
            "the failed attempt's commit must not become the baseline"
        );
    }

    /// Nothing recorded is *unknown*, not "unchanged" — every build predates
    /// this column, and reading NULL as up-to-date would freeze them all.
    #[tokio::test]
    async fn a_build_with_no_record_reports_nothing() {
        let db = setup().await;
        build(&db, 5, BuildStates::SUCCESSFUL_BUILD, "r1.aaa-1", 100).await;

        assert!(
            latest_successful_build_vcs_sources(&db, 1)
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// A PKGBUILD can carry several `git+` sources, and re-recording replaces
    /// the set — the worker's report of what it checked out overwrites the
    /// guess made when the build was queued.
    #[tokio::test]
    async fn several_sources_are_recorded_and_re_recording_replaces() {
        let db = setup().await;
        build(&db, 5, BuildStates::SUCCESSFUL_BUILD, "r1.aaa-1", 100).await;
        let id = build_id(&db, 5).await;

        let two = sources(&[
            ("git+https://example.test/a.git", "a1"),
            ("git+https://example.test/b.git", "b1"),
        ]);
        record_build_vcs_sources(&db, id, &two).await.unwrap();
        assert_eq!(
            latest_successful_build_vcs_sources(&db, 1).await.unwrap(),
            two
        );

        let one = sources(&[("git+https://example.test/a.git", "a2")]);
        record_build_vcs_sources(&db, id, &one).await.unwrap();
        assert_eq!(
            latest_successful_build_vcs_sources(&db, 1).await.unwrap(),
            one,
            "re-recording replaces the set rather than merging into it"
        );
    }

    /// An empty set clears the column instead of storing `{}`: "no sources" and
    /// "sources unknown" are the same answer to the only question asked of it.
    #[tokio::test]
    async fn recording_nothing_clears_the_record() {
        let db = setup().await;
        build(&db, 5, BuildStates::SUCCESSFUL_BUILD, "r1.aaa-1", 100).await;
        let id = build_id(&db, 5).await;
        record_build_vcs_sources(&db, id, &sources(&[(&url(), "aaa")]))
            .await
            .unwrap();

        record_build_vcs_sources(&db, id, &BTreeMap::new())
            .await
            .unwrap();

        assert!(
            latest_successful_build_vcs_sources(&db, 1)
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// A stored string nobody can parse is not evidence of anything, so it
    /// reads as unknown rather than taking the version check down with it.
    #[tokio::test]
    async fn unreadable_json_reads_as_unknown() {
        let db = setup().await;
        build(&db, 5, BuildStates::SUCCESSFUL_BUILD, "r1.aaa-1", 100).await;
        let id = build_id(&db, 5).await;
        db.execute_unprepared(&format!(
            "UPDATE builds SET vcs_sources = 'not json' WHERE id = {id}"
        ))
        .await
        .unwrap();

        assert!(
            latest_successful_build_vcs_sources(&db, 1)
                .await
                .unwrap()
                .is_empty()
        );
    }
}
