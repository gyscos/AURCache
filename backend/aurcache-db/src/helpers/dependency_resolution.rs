//! Dependency resolution against the packages AURCache tracks.
//!
//! This lives in `aurcache-db` because it is the lowest crate that can see
//! both the `packages` entity and the `AurClient` (via `aurcache-deps`), which
//! lets the schema migration and the runtime package pipeline share one
//! implementation.
//!
//! It is deliberately thin. Deciding *what satisfies a dependency name* is
//! `aurcache-deps`' job and is the same question whatever the source, so all
//! this does is describe the tracked packages in the shape
//! [`SatisfyIndex`] understands and hand it over. Matching, ranking and
//! ordering all happen once, in
//! [`AurClient::resolve_dependencies`](aurcache_deps::AurClient::resolve_dependencies).

use aurcache_deps::{AurClient, Dependency, Resolutions, SatisfyIndex};
use sea_orm::{ConnectionTrait, DbErr, EntityTrait};

use crate::packages;

/// A package that can satisfy a dependency: either a row already in the
/// database, or one an in-flight add has planned but not yet inserted.
///
/// Only these three fields are ever consulted when matching, so planned
/// packages are represented directly rather than as `packages::Model`s with a
/// placeholder id — a fake id would be a trap for the next reader.
#[derive(Debug, Clone)]
pub struct PackageCandidate {
    /// The pkgbase, which is what a dependency ultimately resolves to.
    pub name: String,
    /// JSON array of split package names, as stored on `packages`.
    pub split_packages: Option<String>,
    /// JSON array of `provides` entries, as stored on `packages`.
    pub provides: Option<String>,
}

impl From<&packages::Model> for PackageCandidate {
    fn from(pkg: &packages::Model) -> Self {
        Self {
            name: pkg.name.clone(),
            split_packages: pkg.split_packages.clone(),
            provides: pkg.provides.clone(),
        }
    }
}

/// Resolve `deps` to what should happen about each of them.
///
/// `planned` are packages an in-flight add intends to insert. An add resolves
/// its whole dependency graph before writing anything, so a package planned
/// earlier in the same add is not yet in the database — without offering them
/// here, a dependency satisfied by a sibling in the same add would be resolved
/// against the AUR and planned a second time. Callers with nothing in flight
/// pass `&[]`.
///
/// `platforms` scopes AURCache's own repository, which is stored one directory
/// per platform; empty means every platform present.
pub async fn resolve_dependencies<C: ConnectionTrait>(
    client: &AurClient,
    db: &C,
    deps: &[Dependency<'_>],
    planned: &[PackageCandidate],
    platforms: &[String],
) -> Result<Resolutions, aurcache_deps::Error> {
    let wanted = deps.iter().map(|dep| dep.name).collect();
    let tracked = tracked_index(db, planned, &wanted)
        .await
        .map_err(|e| aurcache_deps::Error::Rpc(e.to_string()))?;

    client.resolve_dependencies(deps, &tracked, platforms).await
}

/// Index every package AURCache tracks, plus anything `planned`, under the
/// names each one answers to.
///
/// Every row counts, whatever state its last build left it in. A row's status
/// says how its build went, not whether a dependency on it is real, and what
/// actually gates a dependent is the dependee's latest *successful build*
/// (`helpers::builds::dependency_satisfied`) — which no filter here could
/// speak for. Restricting this to `ACTIVE`/`SUCCESS`/`ENQUEUED`, as it once
/// did, only meant a `Failed` or `WaitingForDeps` package went unrecognised
/// here and was picked up a stage later by its artifact sitting in the
/// repository, which reports "already available" and records no dependency
/// link at all.
async fn tracked_index<C: ConnectionTrait>(
    db: &C,
    planned: &[PackageCandidate],
    wanted: &std::collections::HashSet<&str>,
) -> Result<SatisfyIndex, DbErr> {
    let rows = packages::Entity::find().all(db).await?;
    let candidates = rows
        .iter()
        .map(PackageCandidate::from)
        .chain(planned.iter().cloned());

    let mut index = SatisfyIndex::new();
    for candidate in candidates {
        // A row's own name is its pkgbase, and it carries the `provides` for
        // the base as a whole.
        index.insert_package(
            &candidate.name,
            &candidate.name,
            // Nothing here has a version: these are matched to decide whether
            // a dependency *edge* should exist, and the edge records the
            // constraint for the build queue to check against real builds.
            None,
            parse_json_list(candidate.provides.as_deref()),
            wanted,
        );
        for split in parse_json_list(candidate.split_packages.as_deref()) {
            index.insert_package(&split, &candidate.name, None, Vec::<String>::new(), wanted);
        }
    }
    Ok(index)
}

fn parse_json_list(json: Option<&str>) -> Vec<String> {
    json.and_then(|value| serde_json::from_str(value).ok())
        .unwrap_or_default()
}
