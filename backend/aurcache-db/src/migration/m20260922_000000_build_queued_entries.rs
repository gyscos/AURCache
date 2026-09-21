//! Rewrites the log's build requests into `build.queued`, with their cause.
//!
//! They were `package.updated` with a `forced` flag, and `build.retried` for a
//! retry from a build page. "Forced" was the auto-updater's way past the
//! version check for VCS packages as much as a person asking for a rebuild, so
//! the entries said "forced update" for what were updates. Each becomes one
//! kind with the cause spelled out:
//!
//! - not forced, or forced by the server itself: an **update**;
//! - forced by a person: a **retry** if the package's last build before it had
//!   failed, a **rebuild** otherwise (the entry never said which, and the build
//!   history does);
//! - `build.retried`: a **retry**, naming the build it repeated.
//!
//! The server acting on its own was credited to a user called "Server" by the
//! log this one replaced; those entries lose it, as the server's own entries
//! carry no user. Each rewritten entry's index rows are rewritten to match its
//! new payload.

use crate::prelude::{Builds, LogEntities, Logs, Packages};
use crate::{builds, log_entities, logs, packages};
use aurcache_common::builder::BuildStates;
use sea_orm::ActiveValue::Set;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, EntityTrait, IntoActiveModel, QueryFilter, QueryOrder,
    QuerySelect,
};
use sea_orm_migration::prelude::*;
use serde_json::{Value, json};

#[derive(DeriveMigrationName)]
pub struct Migration;

const SERVER_USER: &str = "Server";

/// The cause of one old entry, and the build it names if any.
struct Rewrite {
    pkgbase: String,
    cause: &'static str,
    build: Option<(String, i32)>,
}

impl Rewrite {
    /// What `Event::BuildQueued` says for the same payload. The old entries
    /// never named the build they queued, only the one a retry repeated.
    fn message(&self) -> String {
        let cause = match (&self.build, self.cause) {
            (Some((pkgbase, number)), "retry") => format!("retry of {pkgbase} #{number}"),
            (_, cause) => cause.to_string(),
        };
        format!("queued a build of {} ({cause})", self.pkgbase)
    }

    fn data(&self) -> Value {
        let mut data = json!({ "pkg": format!("pkg:{}", self.pkgbase), "cause": self.cause });
        if let Some((pkgbase, number)) = &self.build {
            data["retried"] = json!(format!("build:{pkgbase}/{number}"));
        }
        data
    }

    /// The index rows the new payload implies: the package, and a build filed
    /// under its package as well, in the role it plays.
    fn index(&self, log_id: i32) -> Vec<log_entities::ActiveModel> {
        let row = |role: &str, ns: &str, id: String| log_entities::ActiveModel {
            log_id: Set(log_id),
            role: Set(role.to_string()),
            ns: Set(ns.to_string()),
            id: Set(id),
        };
        let mut rows = vec![row("pkg", "pkg", self.pkgbase.clone())];
        if let Some((pkgbase, number)) = &self.build {
            rows.push(row("retried", "pkg", pkgbase.clone()));
            rows.push(row("retried", "build", format!("{pkgbase}/{number}")));
        }
        rows
    }
}

/// `pkg:hello` → `hello`, `build:hello/7` → `(hello, 7)`.
fn package_of(value: &Value) -> Option<String> {
    value.as_str()?.strip_prefix("pkg:").map(str::to_string)
}

fn build_of(value: &Value) -> Option<(String, i32)> {
    let (pkgbase, number) = value.as_str()?.strip_prefix("build:")?.rsplit_once('/')?;
    Some((pkgbase.to_string(), number.parse().ok()?))
}

/// Whether the package's last build started before `at` failed. A package
/// with no such build -- or gone altogether -- reads as not failed, so a
/// person's forced request becomes a rebuild.
async fn failed_before<C: ConnectionTrait>(db: &C, pkgbase: &str, at: i64) -> Result<bool, DbErr> {
    let Some((pkg_id,)) = Packages::find()
        .select_only()
        .column(packages::Column::Id)
        .filter(packages::Column::Name.eq(pkgbase))
        .into_tuple::<(i32,)>()
        .one(db)
        .await?
    else {
        return Ok(false);
    };
    let status = Builds::find()
        .select_only()
        .column(builds::Column::Status)
        .filter(builds::Column::PkgId.eq(pkg_id))
        .filter(builds::Column::StartTime.lt(at))
        .order_by_desc(builds::Column::StartTime)
        .into_tuple::<(Option<i32>,)>()
        .one(db)
        .await?;
    Ok(matches!(status, Some((Some(BuildStates::FAILED_BUILD),))))
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        let rows = Logs::find()
            .filter(logs::Column::Kind.is_in(["package.updated", "build.retried"]))
            .all(db)
            .await?;
        for row in rows {
            let Ok(data) = serde_json::from_str::<Value>(&row.data) else {
                continue;
            };
            let user = row.user.clone().filter(|user| user != SERVER_USER);
            let rewrite = if row.kind == "build.retried" {
                let Some(build) = build_of(&data["build"]) else {
                    continue;
                };
                Rewrite {
                    pkgbase: build.0.clone(),
                    cause: "retry",
                    build: Some(build),
                }
            } else {
                let Some(pkgbase) = package_of(&data["pkg"]) else {
                    continue;
                };
                let forced = data["forced"].as_bool().unwrap_or(false);
                let cause = if !forced || user.is_none() {
                    "update"
                } else if failed_before(db, &pkgbase, row.timestamp).await? {
                    "retry"
                } else {
                    "rebuild"
                };
                Rewrite {
                    pkgbase,
                    cause,
                    build: None,
                }
            };

            let id = row.id;
            let mut active = row.into_active_model();
            active.kind = Set("build.queued".to_string());
            active.message = Set(rewrite.message());
            active.data = Set(rewrite.data().to_string());
            active.user = Set(user);
            active.update(db).await?;
            LogEntities::delete_many()
                .filter(log_entities::Column::LogId.eq(id))
                .exec(db)
                .await?;
            LogEntities::insert_many(rewrite.index(id)).exec(db).await?;
        }
        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        // What `forced` said cannot be told from the cause it became, and the
        // old kinds are not written any more: nothing to go back to.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::migration::Migrator;
    use crate::prelude::{LogEntities, Logs};
    use crate::{builds, log_entities, logs, packages};
    use aurcache_common::builder::BuildStates;
    use aurcache_common::source::SourceData;
    use pacman_mirrors::platforms::Platform;
    use sea_orm::ActiveValue::Set;
    use sea_orm::{
        ActiveModelTrait, ColumnTrait, Database, DatabaseConnection, EntityTrait, QueryFilter,
    };
    use sea_orm_migration::MigratorTrait;

    async fn entry(db: &DatabaseConnection, kind: &str, data: &str, user: Option<&str>, at: i64) {
        logs::ActiveModel {
            kind: Set(kind.to_string()),
            severity: Set(aurcache_common::api::activity::Severity::Info),
            message: Set("forced update of package hello".to_string()),
            data: Set(data.to_string()),
            timestamp: Set(at),
            user: Set(user.map(str::to_string)),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap();
    }

    /// Every old shape becomes `build.queued` with the cause it really was:
    /// the server's forced updates are updates and lose the "Server" user, a
    /// person's is a retry after a failed build and a rebuild otherwise, and a
    /// build-page retry names its build and is indexed under it.
    #[tokio::test]
    async fn old_build_requests_become_queued_builds_with_their_cause() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        // Everything up to just before this migration.
        Migrator::up(&db, None).await.unwrap();
        Migrator::down(
            &db,
            Some(crate::migration::steps_back_to(
                "m20260922_000000_build_queued_entries",
            )),
        )
        .await
        .unwrap();

        let pkg = packages::ActiveModel {
            name: Set("hello".to_string()),
            status: Set(BuildStates::FAILED_BUILD),
            out_of_date: Set(0),
            build_flags: Set(String::new()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(crate::packages::SourceType::Aur),
            source_data: Set(SourceData::Aur {
                name: "hello".into(),
            }),
            directly_requested: Set(true),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();
        builds::ActiveModel {
            pkg_id: Set(pkg.id),
            number: Set(1),
            status: Set(Some(BuildStates::SUCCESSFUL_BUILD)),
            start_time: Set(Some(100)),
            platform: Set(Platform::X86_64),
            version: Set("1-1".to_string()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();
        builds::ActiveModel {
            pkg_id: Set(pkg.id),
            number: Set(2),
            status: Set(Some(BuildStates::FAILED_BUILD)),
            start_time: Set(Some(300)),
            platform: Set(Platform::X86_64),
            version: Set("1-1".to_string()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();

        let forced = r#"{"pkg":"pkg:hello","forced":true}"#;
        entry(&db, "package.updated", forced, Some("Server"), 200).await;
        entry(&db, "package.updated", forced, Some("alice"), 200).await;
        entry(&db, "package.updated", forced, Some("alice"), 400).await;
        entry(
            &db,
            "package.updated",
            r#"{"pkg":"pkg:hello","forced":false}"#,
            Some("alice"),
            500,
        )
        .await;
        entry(
            &db,
            "build.retried",
            r#"{"build":"build:hello/2"}"#,
            Some("alice"),
            600,
        )
        .await;

        Migrator::up(&db, None).await.unwrap();

        let rows = Logs::find().all(&db).await.unwrap();
        assert!(rows.iter().all(|r| r.kind == "build.queued"));
        let said: Vec<(Option<&str>, &str)> = rows
            .iter()
            .map(|r| (r.user.as_deref(), r.message.as_str()))
            .collect();
        assert_eq!(
            said,
            [
                (None, "queued a build of hello (update)"),
                // The build before it (#1) succeeded.
                (Some("alice"), "queued a build of hello (rebuild)"),
                // The build before it (#2) failed.
                (Some("alice"), "queued a build of hello (retry)"),
                (Some("alice"), "queued a build of hello (update)"),
                (Some("alice"), "queued a build of hello (retry of hello #2)"),
            ]
        );

        let retried = rows.last().unwrap();
        let index: Vec<(String, String, String)> = LogEntities::find()
            .filter(log_entities::Column::LogId.eq(retried.id))
            .all(&db)
            .await
            .unwrap()
            .into_iter()
            .map(|r| (r.role, r.ns, r.id))
            .collect();
        assert_eq!(index.len(), 3, "{index:?}");
        assert!(index.contains(&(
            "retried".to_string(),
            "build".to_string(),
            "hello/2".to_string()
        )));
        assert!(index.contains(&("pkg".to_string(), "pkg".to_string(), "hello".to_string())));
    }
}
