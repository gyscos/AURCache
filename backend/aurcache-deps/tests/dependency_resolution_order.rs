//! The stages of [`AurClient::resolve_dependencies`], in order.
//!
//! Resolution walks three sources -- the official repositories, the packages
//! AURCache tracks, and the AUR (by name, then by `provides`) -- and stops at
//! the first that answers. These tests pin that order, and pin the two things
//! each stage is allowed to conclude: what it resolves to, and what it costs.
//! The cost half matters because nearly every package depends on names that
//! live in `core`/`extra`/`multilib`, and resolving those over the network
//! first spent a request per package on an answer already sitting in a file.

use std::fs::{self, File};
use std::path::Path;

use aurcache_deps::{AurClient, Dependency, DependencyResolution, MatchKind, SatisfyIndex};
use flate2::Compression;
use flate2::write::GzEncoder;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Write one official repository database holding a `desc` entry per package.
fn write_official_db(
    cache_dir: &Path,
    repo_name: &str,
    packages: &[(&str, Option<&str>, &[&str])],
) {
    fs::create_dir_all(cache_dir).unwrap();
    let file = File::create(cache_dir.join(format!("{repo_name}.db.tar.gz"))).unwrap();
    let mut builder = tar::Builder::new(GzEncoder::new(file, Compression::default()));

    for (pkg_name, version, provides) in packages {
        let mut desc = format!("%NAME%\n{pkg_name}\n\n%BASE%\n{pkg_name}\n\n");
        if let Some(version) = version {
            desc.push_str(&format!("%VERSION%\n{version}\n\n"));
        }
        if !provides.is_empty() {
            desc.push_str("%PROVIDES%\n");
            for entry in *provides {
                desc.push_str(entry);
                desc.push('\n');
            }
            desc.push('\n');
        }

        let mut header = tar::Header::new_gnu();
        header
            .set_path(format!("{pkg_name}-{}-1/desc", version.unwrap_or("1.0")))
            .unwrap();
        header.set_size(desc.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append(&header, desc.as_bytes()).unwrap();
    }
    builder.finish().unwrap();
}

struct Env {
    _tmp: tempfile::TempDir,
    client: AurClient,
}

/// A client whose RPC points at `rpc_url` and whose official repositories hold
/// `published` -- in `extra`, with `core` and `multilib` present but empty.
///
/// Present and empty rather than absent: resolution refuses to guess when it
/// cannot read the databases, so leaving them out would fail the resolve
/// rather than fall through to the AUR. No mirrorlist is written either, since
/// a cache this fresh is never refreshed.
fn env_for(rpc_url: &str, published: &[(&str, Option<&str>, &[&str])]) -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let cache_dir = tmp.path().join("official-cache");
    write_official_db(&cache_dir, "extra", published);
    write_official_db(&cache_dir, "core", &[]);
    write_official_db(&cache_dir, "multilib", &[]);

    let client =
        AurClient::with_urls_and_paths(rpc_url, tmp.path().join("no-such-mirrorlist"), cache_dir);
    Env { _tmp: tmp, client }
}

/// Stage 2: a package the caller's database tracks resolves to it, without a
/// single request to the AUR.
#[tokio::test]
async fn a_tracked_package_resolves_locally_and_costs_no_rpc_call() {
    let server = MockServer::start().await;
    let env = env_for(&format!("{}/rpc/v5", server.uri()), &[]);

    let mut tracked = SatisfyIndex::new();
    tracked.insert("mydep", "mydep-git", MatchKind::Provides, None);

    let resolved = env
        .client
        .resolve_dependencies(&[Dependency::unversioned("mydep")], &tracked)
        .await
        .unwrap();

    assert_eq!(
        resolved.get("mydep"),
        Some(&DependencyResolution::Local {
            pkgbase: "mydep-git".to_string()
        })
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

/// Stage 1 wins over stage 2, and how a candidate claims the name does not
/// enter into it.
///
/// The reported bug: `git-git` declares `provides=('git')`, so once it was
/// tracked it captured every dependency on `git` -- including over the `git`
/// in `extra`, which carries the name. Resolving a name to a package is what
/// makes that package tracked, so one bad answer used to become permanent.
#[tokio::test]
async fn the_official_repositories_beat_a_tracked_package_that_provides_the_name() {
    let server = MockServer::start().await;
    let env = env_for(
        &format!("{}/rpc/v5", server.uri()),
        &[("git", Some("2.52.0-1"), &[])],
    );

    let mut tracked = SatisfyIndex::new();
    tracked.insert("git", "git-git", MatchKind::Provides, None);

    let resolved = env
        .client
        .resolve_dependencies(&[Dependency::unversioned("git")], &tracked)
        .await
        .unwrap();

    assert_eq!(
        resolved.get("git"),
        Some(&DependencyResolution::Available),
        "a tracked provider must not capture a name the repositories carry"
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

/// Stage 1: a dependency the official repositories publish is answered from
/// disk, and the RPC is never contacted.
#[tokio::test]
async fn a_dependency_in_the_official_repositories_costs_no_rpc_call() {
    let server = MockServer::start().await;
    let env = env_for(
        &format!("{}/rpc/v5", server.uri()),
        &[("mydep", Some("1.0"), &[])],
    );

    let resolved = env
        .client
        .resolve_dependencies(&[Dependency::unversioned("mydep")], &SatisfyIndex::new())
        .await
        .unwrap();

    assert_eq!(
        resolved.get("mydep"),
        Some(&DependencyResolution::Available)
    );
    let seen = server.received_requests().await.unwrap();
    assert!(
        seen.is_empty(),
        "the AUR was contacted for a dependency already in the repositories: {:?}",
        seen.iter().map(|r| r.url.to_string()).collect::<Vec<_>>()
    );
}

/// The same when the name is only reachable through `%PROVIDES%`, which is how
/// most virtual dependencies resolve.
#[tokio::test]
async fn a_provided_dependency_also_costs_no_rpc_call() {
    let server = MockServer::start().await;
    let env = env_for(
        &format!("{}/rpc/v5", server.uri()),
        &[("myprovider", Some("1.0"), &["mydep=1.2.3"])],
    );

    let resolved = env
        .client
        .resolve_dependencies(&[Dependency::unversioned("mydep")], &SatisfyIndex::new())
        .await
        .unwrap();

    assert_eq!(
        resolved.get("mydep"),
        Some(&DependencyResolution::Available)
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

/// A repository hit ends resolution, so it has to honour the constraint: the
/// binary is installed as it is and nothing downstream re-checks its version.
#[tokio::test]
async fn a_repository_entry_that_is_too_old_does_not_answer() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"type":"multiinfo","resultcount":1,"results":[
                 {"Name":"mydep","PackageBase":"mydep","Version":"2.5-1"}
               ],"version":5}"#,
            "application/json",
        ))
        .mount(&server)
        .await;

    let env = env_for(
        &format!("{}/rpc/v5", server.uri()),
        &[("mydep", Some("1.0-1"), &[])],
    );

    let resolved = env
        .client
        .resolve_dependencies(&[Dependency::new("mydep", ">=2.0")], &SatisfyIndex::new())
        .await
        .unwrap();

    assert_eq!(
        resolved.get("mydep"),
        Some(&DependencyResolution::Aur {
            pkgbase: "mydep".to_string()
        }),
        "a repository holding 1.0 must not answer for >=2.0"
    );
}

/// A bare `provides` carries no version, so it cannot answer a bound -- the
/// same rule pacman applies.
#[tokio::test]
async fn an_unversioned_provides_does_not_answer_a_versioned_dependency() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"type":"multiinfo","resultcount":1,"results":[
                 {"Name":"mydep","PackageBase":"mydep","Version":"2.5-1"}
               ],"version":5}"#,
            "application/json",
        ))
        .mount(&server)
        .await;

    let env = env_for(
        &format!("{}/rpc/v5", server.uri()),
        &[("myprovider", Some("1.0"), &["mydep"])],
    );

    let resolved = env
        .client
        .resolve_dependencies(&[Dependency::new("mydep", ">=2.0")], &SatisfyIndex::new())
        .await
        .unwrap();

    assert!(matches!(
        resolved.get("mydep"),
        Some(DependencyResolution::Aur { .. })
    ));
}

/// Stage 3: a mix still asks the AUR, but only about the name the repositories
/// could not answer -- and in one request, not one per dependency.
#[tokio::test]
async fn only_the_unresolved_names_reach_the_aur() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"type":"multiinfo","resultcount":1,"results":[
                 {"Name":"stranger","PackageBase":"stranger","Version":"1.0-1"}
               ],"version":5}"#,
            "application/json",
        ))
        .mount(&server)
        .await;

    let env = env_for(
        &format!("{}/rpc/v5", server.uri()),
        &[("known", Some("1.0"), &[])],
    );

    let resolved = env
        .client
        .resolve_dependencies(
            &[
                Dependency::unversioned("known"),
                Dependency::unversioned("stranger"),
            ],
            &SatisfyIndex::new(),
        )
        .await
        .unwrap();

    assert_eq!(
        resolved.get("known"),
        Some(&DependencyResolution::Available)
    );
    assert_eq!(
        resolved.get("stranger"),
        Some(&DependencyResolution::Aur {
            pkgbase: "stranger".to_string()
        })
    );

    let urls: Vec<String> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| r.url.to_string())
        .collect();
    assert_eq!(urls.len(), 1, "expected one batched request, got {urls:?}");
    assert!(!urls[0].contains("known"), "{urls:?}");
}

/// Stage 4, and the end of the line: a name nothing anywhere provides is
/// reported, not dropped.
#[tokio::test]
async fn a_name_nothing_provides_is_reported_as_unresolved() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"type":"multiinfo","resultcount":0,"results":[],"version":5}"#,
            "application/json",
        ))
        .mount(&server)
        .await;

    let env = env_for(&format!("{}/rpc/v5", server.uri()), &[]);

    let resolved = env
        .client
        .resolve_dependencies(&[Dependency::unversioned("libjpeg6")], &SatisfyIndex::new())
        .await
        .unwrap();

    assert!(resolved.get("libjpeg6").is_none());
    assert_eq!(resolved.unresolved, vec!["libjpeg6".to_string()]);
}

/// Reading the repositories is not allowed to fail quietly. A corrupt database
/// used to be indistinguishable from "the official repositories do not have
/// it", which sent ordinary `core` names off to be built from the AUR.
#[tokio::test]
async fn an_unreadable_repository_fails_the_resolve() {
    let tmp = tempfile::tempdir().unwrap();
    let cache_dir = tmp.path().join("official-cache");
    fs::create_dir_all(&cache_dir).unwrap();
    // Present, fresh, and not a gzip stream.
    for repo_name in ["core", "extra", "multilib"] {
        fs::write(cache_dir.join(format!("{repo_name}.db.tar.gz")), b"garbage").unwrap();
    }

    let client = AurClient::with_urls_and_paths(
        "http://unused.invalid/rpc/v5",
        tmp.path().join("no-such-mirrorlist"),
        cache_dir,
    );

    let result = client
        .resolve_dependencies(&[Dependency::unversioned("glibc")], &SatisfyIndex::new())
        .await;
    assert!(
        result.is_err(),
        "a corrupt repository database must not read as 'not found'"
    );
}

/// Duplicates cost nothing: a name appearing in both `depends` and
/// `makedepends` is resolved once.
#[tokio::test]
async fn a_repeated_dependency_is_resolved_once() {
    let server = MockServer::start().await;
    let env = env_for(
        &format!("{}/rpc/v5", server.uri()),
        &[("mydep", Some("1.0"), &[])],
    );

    let resolved = env
        .client
        .resolve_dependencies(
            &[
                Dependency::unversioned("mydep"),
                Dependency::unversioned("mydep"),
            ],
            &SatisfyIndex::new(),
        )
        .await
        .unwrap();

    assert_eq!(resolved.found.len(), 1);
}
