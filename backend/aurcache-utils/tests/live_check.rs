//! What a removal collects, and what it leaves alone.

use aurcache_db::migration::Migrator;
use aurcache_db::packages::{self, SourceData, SourceType};
use aurcache_db::{dependencies, prelude::Dependencies};
use aurcache_utils::package::live_check::package_remove;
use sea_orm::{ActiveModelTrait, Database, DatabaseConnection, EntityTrait, PaginatorTrait, Set};
use sea_orm_migration::MigratorTrait;

async fn memory_db() -> DatabaseConnection {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    Migrator::up(&db, None).await.unwrap();
    db
}

async fn package(db: &DatabaseConnection, name: &str, directly_requested: bool) -> i32 {
    packages::ActiveModel {
        name: Set(name.to_string()),
        status: Set(3),
        out_of_date: Set(0),
        upstream_version: Set(None),
        latest_build: Set(None),
        build_flags: Set(String::new()),
        platforms: Set("x86_64".to_string()),
        source_type: Set(SourceType::Aur),
        source_data: Set(SourceData::Aur { name: name.into() }),
        directly_requested: Set(directly_requested),
        split_packages: Set(None),
        ..Default::default()
    }
    .save(db)
    .await
    .unwrap()
    .id
    .unwrap()
}

async fn needs(db: &DatabaseConnection, dependent: i32, dependee: i32) {
    dependencies::ActiveModel {
        dependent_id: Set(dependent),
        dependee_id: Set(dependee),
        version_constraint: Set(String::new()),
        ..Default::default()
    }
    .save(db)
    .await
    .unwrap();
}

async fn remaining(db: &DatabaseConnection) -> Vec<String> {
    let mut names: Vec<String> = packages::Entity::find()
        .all(db)
        .await
        .unwrap()
        .into_iter()
        .map(|package| package.name)
        .collect();
    names.sort();
    names
}

/// The ordinary case: removing the requested package takes the chain of
/// dependencies below it, and the links along with them.
#[tokio::test]
async fn removing_a_root_collects_the_chain_under_it() {
    let db = memory_db().await;
    let root = package(&db, "root", true).await;
    let middle = package(&db, "middle", false).await;
    let leaf = package(&db, "leaf", false).await;
    needs(&db, root, middle).await;
    needs(&db, middle, leaf).await;

    package_remove(&db, root).await.unwrap();

    assert!(remaining(&db).await.is_empty());
    assert_eq!(Dependencies::find().count(&db).await.unwrap(), 0);
}

/// A dependency two packages share outlives the first of them.
#[tokio::test]
async fn a_shared_dependency_waits_for_its_last_dependent() {
    let db = memory_db().await;
    let first = package(&db, "first", true).await;
    let second = package(&db, "second", true).await;
    let shared = package(&db, "shared", false).await;
    needs(&db, first, shared).await;
    needs(&db, second, shared).await;

    package_remove(&db, first).await.unwrap();
    assert_eq!(remaining(&db).await, ["second", "shared"]);

    package_remove(&db, second).await.unwrap();
    assert!(remaining(&db).await.is_empty());
}

/// Two dependencies that need each other keep each other alive under a
/// per-package "does anything depend on me" rule, and used to be left behind
/// with nothing above them. Rare in the AUR, but permanent when it happened.
#[tokio::test]
async fn a_dependency_cycle_is_collected_with_its_root() {
    let db = memory_db().await;
    let root = package(&db, "root", true).await;
    let left = package(&db, "left", false).await;
    let right = package(&db, "right", false).await;
    needs(&db, root, left).await;
    needs(&db, left, right).await;
    needs(&db, right, left).await;

    package_remove(&db, root).await.unwrap();

    assert!(remaining(&db).await.is_empty());
}

/// A cycle that a requested package still needs stays put: reachability, not
/// "is it in a cycle", is what decides.
#[tokio::test]
async fn a_cycle_something_still_needs_is_kept() {
    let db = memory_db().await;
    let leaving = package(&db, "leaving", true).await;
    let staying = package(&db, "staying", true).await;
    let left = package(&db, "left", false).await;
    let right = package(&db, "right", false).await;
    needs(&db, leaving, left).await;
    needs(&db, staying, left).await;
    needs(&db, left, right).await;
    needs(&db, right, left).await;

    package_remove(&db, leaving).await.unwrap();

    assert_eq!(remaining(&db).await, ["left", "right", "staying"]);
}

/// Nothing outside what the removed package reached is touched, even when it
/// is unreferenced itself — an add that has inserted a package but not yet
/// linked it up is not this removal's to collect.
#[tokio::test]
async fn an_unrelated_unlinked_package_is_left_alone() {
    let db = memory_db().await;
    let root = package(&db, "root", true).await;
    let dependency = package(&db, "dependency", false).await;
    needs(&db, root, dependency).await;
    package(&db, "in-flight", false).await;

    package_remove(&db, root).await.unwrap();

    assert_eq!(remaining(&db).await, ["in-flight"]);
}
