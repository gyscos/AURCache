//! The public API keys packages by pkgbase, and a pkgbase may contain `+`.
//!
//! 187 of the 161,057 packages in the AUR have one — `a+`, `aewm++`,
//! `antlr3-c++-devel`. That character is the reason these routes take the
//! pkgbase as a **path segment** rather than a query parameter: Rocket decodes
//! a path with `percent_decode_lossy` (where `+` is literal) but decodes a
//! query value with `url_decode`, which replaces `+` with a space first. A
//! `?pkgbase=aewm++` filter therefore looks up `aewm  ` and quietly finds
//! nothing.
//!
//! These tests pin that down, because it is invisible until someone adds one of
//! those packages.

use std::sync::Arc;

use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_db::action::Action;
use aurcache_db::builds;
use aurcache_db::dependencies;
use aurcache_db::migration::Migrator;
use aurcache_db::packages;
use aurcache_db::packages::{SourceData, SourceType};
use aurcache_db::prelude::{Builds, Dependencies, Packages};
use aurcache_types::build_state::BuildStates;
use aurcache_utils::snapshot::SnapshotStore;
use pacman_mirrors::platforms::Platform;
use rocket::http::Status;
use rocket::local::asynchronous::Client;
use rocket::tokio::sync::broadcast;
use sea_orm::ActiveModelTrait;
use sea_orm::ActiveValue::Set;
use sea_orm::{Database, DatabaseConnection, EntityTrait};
use sea_orm_migration::MigratorTrait;

/// Names that exercise every character the AUR actually uses, plus the two
/// fully-numeric names that exist (`1337`, `67`) — those are why the routes
/// cannot accept an id *or* a name and guess between them.
const NAMES: [&str; 6] = [
    "aewm++",
    "a+",
    "antlr3-c++-devel",
    "2048.c",
    "gtk_theme-git",
    "1337",
];

async fn insert_build(db: &DatabaseConnection, pkg_id: i32, status: i32, version: &str) {
    Builds::insert(builds::ActiveModel {
        pkg_id: Set(pkg_id),
        status: Set(Some(status)),
        platform: Set(Platform::X86_64),
        version: Set(version.to_string()),
        start_time: Set(Some(100)),
        end_time: Set(Some(200)),
        ..Default::default()
    })
    .exec(db)
    .await
    .expect("insert build");
}

async fn seed(db: &DatabaseConnection, name: &str) -> i32 {
    let model = packages::ActiveModel {
        name: Set(name.to_string()),
        status: Set(0),
        out_of_date: Set(0),
        // `package::add` always records this, and `SimplePackage` types it as a
        // plain String — a row without it makes the list route fail to decode.
        upstream_version: Set(Some("1.0-1".to_string())),
        build_flags: Set(String::new()),
        platforms: Set("x86_64".to_string()),
        source_type: Set(SourceType::Aur),
        source_data: Set(SourceData::Aur {
            name: name.to_string(),
        }),
        directly_requested: Set(true),
        ..Default::default()
    };
    Packages::insert(model)
        .exec(db)
        .await
        .expect("insert package")
        .last_insert_id
}

/// Mounts the *real* route table, so the test exercises the same URL matching
/// and decoding production does rather than a reduced stand-in. That means
/// supplying every piece of state the mounted routes declare, even the ones
/// these tests never call — Rocket verifies that up front.
async fn test_client() -> (Client, DatabaseConnection) {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    Migrator::up(&db, None).await.unwrap();

    let checkouts = tempfile::tempdir().expect("tempdir");
    let rocket = rocket::build()
        .manage(db.clone())
        .manage(ActivityLog::new(db.clone()))
        .manage(broadcast::channel::<Action>(16).0)
        .manage(Arc::new(SnapshotStore::with_checkout_root(
            checkouts.path().to_path_buf(),
        )))
        .mount("/api", aurcache_api::backend::build_api());
    // The checkout root only has to outlive Rocket's construction; nothing in
    // these tests fetches sources.
    std::mem::forget(checkouts);
    (Client::tracked(rocket).await.unwrap(), db)
}

/// A pkgbase containing `+` must resolve to *that* package, not to a
/// space-mangled name and not to a different row.
#[rocket::async_test]
async fn packages_with_special_characters_resolve_by_name() {
    let (client, db) = test_client().await;
    for name in NAMES {
        seed(&db, name).await;
    }

    for name in NAMES {
        let response = client.get(format!("/api/package/{name}")).dispatch().await;
        assert_eq!(
            response.status(),
            Status::Ok,
            "GET /api/package/{name} should resolve"
        );
        let body = response.into_string().await.expect("body");
        assert!(
            body.contains(&format!("\"name\":\"{name}\"")),
            "GET /api/package/{name} returned the wrong package: {body}"
        );
    }
}

/// The numeric names are the reason there is no id fallback: with one, `1337`
/// would resolve to whatever row happens to hold that id.
#[rocket::async_test]
async fn a_numeric_pkgbase_is_a_name_not_an_id() {
    let (client, db) = test_client().await;
    // Seed something else first so ids and names cannot coincide by accident.
    seed(&db, "first-package").await;
    let numeric_id = seed(&db, "1337").await;
    assert_ne!(numeric_id, 1337, "test would be vacuous if the id matched");

    let response = client.get("/api/package/1337").dispatch().await;
    assert_eq!(response.status(), Status::Ok);
    let body = response.into_string().await.expect("body");
    assert!(
        body.contains("\"name\":\"1337\""),
        "numeric pkgbase resolved to something else: {body}"
    );
}

/// Sub-resources take the pkgbase in the path for the same reason the package
/// route does, so they need the same guarantee.
#[rocket::async_test]
async fn sub_resources_accept_a_plus_in_the_pkgbase() {
    let (client, db) = test_client().await;
    seed(&db, "aewm++").await;

    for path in [
        "/api/package/aewm++/settings",
        "/api/package/aewm++/builds",
        "/api/package/aewm++/source/files",
    ] {
        let status = client.get(path).dispatch().await.status();
        assert_ne!(
            status,
            Status::NotFound,
            "{path} 404'd, so the pkgbase did not survive the URL"
        );
    }
}

/// An unknown pkgbase is a 404, not a wrong package.
#[rocket::async_test]
async fn an_unknown_pkgbase_is_not_found() {
    let (client, db) = test_client().await;
    seed(&db, "aewm++").await;

    // `aewm  ` is what `aewm++` decodes to if it is ever put through
    // form-urlencoded decoding, so this is the failure mode being guarded.
    let response = client.get("/api/package/aewm%20%20").dispatch().await;
    assert_eq!(response.status(), Status::NotFound);
}

/// The client and the server must use *one* type per API shape, not two that
/// happen to look alike.
///
/// Hand-mirrored copies drift silently: before these shapes were shared, the
/// client's git variant expected `git_url`/`git_ref` while the server sent
/// `url`/`ref`, so `pkg get` failed outright for every git-sourced package —
/// and the client's `ExtendedPackage` was missing `has_patch` entirely. Both
/// were invisible because nothing deserialised a git package.
#[test]
fn client_and_server_share_one_type_per_shape() {
    // Compiles only if these are the *same* type, not merely the same fields.
    // The left side is what the server serialises, the right what the client
    // deserialises: naming both is what makes this catch drift. Comparing
    // `aurcache_types` against the client would be near-tautological, since
    // the client re-exports it.
    fn _same<T>(x: T) -> T {
        x
    }
    let _: fn(aurcache_api::models::package::SimplePackage) -> aurcache_client::SimplePackage =
        _same;
    let _: fn(aurcache_api::models::package::ExtendedPackage) -> aurcache_client::ExtendedPackage =
        _same;
    let _: fn(aurcache_api::models::builds::BuildSummary) -> aurcache_client::Build = _same;
    let _: fn(aurcache_api::models::package::PackageSource) -> aurcache_client::PackageSource =
        _same;
    let _: fn(
        aurcache_api::models::package::PackageDependency,
    ) -> aurcache_client::PackageDependency = _same;
}

/// A git source round-trips through the wire format the server actually emits.
///
/// Pins the field names — `url`, `ref`, `subfolder` — that the client had
/// wrong.
#[test]
fn a_git_source_round_trips_through_json() {
    use aurcache_types::api::package::PackageSource;
    use aurcache_types::source::GitSourceSpec;

    let source = PackageSource::Git(GitSourceSpec {
        url: "https://aur.archlinux.org/hello.git".to_string(),
        r#ref: "master".to_string(),
        subfolder: String::new(),
    });

    let json = serde_json::to_string(&source).expect("serialise");
    assert!(
        json.contains(r#""url":"https://aur.archlinux.org/hello.git""#),
        "{json}"
    );
    assert!(json.contains(r#""ref":"master""#), "{json}");
    assert!(
        !json.contains("git_url"),
        "field renamed on the wire: {json}"
    );

    let back: PackageSource = serde_json::from_str(&json).expect("deserialise");
    assert_eq!(back, source);
}

/// `latest_version` must mean the same thing on both routes.
///
/// The list query used to wrap the lookup in `COALESCE(..., '')` while the
/// detail route returned the column as-is, so a package with no build was `""`
/// from one and `null` from the other — one field with two representations of
/// "no version", depending on which URL you asked.
#[rocket::async_test]
async fn a_package_with_no_build_has_no_version_on_either_route() {
    let (client, db) = test_client().await;
    seed(&db, "hello").await;

    let listed = client
        .get("/api/packages/list?limit=10")
        .dispatch()
        .await
        .into_string()
        .await
        .expect("list body");
    assert!(
        listed.contains(r#""latest_version":null"#),
        "list should report no version as null: {listed}"
    );

    let detail = client
        .get("/api/package/hello")
        .dispatch()
        .await
        .into_string()
        .await
        .expect("detail body");
    assert!(
        detail.contains(r#""latest_version":null"#),
        "detail should report no version as null: {detail}"
    );
}

/// A build exists but has not worked out a version yet.
///
/// `builds.version` is NOT NULL DEFAULT '', so an enqueued build stores an
/// empty string. That is "not known", and must not read as a real version.
#[rocket::async_test]
async fn an_enqueued_builds_empty_version_is_not_a_version() {
    let (client, db) = test_client().await;
    let pkg_id = seed(&db, "hello").await;

    Builds::insert(builds::ActiveModel {
        pkg_id: Set(pkg_id),
        status: Set(Some(BuildStates::ENQUEUED_BUILD)),
        platform: Set(Platform::X86_64),
        version: Set(String::new()),
        start_time: Set(Some(1)),
        ..Default::default()
    })
    .exec(&db)
    .await
    .expect("insert build");

    for path in ["/api/packages/list?limit=10", "/api/package/hello"] {
        let body = client
            .get(path)
            .dispatch()
            .await
            .into_string()
            .await
            .expect("body");
        assert!(
            body.contains(r#""latest_version":null"#),
            "GET {path} reported an empty version as a version: {body}"
        );
    }
}

/// The version of a finished build is still reported, so the change above did
/// not simply blank the field out.
#[rocket::async_test]
async fn a_completed_builds_version_is_reported() {
    let (client, db) = test_client().await;
    let pkg_id = seed(&db, "hello").await;

    Builds::insert(builds::ActiveModel {
        pkg_id: Set(pkg_id),
        status: Set(Some(BuildStates::SUCCESSFUL_BUILD)),
        platform: Set(Platform::X86_64),
        version: Set("2.12.1-1".to_string()),
        start_time: Set(Some(1)),
        end_time: Set(Some(2)),
        ..Default::default()
    })
    .exec(&db)
    .await
    .expect("insert build");

    for path in ["/api/packages/list?limit=10", "/api/package/hello"] {
        let body = client
            .get(path)
            .dispatch()
            .await
            .into_string()
            .await
            .expect("body");
        assert!(
            body.contains(r#""latest_version":"2.12.1-1""#),
            "GET {path} lost the build version: {body}"
        );
    }
}

/// A package can be directly requested while its upstream version is still
/// unknown, and that must not break the list.
///
/// The dependency-resolution migration inserts rows with a NULL
/// `upstream_version` and `directly_requested = false`. Adding one of those
/// explicitly later calls `set_directly_requested`, which flips the flag and
/// nothing else — so the row enters the list before any version check has
/// filled the column in. `SimplePackage` typed it as a plain `String`, which
/// fails to decode, taking down the whole route rather than one row.
#[rocket::async_test]
async fn a_package_with_no_upstream_version_yet_does_not_break_the_list() {
    let (client, db) = test_client().await;

    Packages::insert(packages::ActiveModel {
        name: Set("promoted-dep".to_string()),
        status: Set(0),
        out_of_date: Set(0),
        upstream_version: Set(None),
        build_flags: Set(String::new()),
        platforms: Set("x86_64".to_string()),
        source_type: Set(SourceType::Aur),
        source_data: Set(SourceData::Aur {
            name: "promoted-dep".to_string(),
        }),
        directly_requested: Set(true),
        ..Default::default()
    })
    .exec(&db)
    .await
    .expect("insert package");

    let response = client.get("/api/packages/list?limit=10").dispatch().await;
    assert_eq!(
        response.status(),
        Status::Ok,
        "one row with no upstream version must not fail the whole list"
    );
    let body = response.into_string().await.expect("body");
    assert!(
        body.contains(r#""upstream_version":null"#),
        "an undetermined upstream version should be null: {body}"
    );
}

/// A failed build is not a version that got built.
///
/// `latest_version` is read everywhere as "what is in the repository". Taking
/// the newest build regardless of outcome meant a failed build of a new
/// version reported that version as built — and since the version check
/// compares upstream against this field, an upstream release whose first build
/// failed silently stopped being flagged as out of date.
#[rocket::async_test]
async fn a_failed_build_does_not_become_the_reported_version() {
    let (client, db) = test_client().await;
    let pkg_id = seed(&db, "hello").await;

    // The version that is actually in the repository.
    Builds::insert(builds::ActiveModel {
        pkg_id: Set(pkg_id),
        status: Set(Some(BuildStates::SUCCESSFUL_BUILD)),
        platform: Set(Platform::X86_64),
        version: Set("2.12.1-1".to_string()),
        start_time: Set(Some(100)),
        end_time: Set(Some(200)),
        ..Default::default()
    })
    .exec(&db)
    .await
    .expect("insert successful build");

    // A newer attempt at the next version that failed.
    Builds::insert(builds::ActiveModel {
        pkg_id: Set(pkg_id),
        status: Set(Some(BuildStates::FAILED_BUILD)),
        platform: Set(Platform::X86_64),
        version: Set("2.12.1-2".to_string()),
        start_time: Set(Some(300)),
        end_time: Set(Some(310)),
        ..Default::default()
    })
    .exec(&db)
    .await
    .expect("insert failed build");

    for path in ["/api/packages/list?limit=10", "/api/package/hello"] {
        let body = client
            .get(path)
            .dispatch()
            .await
            .into_string()
            .await
            .expect("body");
        assert!(
            body.contains(r#""latest_version":"2.12.1-1""#),
            "GET {path} should report the version in the repo: {body}"
        );
        assert!(
            !body.contains(r#""latest_version":"2.12.1-2""#),
            "GET {path} reported a version whose build failed: {body}"
        );
    }
}

/// A build still running is not in the repository either.
#[rocket::async_test]
async fn an_in_progress_build_does_not_become_the_reported_version() {
    let (client, db) = test_client().await;
    let pkg_id = seed(&db, "hello").await;

    Builds::insert(builds::ActiveModel {
        pkg_id: Set(pkg_id),
        status: Set(Some(BuildStates::SUCCESSFUL_BUILD)),
        platform: Set(Platform::X86_64),
        version: Set("1.0-1".to_string()),
        start_time: Set(Some(100)),
        end_time: Set(Some(200)),
        ..Default::default()
    })
    .exec(&db)
    .await
    .expect("insert successful build");

    Builds::insert(builds::ActiveModel {
        pkg_id: Set(pkg_id),
        status: Set(Some(BuildStates::ACTIVE_BUILD)),
        platform: Set(Platform::X86_64),
        version: Set("2.0-1".to_string()),
        start_time: Set(Some(300)),
        ..Default::default()
    })
    .exec(&db)
    .await
    .expect("insert running build");

    for path in ["/api/packages/list?limit=10", "/api/package/hello"] {
        let body = client
            .get(path)
            .dispatch()
            .await
            .into_string()
            .await
            .expect("body");
        assert!(
            body.contains(r#""latest_version":"1.0-1""#),
            "GET {path}: a running build must not count as built: {body}"
        );
    }
}

/// A dependency reports whether it is holding the build back.
///
/// The builder promotes a dependent only once every dependency has a
/// successful build whose version satisfies the recorded constraint. Without
/// that on the package page there is no way to see *which* relation is the
/// reason a package will not build.
#[rocket::async_test]
async fn dependencies_report_whether_they_are_satisfied() {
    let (client, db) = test_client().await;
    let app = seed(&db, "app").await;
    let ready = seed(&db, "ready-dep").await;
    let stale = seed(&db, "stale-dep").await;
    let unbuilt = seed(&db, "unbuilt-dep").await;

    // Built at a version that meets the constraint.
    insert_build(&db, ready, BuildStates::SUCCESSFUL_BUILD, "2.0-1").await;
    // Built, but too old for what `app` requires.
    insert_build(&db, stale, BuildStates::SUCCESSFUL_BUILD, "1.0-1").await;
    // `unbuilt` has no build at all.

    for (dependee, constraint) in [(ready, ">=2.0"), (stale, ">=2.0"), (unbuilt, ">=2.0")] {
        Dependencies::insert(dependencies::ActiveModel {
            dependent_id: Set(app),
            dependee_id: Set(dependee),
            version_constraint: Set(constraint.to_string()),
            ..Default::default()
        })
        .exec(&db)
        .await
        .expect("insert dependency");
    }

    let body = client
        .get("/api/package/app")
        .dispatch()
        .await
        .into_string()
        .await
        .expect("body");
    let value: serde_json::Value = serde_json::from_str(&body).expect("json");
    let deps = value["dependencies"].as_array().expect("dependencies");
    assert_eq!(deps.len(), 3, "{body}");

    let by_name = |name: &str| {
        deps.iter()
            .find(|d| d["name"] == name)
            .unwrap_or_else(|| panic!("no {name} in {body}"))
            .clone()
    };

    let ready = by_name("ready-dep");
    assert_eq!(ready["satisfied"], true, "{ready}");
    assert_eq!(ready["built_version"], "2.0-1", "{ready}");

    // Built, but not to a version that satisfies the constraint — this is the
    // case a bare status badge cannot express.
    let stale = by_name("stale-dep");
    assert_eq!(stale["satisfied"], false, "{stale}");
    assert_eq!(stale["built_version"], "1.0-1", "{stale}");

    let unbuilt = by_name("unbuilt-dep");
    assert_eq!(unbuilt["satisfied"], false, "{unbuilt}");
    assert_eq!(
        unbuilt["built_version"],
        serde_json::Value::Null,
        "{unbuilt}"
    );
}

/// An unconstrained dependency is satisfied by any successful build, and by
/// none at all only because there is nothing in the repository to use.
#[rocket::async_test]
async fn an_unconstrained_dependency_only_needs_to_have_built() {
    let (client, db) = test_client().await;
    let app = seed(&db, "app").await;
    let dep = seed(&db, "any-version").await;
    insert_build(&db, dep, BuildStates::SUCCESSFUL_BUILD, "0.0.1-1").await;

    Dependencies::insert(dependencies::ActiveModel {
        dependent_id: Set(app),
        dependee_id: Set(dep),
        version_constraint: Set(String::new()),
        ..Default::default()
    })
    .exec(&db)
    .await
    .expect("insert dependency");

    let body = client
        .get("/api/package/app")
        .dispatch()
        .await
        .into_string()
        .await
        .expect("body");
    assert!(body.contains(r#""satisfied":true"#), "{body}");
}

/// The package route answers from the row, with no AUR request.
///
/// It used to fetch live on every call: ~128ms of a ~130ms response, and one
/// of the AUR's 4000 daily calls per page view. These tests have no network,
/// so a route that still reached for the AUR would fail here.
#[rocket::async_test]
async fn package_metadata_is_served_from_the_row() {
    let (client, db) = test_client().await;
    let pkg_id = seed(&db, "hello").await;

    // What the version-check scheduler mirrors onto the row.
    packages::ActiveModel {
        id: Set(pkg_id),
        aur_description: Set(Some("Prints Hello World and more".to_string())),
        aur_maintainer: Set(Some("someone".to_string())),
        aur_project_url: Set(Some("https://www.gnu.org/software/hello/".to_string())),
        aur_licenses: Set(Some("GPL-3.0-or-later".to_string())),
        aur_first_submitted: Set(Some(1_425_168_000)),
        aur_last_modified: Set(Some(1_755_000_000)),
        aur_flagged_outdated: Set(Some(false)),
        ..Default::default()
    }
    .update(&db)
    .await
    .expect("store metadata");

    let body = client
        .get("/api/package/hello")
        .dispatch()
        .await
        .into_string()
        .await
        .expect("body");

    assert!(body.contains("Prints Hello World and more"), "{body}");
    assert!(body.contains(r#""maintainer":"someone""#), "{body}");
    assert!(body.contains("GPL-3.0-or-later"), "{body}");
    assert!(
        body.contains(r#""package_type":"Aur""#),
        "should report an AUR source: {body}"
    );
}

/// A package the AUR did not return has nothing mirrored, and is reported as
/// not found rather than as an AUR package with every field blank.
#[rocket::async_test]
async fn a_package_with_no_mirrored_metadata_reads_as_not_found() {
    let (client, db) = test_client().await;
    seed(&db, "hello").await;

    let body = client
        .get("/api/package/hello")
        .dispatch()
        .await
        .into_string()
        .await
        .expect("body");
    assert!(body.contains(r#""package_type":"AurNotFound""#), "{body}");
}
