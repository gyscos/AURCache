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
use aurcache_common::build_state::BuildStates;
use aurcache_db::action::Action;
use aurcache_db::builds;
use aurcache_db::dependencies;
use aurcache_db::migration::Migrator;
use aurcache_db::packages;
use aurcache_db::packages::{SourceData, SourceType};
use aurcache_db::prelude::{Builds, Dependencies, Packages};
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

/// Every caller seeds a single build per package, so build number 1 is right
/// for all of them; `UNIQUE (pkg_id, number)` means it cannot be left to the
/// column default.
async fn insert_build(db: &DatabaseConnection, pkg_id: i32, status: i32, version: &str) {
    Builds::insert(builds::ActiveModel {
        number: Set(1),
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
        .manage(ActivityLog::discarding())
        .manage(broadcast::channel::<Action>(16).0)
        // Routes that act on packages take the bundle; these tests never reach
        // one, but Rocket refuses to launch with an unmanaged type.
        .manage(Arc::new(SnapshotStore::with_checkout_root(
            checkouts.path().to_path_buf(),
        )))
        .manage(aurcache_utils::services::Services::new(
            db.clone(),
            broadcast::channel::<Action>(16).0,
            Arc::new(SnapshotStore::with_checkout_root(
                checkouts.path().to_path_buf(),
            )),
            Arc::new(aurcache_deps::AurClient::new()),
            Arc::new(aurcache_utils::repository::Repository::new(
                checkouts.path().join("repo"),
            )),
            ActivityLog::discarding(),
        ))
        // The dump route reports which AURCache wrote a dump. Rocket's
        // sentinels refuse to launch without it, which is the point: a route
        // needing unmanaged state would otherwise 500 in production.
        .manage(aurcache_api::init::ServerVersion("test".to_string()))
        // Dump and restore both move the CA's files, so both need to know
        // where they are. Rocket's sentinels refuse to launch without it.
        .manage(aurcache_api::init::CaDirectory(std::path::PathBuf::from(
            "/nonexistent-ca-dir",
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
    // `aurcache_common` against the client would be near-tautological, since
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
    use aurcache_common::api::package::PackageSource;
    use aurcache_common::source::GitSourceSpec;

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
        number: Set(1),
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
        number: Set(1),
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
        number: Set(1),
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
        number: Set(2),
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
        number: Set(1),
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
        number: Set(2),
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
        source_description: Set(Some("Prints Hello World and more".to_string())),
        source_maintainer: Set(Some("someone".to_string())),
        source_project_url: Set(Some("https://www.gnu.org/software/hello/".to_string())),
        source_licenses: Set(Some("GPL-3.0-or-later".to_string())),
        source_first_submitted: Set(Some(1_425_168_000)),
        source_last_modified: Set(Some(1_755_000_000)),
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

/// A package the AUR no longer lists says so.
///
/// Driven by what the last check actually found, not inferred from missing
/// metadata: metadata now comes from the source checkout, which a package
/// removed from the AUR still has.
#[rocket::async_test]
async fn a_package_the_aur_no_longer_lists_reads_as_not_found() {
    let (client, db) = test_client().await;
    let pkg_id = seed(&db, "hello").await;

    packages::ActiveModel {
        id: Set(pkg_id),
        aur_missing: Set(Some(true)),
        ..Default::default()
    }
    .update(&db)
    .await
    .expect("mark missing");

    let body = client
        .get("/api/package/hello")
        .dispatch()
        .await
        .into_string()
        .await
        .expect("body");
    assert!(body.contains(r#""package_type":"AurNotFound""#), "{body}");
}

// ------------------------------------------------------------ build filters

/// A build of `pkg_id`, numbered `number`, in `status`, run by `worker`.
async fn insert_worker_build(
    db: &DatabaseConnection,
    pkg_id: i32,
    number: i32,
    status: i32,
    worker: Option<i32>,
) {
    use sea_orm::ConnectionTrait;
    let worker = worker.map_or_else(|| "NULL".to_string(), |w| w.to_string());
    db.execute_unprepared(&format!(
        "INSERT INTO builds (pkg_id, number, status, start_time, platform, version, \
         attempt_count, worker_id) \
         VALUES ({pkg_id}, {number}, {status}, {number}, 'x86_64', '1.0-1', 0, {worker})"
    ))
    .await
    .expect("insert build");
}

async fn insert_worker(db: &DatabaseConnection, id: i32) {
    use sea_orm::ConnectionTrait;
    db.execute_unprepared(&format!(
        "INSERT INTO workers (id, name, status, cert_fingerprint, native_arches, \
         emulated_arches, package_affinity, priority, concurrency) \
         VALUES ({id}, 'w{id}', 'approved', 'fp{id}', 'x86_64', '', '', 0, 1)"
    ))
    .await
    .expect("insert worker");
}

/// A listed build, by what names it.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Listed {
    pkg: String,
    number: i32,
}

fn listed_build(pkg: &str, number: i32) -> Listed {
    Listed {
        pkg: pkg.to_string(),
        number,
    }
}

/// What a list route returned, sorted so the comparison ignores its order.
async fn listed(client: &Client, path: &str) -> Vec<Listed> {
    let response = client.get(path).dispatch().await;
    assert_eq!(response.status(), Status::Ok, "{path}");
    let builds: Vec<aurcache_api::models::builds::BuildSummary> =
        response.into_json().await.expect("a build list");
    let mut out: Vec<_> = builds
        .into_iter()
        .map(|b| listed_build(&b.pkg_name, b.number))
        .collect();
    out.sort();
    out
}

/// What `aurcli builds list --worker/--status` and `worker pause --wait`
/// ask: which builds one worker ran, and which it is running now. A finished
/// build keeps its worker, so the history lists too.
#[rocket::async_test]
async fn builds_filter_by_worker_and_state() {
    let (client, db) = test_client().await;
    let hello = seed(&db, "hello").await;
    let world = seed(&db, "world").await;
    // Its own package: at most one pending build per package and platform.
    let queued = seed(&db, "queued").await;
    insert_worker(&db, 1).await;
    insert_worker(&db, 2).await;
    insert_worker_build(&db, hello, 1, BuildStates::SUCCESSFUL_BUILD, Some(1)).await;
    insert_worker_build(&db, hello, 2, BuildStates::ACTIVE_BUILD, Some(1)).await;
    insert_worker_build(&db, world, 1, BuildStates::PUBLISHING, Some(2)).await;
    insert_worker_build(&db, queued, 1, BuildStates::ENQUEUED_BUILD, None).await;

    let h = |n| listed_build("hello", n);
    let w = |n| listed_build("world", n);
    let q = |n| listed_build("queued", n);
    assert_eq!(listed(&client, "/api/builds?worker=1").await, [h(1), h(2)]);
    assert_eq!(
        listed(&client, "/api/builds?worker=1&status=active").await,
        [h(2)]
    );
    assert_eq!(
        listed(&client, "/api/builds?worker=2&status=active").await,
        []
    );
    assert_eq!(
        listed(&client, "/api/builds?status=active,publishing").await,
        [h(2), w(1)]
    );
    assert_eq!(listed(&client, "/api/builds?status=enqueued").await, [q(1)]);
    assert_eq!(
        listed(&client, "/api/package/hello/builds?worker=2").await,
        []
    );
    assert_eq!(
        listed(&client, "/api/package/world/builds?status=publishing").await,
        [w(1)]
    );

    let response = client.get("/api/builds?status=running").dispatch().await;
    assert_eq!(response.status(), Status::BadRequest);
    let body = response.into_string().await.unwrap_or_default();
    assert!(
        body.contains("waiting-for-deps"),
        "names the valid states: {body}"
    );
}

/// The disk breakdown a worker reported comes back on the build, and a build
/// without one has none -- not a row of zeros.
#[rocket::async_test]
async fn a_builds_disk_usage_is_listed_with_it() {
    use sea_orm::ConnectionTrait;
    let (client, db) = test_client().await;
    let hello = seed(&db, "hello").await;
    let world = seed(&db, "world").await;
    insert_build(&db, hello, BuildStates::SUCCESSFUL_BUILD, "1.0-1").await;
    insert_build(&db, world, BuildStates::SUCCESSFUL_BUILD, "1.0-1").await;
    db.execute_unprepared(&format!(
        "UPDATE builds SET disk_chroot = 700, disk_workdir = 50, disk_sources = 9 \
         WHERE pkg_id = {hello}"
    ))
    .await
    .unwrap();

    let response = client.get("/api/builds").dispatch().await;
    let builds: Vec<aurcache_api::models::builds::BuildSummary> =
        response.into_json().await.expect("a build list");
    let usage_of = |name: &str| {
        builds
            .iter()
            .find(|b| b.pkg_name == name)
            .unwrap()
            .disk_usage
    };
    assert_eq!(
        usage_of("hello"),
        Some(aurcache_common::api::builds::DiskUsage {
            chroot: Some(700),
            workdir: Some(50),
            sources: Some(9),
            build_tree: None,
        })
    );
    assert_eq!(usage_of("world"), None);
}
