use crate::package::add::{
    ensure_aur_package_exists_recursive, provides_json, split_packages_json,
};
use crate::services::Services;
use crate::vcs_check::{record_queued_vcs_sources, resolve_vcs_commits, vcs_sources_moved};
use alpm_types::Version;
use anyhow::{anyhow, bail};
use async_recursion::async_recursion;
use aurcache_activitylog::events::Event;
use aurcache_common::build_state::BuildTrigger;
use aurcache_common::builder::BuildStates;
use aurcache_db::action::Action;
use aurcache_db::helpers::build_enqueue::{enqueue_build_if_missing, promote_waiting_build};
use aurcache_db::prelude::{Builds, Dependencies, PackageVcsSources, Packages};
use aurcache_db::{builds, dependencies, package_vcs_sources, packages};
use aurcache_deps::{DependencyResolution, PkgDeps};
use pacman_mirrors::platforms::Platform;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter,
    QueryOrder, QuerySelect, Set, TransactionTrait,
};
use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use tokio::sync::broadcast::Sender;
use tracing::{info, warn};

/// Remove packages that have no remaining dependents and are not directly requested.
///
/// Deleting through [`package_delete`] rather than row by row here. The
/// hand-written version this replaced took the builds, the dependency links and
/// the VCS sources and left the `files` rows behind, pointing at a package id
/// that no longer existed. Nothing notices until the same package is added
/// again and rebuilt: ingest finds a `files` row for the artifact owned by
/// somebody else, cannot find a dependency edge to justify a transfer -- there
/// is no owner left to have one -- and refuses to publish with "already
/// produced by another package". That is terminal, and it repeats on every
/// retry until the build's attempt budget is spent, so the package can never be
/// built again. `files.package_id` has a foreign key now, which would have made
/// this loud rather than silent, but the rows still have to go so their
/// artifacts leave the repository with them.
async fn remove_orphaned_packages(services: &Services, exclude_id: i32) -> anyhow::Result<()> {
    let db = &services.db;
    // Only ids: the rows are never read, only their keys collected.
    let candidate_ids: Vec<i32> = Packages::find()
        .select_only()
        .column(packages::Column::Id)
        .filter(packages::Column::DirectlyRequested.eq(false))
        .filter(packages::Column::Id.ne(exclude_id))
        .into_tuple::<i32>()
        .all(db)
        .await?;
    if candidate_ids.is_empty() {
        return Ok(());
    }
    // The dependees that still have edges, in one grouped query — not one
    // `COUNT` per candidate.
    let referenced: HashSet<i32> = Dependencies::find()
        .select_only()
        .column(dependencies::Column::DependeeId)
        .filter(dependencies::Column::DependeeId.is_in(candidate_ids.iter().copied()))
        .group_by(dependencies::Column::DependeeId)
        .into_tuple::<i32>()
        .all(db)
        .await?
        .into_iter()
        .collect();
    let orphaned: Vec<i32> = candidate_ids
        .into_iter()
        .filter(|id| !referenced.contains(id))
        .collect();
    crate::package::delete::package_delete(db, &services.store, &services.repo, &orphaned).await
}

/// Update every package currently marked as outdated.
///
/// Only packages whose latest build completed successfully are retriggered.
/// Returns the build IDs enqueued across all updated packages.
///
/// Updates of packages with VCS sources run forced, and only those. A VCS
/// package is flagged when an upstream commit moved without its PKGBUILD's
/// version changing, and the unforced path would refuse exactly that case as
/// "already up to date" -- the version is only computed by `pkgver()` at build
/// time.
///
/// Anything else runs unforced, because for it the same version means nothing
/// new to build. The flag says the AUR is ahead of what was built; if the
/// source this server resolves has not caught up (a failed snapshot refresh),
/// a forced update would rebuild the old version, the success would clear the
/// flag, and the next version check would set it again -- a rebuild every
/// pass. Unforced, it is skipped and stays flagged until the source catches up.
pub async fn package_update_all_outdated(services: &Services) -> anyhow::Result<Vec<i32>> {
    let db = &services.db;
    let pkg_models: Vec<packages::Model> = Packages::find()
        .filter(packages::Column::OutOfDate.eq(1))
        .order_by_asc(packages::Column::Name)
        .all(db)
        .await?;
    let activity_log = &services.activity;

    // Which outdated packages track a VCS source, in one query: the loop
    // below used to count per package, a round trip each for a flag that
    // only decides forced-vs-unforced.
    let vcs_tracked: std::collections::HashSet<i32> = match PackageVcsSources::find()
        .select_only()
        .column(package_vcs_sources::Column::PackageId)
        .filter(package_vcs_sources::Column::PackageId.is_in(pkg_models.iter().map(|pkg| pkg.id)))
        .into_tuple()
        .all(db)
        .await
    {
        Ok(ids) => ids.into_iter().collect(),
        Err(e) => {
            warn!(
                "Auto update could not list VCS-tracked packages, treating all as untracked: {e}"
            );
            std::collections::HashSet::new()
        }
    };

    let mut ids_total = vec![];
    // One package's failure must not starve the rest. A sourceinfo that no
    // longer applies after an upstream bump, a dependency that cannot be
    // fetched, or a resolution that comes apart mid-way all fail this update,
    // and with `?` the whole pass stopped at the first of them -- silently
    // skipping every package after it, every round, while the offender stayed
    // out of date. Each package is handled on its own and moves on.
    for pkg in pkg_models {
        if pkg.status != BuildStates::SUCCESSFUL_BUILD {
            info!(
                "Package auto update was not triggered for package {} because of prev. build status: {}",
                pkg.name, pkg.status
            );
            continue;
        }
        let package_name = pkg.name.clone();
        let force = vcs_tracked.contains(&pkg.id);
        match package_update(services, pkg, force, BuildTrigger::AutoUpdate).await {
            Ok(results) => {
                // No actor: the auto-updater is the server acting on its own,
                // which is what an entry without a user already says.
                activity_log.emit(Event::PackageUpdated {
                    pkg: package_name.clone().into(),
                    forced: force,
                });
                ids_total.extend(
                    results
                        .into_iter()
                        .filter(|r| r.enqueued)
                        .map(|r| r.build_id),
                );
            }
            Err(e) => activity_log.emit(Event::UpdateSkipped {
                pkg: package_name.into(),
                error: format!("{e:#}"),
            }),
        }
    }
    Ok(ids_total)
}

/// Updates a single package for all required platforms.
///
/// This function fetches the latest package metadata and updates it if necessary.
///
/// # Arguments
///
/// * `store` - The process-wide [`SnapshotStore`]. It must be the shared instance
///   (see `main.rs`), not a fresh one: a new store starts with an empty cache and
///   re-resolves every source from scratch, and its cache never sees the
///   refreshes the version-check scheduler performs.
/// * `db` - A reference to the database connection.
/// * `pkg_model` - The package model to update.
/// * `force` - A boolean flag to force an update even if the package version is unchanged.
/// * `trigger` - Why the builds are being queued, recorded on each build row --
///   and on the rows of any dependency rebuilt along the way, which is part of
///   the same request.
/// * `tx` - A broadcast channel sender for triggering build actions.
///
/// # Returns
///
/// * `Ok(Vec<PlatformUpdateResult>)` - One entry per configured platform, describing the build
///   that was enqueued/promoted or left waiting on dependencies.
/// * `Err(anyhow::Error)` - If any error occurs during the update trigger.
pub async fn package_update(
    services: &Services,
    pkg_model: packages::Model,
    force: bool,
    trigger: BuildTrigger,
) -> anyhow::Result<Vec<PlatformUpdateResult>> {
    let mut visited = HashSet::new();
    package_update_inner(services, pkg_model, force, trigger, &mut visited).await
}

/// Recompute and persist a package's dependency graph from its current
/// (possibly patched) source, without checking versions or enqueuing builds.
///
/// A patch edit can change `depends`/`makedepends` without necessarily
/// bumping `pkgver`/`pkgrel`, so the dependency graph needs to be kept in
/// sync independently of the regular version-triggered update flow. This is
/// intentionally lighter-weight than [`package_update`]: it does
/// not enqueue or promote any builds.
pub async fn package_resync_dependencies(
    services: &Services,
    pkg_model: &packages::Model,
) -> anyhow::Result<()> {
    let sourceinfo = services
        .store
        .sourceinfo(&pkg_model.source_data, pkg_model.patch.as_deref())
        .await
        .map_err(|e| anyhow!("Failed to resolve source info: {e:#}"))?;
    let deps = aurcache_deps::deps_from_srcinfo(
        &sourceinfo,
        &crate::pkg::architectures_for_platforms(&pkg_model.platforms),
    );

    sync_dependency_graph(services, pkg_model, &deps).await?;

    // The dependency change may have made some previously-required
    // dependency-only packages no longer needed.
    remove_orphaned_packages(services, pkg_model.id).await?;

    Ok(())
}

/// Recursively update a package and its dependencies, enqueuing builds for ready platforms.
#[allow(clippy::double_must_use)]
#[async_recursion]
async fn package_update_inner(
    services: &Services,
    pkg_model: packages::Model,
    force: bool,
    trigger: BuildTrigger,
    visited: &mut HashSet<i32>,
) -> anyhow::Result<Vec<PlatformUpdateResult>> {
    if !visited.insert(pkg_model.id) {
        return Ok(vec![]);
    }

    let sourceinfo = services
        .store
        .sourceinfo(&pkg_model.source_data, pkg_model.patch.as_deref())
        .await
        .map_err(|e| anyhow!("Failed to resolve source info: {e:#}"))?;
    let upstream_version = sourceinfo.base.version.to_string();
    let deps = aurcache_deps::deps_from_srcinfo(
        &sourceinfo,
        &crate::pkg::architectures_for_platforms(&pkg_model.platforms),
    );

    let graph = sync_dependency_graph(services, &pkg_model, &deps).await?;

    // With the update, it's possible some dependencies are no longer needed.
    remove_orphaned_packages(services, pkg_model.id).await?;

    // Only a *successful* build makes a version "already built". This used to
    // ask for the latest build of any outcome, which meant a failed attempt at
    // the new version blocked every retry of it: upstream moves to 1.4.1-1, the
    // build fails, the package stays flagged out of date, and pressing Update
    // answers "already up to date (version 1.4.1-1)" about a version that is
    // nowhere in the repository. Nothing could shift it but a forced build.
    let built_version = aurcache_db::helpers::builds::latest_successful_version_any_platform(
        &services.db,
        pkg_model.id,
    )
    .await?;

    if !force {
        // A VCS package's published `pkgver` says when its PKGBUILD was last
        // touched, not what upstream is at, so the version comparison below
        // cannot answer for one: it compares `1:r11002.cdabad3d0-1` from the
        // AUR against `1:r14632.02cac3259-1` that `pkgver()` produced, which
        // never match, and every unforced update rebuilt. The sources are what
        // it should be asking about.
        match vcs_sources_moved(&services.db, pkg_model.id, &sourceinfo).await {
            Ok(Some(false)) => bail!(
                "Latest build is already up to date (no tracked source has moved; \
                 use --force to rebuild anyway)"
            ),
            Ok(Some(true)) => {}
            // Not a VCS package, or nothing recorded to compare against: the
            // version is the only question there is.
            Ok(None) => {
                if built_version.as_deref() == Some(upstream_version.as_str()) {
                    bail!("Latest build is already up to date (version {upstream_version})");
                }
            }
            // A remote we could not reach is not evidence of anything. Falling
            // through to the build is the safe direction.
            Err(e) => services.activity.emit(Event::VcsSyncFailed {
                pkg: pkg_model.name.as_str().into(),
                error: format!("{e:#}, building anyway"),
            }),
        }
    }

    let platform_results = enqueue_platform_builds(
        services,
        BuildRequest {
            pkg_model: &pkg_model,
            version: &upstream_version,
            graph: &graph,
            trigger,
        },
        visited,
    )
    .await?;

    // What these builds are being made from, recorded against each of them, so
    // the next version check compares upstream with what was built rather than
    // with whatever it last happened to look at. Resolving costs one
    // `ls-remote` per VCS source and nothing at all for a package that has
    // none. Best-effort: an unrecorded build reads as unknown later, which
    // costs a redundant rebuild, where failing here would cost the build.
    let queued_commits = resolve_vcs_commits(&sourceinfo).await;
    if !queued_commits.is_empty() {
        for result in &platform_results {
            if let Err(e) =
                record_queued_vcs_sources(&services.db, result.build_id, &queued_commits).await
            {
                services.activity.emit(Event::BuildRecordFailed {
                    build: aurcache_common::api::log::BuildRef {
                        pkgbase: pkg_model.name.clone(),
                        number: result.build_number,
                    },
                    what: "queued VCS sources".to_string(),
                    error: e.to_string(),
                });
            }
        }
    }

    let any_enqueued = platform_results.iter().any(|r| r.enqueued);
    let has_waiting = platform_results.iter().any(|r| !r.enqueued);

    let pkgbase = sourceinfo.base.name.to_string();
    let initial_status = if has_waiting && !any_enqueued {
        BuildStates::WAITING_FOR_DEPS
    } else {
        BuildStates::ENQUEUED_BUILD
    };
    // Four columns, not the whole row (which carries the large `source_data`
    // JSON), and no transaction: this is one statement, so there is nothing
    // to make atomic.
    packages::ActiveModel {
        id: Set(pkg_model.id),
        status: Set(initial_status),
        upstream_version: Set(Some(upstream_version.clone())),
        split_packages: Set(split_packages_json(&pkgbase, &deps.pkgnames)?),
        provides: Set(provides_json(&deps.provides)?),
        ..Default::default()
    }
    .update(&services.db)
    .await?;

    Ok(platform_results)
}

/// A single dependency of the package being updated, with its merged bounds
/// (several when a range was declared).
struct DepInfo {
    constraint: Vec<crate::pkg::Constraint>,
    package: packages::Model,
}

/// Dependencies resolved for the current update, keyed by pkgbase.
struct DependencyGraph {
    deps: HashMap<String, DepInfo>,
}

/// Resolve dependency constraints, ensure all dependees exist in the DB,
/// and sync the dependency rows.
///
/// Does not care about builds at this point.
async fn sync_dependency_graph(
    services: &Services,
    pkg_model: &packages::Model,
    deps: &PkgDeps,
) -> anyhow::Result<DependencyGraph> {
    let dep_constraints_by_pkgbase = resolve_dependency_edges(services, pkg_model, deps).await?;

    ensure_missing_dependency_packages(services, pkg_model, &dep_constraints_by_pkgbase).await?;

    if dep_constraints_by_pkgbase.is_empty() {
        sync_dependency_rows(
            &services.db,
            pkg_model.id,
            &dep_constraints_by_pkgbase,
            &HashMap::new(),
        )
        .await?;
        return Ok(DependencyGraph {
            deps: HashMap::new(),
        });
    }

    let dep_packages = fetch_dep_packages_map(&services.db, &dep_constraints_by_pkgbase).await?;

    sync_dependency_rows(
        &services.db,
        pkg_model.id,
        &dep_constraints_by_pkgbase,
        &dep_packages,
    )
    .await?;

    // A dependency row can vanish between `ensure_missing_dependency_packages`
    // and this re-read (a concurrent delete); skip it rather than panic.
    let deps_map = dep_constraints_by_pkgbase
        .into_iter()
        .filter_map(|(pkgbase, constraint)| {
            dep_packages.get(&pkgbase).map(|package| {
                (
                    pkgbase,
                    DepInfo {
                        constraint,
                        package: package.clone(),
                    },
                )
            })
        })
        .collect();

    Ok(DependencyGraph { deps: deps_map })
}

/// The dependency edges `pkg_model` should have, keyed by dependee pkgbase.
///
/// The same reduction the add path performs in `plan_package_with_deps`:
/// resolve every declared dependency, drop the ones already available as
/// binaries, and merge the constraints of every name that landed on the same
/// pkgbase onto one edge. The difference is only what happens afterwards —
/// an add plans rows, a resync reconciles the ones already there.
async fn resolve_dependency_edges(
    services: &Services,
    pkg_model: &packages::Model,
    deps: &PkgDeps,
) -> anyhow::Result<HashMap<String, Vec<crate::pkg::Constraint>>> {
    let declared = crate::pkg::DependencySet::of(deps)?;
    if declared.names.is_empty() {
        return Ok(HashMap::new());
    }

    // As on the add path: what the package answers to itself is never an edge.
    let self_provided =
        crate::pkg::self_provided_names(&pkg_model.name, &deps.pkgnames, &deps.provides);
    let pairs: Vec<(String, String)> = declared
        .to_pairs()
        .into_iter()
        .filter(|(name, _)| !self_provided.contains(name))
        .collect();
    // One package, one resolution, so the snapshot lives no longer than this
    // call.
    let tracked =
        aurcache_db::helpers::dependency_resolution::TrackedPackages::load(&services.db).await?;
    let resolved_deps = aurcache_db::helpers::dependency_resolution::resolve_dependencies(
        &services.client,
        &tracked,
        &crate::pkg::as_dependencies(&pairs),
        &[],
        &current_dependee_names(&services.db, pkg_model.id).await?,
    )
    .await?;

    if !resolved_deps.unresolved.is_empty() {
        for dependency in &resolved_deps.unresolved {
            services.activity.emit(Event::DepsUnresolved {
                pkg: pkg_model.name.as_str().into(),
                dependency: dependency.clone(),
            });
        }
    }

    let mut by_pkgbase: HashMap<String, Vec<crate::pkg::Constraint>> = HashMap::new();
    for (dep_name, _) in &pairs {
        let Some(resolution) = resolved_deps.get(dep_name) else {
            continue;
        };
        let dep_pkgbase = match resolution {
            DependencyResolution::Available => continue,
            DependencyResolution::Local { pkgbase } | DependencyResolution::Aur { pkgbase } => {
                pkgbase
            }
        };
        if dep_pkgbase == &pkg_model.name {
            continue;
        }
        crate::pkg::merge_bounds_into(
            &mut by_pkgbase,
            dep_pkgbase,
            declared.constraints.get(dep_name),
        )?;
    }

    Ok(by_pkgbase)
}

/// The package bases this package already depends on.
///
/// Handed back to resolution as a preference, which is what lets a dependency
/// someone repointed by hand persist. Every edge is recomputed from the
/// declared names on each update, so a provider that beat the ranking once
/// would otherwise be replaced by the ranking's own pick the next time round.
/// Being the existing edge is only a tie-break among the packages that
/// genuinely satisfy the name: one that stops providing it is dropped like any
/// other, and the official repositories are still asked first.
async fn current_dependee_names(
    db: &DatabaseConnection,
    dependent_id: i32,
) -> anyhow::Result<HashSet<String>> {
    let dependee_ids: Vec<i32> = Dependencies::find()
        .filter(dependencies::Column::DependentId.eq(dependent_id))
        .select_only()
        .column(dependencies::Column::DependeeId)
        .into_tuple()
        .all(db)
        .await?;
    if dependee_ids.is_empty() {
        return Ok(HashSet::new());
    }

    Ok(Packages::find()
        .filter(packages::Column::Id.is_in(dependee_ids))
        .select_only()
        .column(packages::Column::Name)
        .into_tuple::<String>()
        .all(db)
        .await?
        .into_iter()
        .collect())
}

/// Ensure all resolved dependency packages exist in the database,
/// adding them via the AUR if missing.
async fn ensure_missing_dependency_packages(
    services: &Services,
    pkg_model: &packages::Model,
    dep_constraints_by_pkgbase: &HashMap<String, Vec<crate::pkg::Constraint>>,
) -> anyhow::Result<()> {
    // The packages already tracked, in one query — not one probe per
    // declared dependency. Nothing declared means nothing to look up (`IN ()`
    // is not valid SQL everywhere).
    if dep_constraints_by_pkgbase.is_empty() {
        return Ok(());
    }
    let tracked: HashSet<String> = Packages::find()
        .select_only()
        .column(packages::Column::Name)
        .filter(packages::Column::Name.is_in(dep_constraints_by_pkgbase.keys().map(String::as_str)))
        .into_tuple::<String>()
        .all(&services.db)
        .await?
        .into_iter()
        .collect();
    for dep_pkgbase in dep_constraints_by_pkgbase.keys() {
        if !tracked.contains(dep_pkgbase) {
            ensure_aur_package_exists_recursive(
                &services.client,
                &services.store,
                &services.db,
                dep_pkgbase,
                &pkg_model.platforms,
                &pkg_model.build_flags,
            )
            .await?;
        }
    }
    Ok(())
}

/// Fetch a name→model map of all dependency packages from the database.
async fn fetch_dep_packages_map(
    db: &DatabaseConnection,
    dep_constraints_by_pkgbase: &HashMap<String, Vec<crate::pkg::Constraint>>,
) -> anyhow::Result<HashMap<String, packages::Model>> {
    Ok(Packages::find()
        .filter(packages::Column::Name.is_in(dep_constraints_by_pkgbase.keys().cloned()))
        .all(db)
        .await?
        .into_iter()
        .map(|pkg| (pkg.name.clone(), pkg))
        .collect())
}

/// Insert, update, or remove dependency rows to match the current constraint set.
async fn sync_dependency_rows(
    db: &DatabaseConnection,
    dependent_id: i32,
    dep_constraints_by_pkgbase: &HashMap<String, Vec<crate::pkg::Constraint>>,
    dep_packages: &HashMap<String, packages::Model>,
) -> anyhow::Result<()> {
    let txn = db.begin().await?;
    let desired_dependee_ids: HashSet<i32> = dep_packages.values().map(|pkg| pkg.id).collect();

    // Loaded once and shared by both passes below: the rows the upsert pass
    // needs are already here, so re-querying each edge is pure overhead.
    let existing: HashMap<i32, dependencies::Model> = Dependencies::find()
        .filter(dependencies::Column::DependentId.eq(dependent_id))
        .all(&txn)
        .await?
        .into_iter()
        .map(|row| (row.dependee_id, row))
        .collect();

    let stale: Vec<i32> = existing
        .keys()
        .filter(|id| !desired_dependee_ids.contains(id))
        .copied()
        .collect();
    if !stale.is_empty() {
        Dependencies::delete_many()
            .filter(dependencies::Column::DependentId.eq(dependent_id))
            .filter(dependencies::Column::DependeeId.is_in(stale))
            .exec(&txn)
            .await?;
    }

    for (dep_pkgbase, constraint) in dep_constraints_by_pkgbase {
        let Some(dep_pkg) = dep_packages.get(dep_pkgbase) else {
            continue;
        };
        let serialized = crate::pkg::join_constraints(constraint);

        if let Some(edge) = existing.get(&dep_pkg.id) {
            let mut active: dependencies::ActiveModel = edge.clone().into();
            active.version_constraint = Set(serialized.clone());
            active.save(&txn).await?;
        } else {
            dependencies::ActiveModel {
                dependent_id: Set(dependent_id),
                dependee_id: Set(dep_pkg.id),
                version_constraint: Set(serialized),
                ..Default::default()
            }
            .save(&txn)
            .await?;
        }
    }

    txn.commit().await?;
    Ok(())
}

/// Check whether a successful build of `dependee` satisfies the given version
/// bounds: every one of them must hold.
async fn dependency_satisfies_constraint(
    db: &DatabaseConnection,
    dependee_id: i32,
    platform: &Platform,
    constraint: &[crate::pkg::Constraint],
) -> anyhow::Result<bool> {
    let Some(version) =
        aurcache_db::helpers::builds::latest_successful_version(db, dependee_id, platform.as_str())
            .await?
    else {
        return Ok(false);
    };

    if constraint.is_empty() {
        return Ok(true);
    };
    let Ok(version) = Version::from_str(&version) else {
        return Ok(false);
    };
    Ok(constraint.iter().all(|bound| bound.is_satisfied(&version)))
}

/// Check whether every dependency in the graph is satisfied, or already has a
/// pending build, on a single platform.
///
/// If a dependency needs a rebuild and no build is pending, this triggers the
/// recursive update so the dependency will be available when the dependent
/// starts. Packages whose last build failed are never auto-retriggered — the
/// user has to retry those explicitly.
async fn dependencies_ready_for_platform(
    services: &Services,
    platform: &Platform,
    graph: &DependencyGraph,
    trigger: BuildTrigger,
    visited: &mut HashSet<i32>,
) -> anyhow::Result<bool> {
    for dep_info in graph.deps.values() {
        if dependency_satisfies_constraint(
            &services.db,
            dep_info.package.id,
            platform,
            &dep_info.constraint,
        )
        .await?
        {
            continue;
        }

        let has_pending_build = Builds::find()
            .filter(builds::Column::PkgId.eq(dep_info.package.id))
            .filter(builds::Column::Platform.eq(platform.as_str()))
            .filter(builds::Column::Status.is_in([
                Some(BuildStates::ENQUEUED_BUILD),
                Some(BuildStates::ACTIVE_BUILD),
                Some(BuildStates::WAITING_FOR_DEPS),
                Some(BuildStates::PUBLISHING),
            ]))
            .count(&services.db)
            .await?
            > 0;

        // A dependency whose last build failed is not auto-retried.
        if !has_pending_build && dep_info.package.status != BuildStates::FAILED_BUILD {
            package_update_inner(services, dep_info.package.clone(), true, trigger, visited)
                .await?;
        }

        return Ok(false);
    }

    Ok(true)
}

/// What to build and its resolved dependency graph.
struct BuildRequest<'a> {
    pkg_model: &'a packages::Model,
    version: &'a str,
    graph: &'a DependencyGraph,
    trigger: BuildTrigger,
}

/// Outcome of triggering an update for a single platform of a package.
#[derive(Debug, Clone)]
pub struct PlatformUpdateResult {
    pub platform: Platform,
    /// The build row created/reused for this platform. Present regardless of
    /// whether the build was actually dispatched or is still waiting on a
    /// dependency rebuild.
    pub build_id: i32,
    /// The same build's public number within its package. Callers that report
    /// back to a user want this rather than `build_id`, which is internal.
    pub build_number: i32,
    /// `true` if the build was enqueued/promoted and dispatched to the builder;
    /// `false` if it was left `WAITING_FOR_DEPS` pending an unfinished
    /// dependency rebuild.
    pub enqueued: bool,
}

/// For each configured platform, check dep readiness and enqueue builds.
async fn enqueue_platform_builds(
    services: &Services,
    request: BuildRequest<'_>,
    visited: &mut HashSet<i32>,
) -> anyhow::Result<Vec<PlatformUpdateResult>> {
    let configured_platforms =
        Platform::parse_many(&request.pkg_model.platforms).collect::<Result<Vec<_>, _>>()?;

    let mut results = Vec::new();

    for platform in &configured_platforms {
        let ready = dependencies_ready_for_platform(
            services,
            platform,
            request.graph,
            request.trigger,
            visited,
        )
        .await?;

        if ready {
            let result = update_platform(
                *platform,
                request.pkg_model.clone(),
                request.version.to_string(),
                request.trigger,
                &services.db,
                &services.tx,
            )
            .await?;
            results.push(PlatformUpdateResult {
                platform: *platform,
                build_id: result.build.id,
                build_number: result.build.number,
                enqueued: result.inserted,
            });
        } else {
            let txn = services.db.begin().await?;
            let start_time = aurcache_db::helpers::time::now_secs();
            let waiting = enqueue_build_if_missing(
                &txn,
                request.pkg_model.id,
                *platform,
                request.version,
                start_time,
                BuildStates::WAITING_FOR_DEPS,
                request.trigger.as_i32(),
            )
            .await?;
            txn.commit().await?;
            results.push(PlatformUpdateResult {
                platform: *platform,
                build_id: waiting.build.id,
                build_number: waiting.build.number,
                enqueued: false,
            });
        }
    }

    Ok(results)
}

/// Create or reuse the pending build entry for a package on one platform.
///
/// If a `WAITING_FOR_DEPS` build already exists for this `(pkg, platform)`, it is promoted to
/// `ENQUEUED` and dispatched rather than inserting a duplicate.  This happens when a dependency
/// finishes and the dependent was already in the pending queue waiting for it.
pub async fn update_platform(
    platform: Platform,
    pkg: packages::Model,
    new_version: String,
    trigger: BuildTrigger,
    db: &DatabaseConnection,
    tx: &Sender<Action>,
) -> anyhow::Result<aurcache_db::helpers::build_enqueue::EnqueueBuildResult> {
    // Fast path: promote an existing WAITING_FOR_DEPS build if one is present.
    if let Some(promoted) = promote_waiting_build(db, pkg.id, platform).await? {
        let _ = tx.send(Action::Build(Box::new(pkg), Box::new(promoted.clone())));
        return Ok(aurcache_db::helpers::build_enqueue::EnqueueBuildResult {
            build: promoted,
            inserted: true,
        });
    }

    let txn = db.begin().await?;
    let start_time = aurcache_db::helpers::time::now_secs();
    let enqueue_result = enqueue_build_if_missing(
        &txn,
        pkg.id,
        platform,
        &new_version,
        start_time,
        BuildStates::ENQUEUED_BUILD,
        trigger.as_i32(),
    )
    .await?;
    txn.commit().await?;

    if enqueue_result.inserted {
        let _ = tx.send(Action::Build(
            Box::new(pkg),
            Box::new(enqueue_result.build.clone()),
        ));
    }
    Ok(enqueue_result)
}

#[cfg(test)]
mod tests {
    use super::package_update;
    use aurcache_activitylog::activity_utils::ActivityLog;
    use aurcache_common::build_state::{BuildTrigger, BuildTriggers};
    /// A repository of its own for a test that never publishes to it.
    fn test_repo() -> Arc<crate::repository::Repository> {
        Arc::new(crate::repository::Repository::new(
            tempdir().unwrap().keep(),
        ))
    }
    use crate::services::Services;
    use crate::snapshot::SnapshotStore;
    use aurcache_common::builder::BuildStates;
    use aurcache_db::action::Action;
    use aurcache_db::migration::Migrator;
    use aurcache_db::packages::SourceData;
    use aurcache_db::prelude::{Dependencies, Packages};
    use aurcache_db::{builds, dependencies, packages};
    use aurcache_deps::AurClient;
    use git2::{Repository, Signature};
    use pacman_mirrors::platforms::Platform;
    use sea_orm::DatabaseConnection;
    use sea_orm::{
        ActiveModelTrait, ColumnTrait, Database, EntityTrait, PaginatorTrait, QueryFilter, Set,
        TryIntoModel,
    };
    use sea_orm_migration::MigratorTrait;
    use serde_json::json;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use tempfile::tempdir;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path, query_param},
    };

    fn rpc_deps_json(
        name: &str,
        pkgbase: &str,
        depends: &[&str],
        make_depends: &[&str],
        version: &str,
    ) -> serde_json::Value {
        json!({
            "Name": name,
            "Version": version,
            "PackageBase": pkgbase,
            "PackageBaseID": 0,
            "ID": 0,
            "NumVotes": 0,
            "Popularity": 0.0,
            "FirstSubmitted": 0,
            "LastModified": 0,
            "URLPath": null,
            "Description": null,
            "Maintainer": null,
            "URL": null,
            "OutOfDate": null,
            "Depends": depends,
            "MakeDepends": make_depends,
            "OptDepends": null,
            "CheckDepends": null,
            "Conflicts": null,
            "Provides": null,
            "Replaces": null,
            "Groups": null,
            "License": null,
            "Keywords": null,
        })
    }

    fn multiinfo_json(results: Vec<serde_json::Value>) -> serde_json::Value {
        json!({
            "type": "multiinfo",
            "resultcount": results.len(),
            "results": results,
        })
    }

    fn make_srcinfo(pkgbase: &str, version: &str, depends: &[&str]) -> String {
        let dep_lines = depends
            .iter()
            .map(|d| format!("    depends = {d}\n"))
            .collect::<String>();
        format!(
            "pkgbase = {pkgbase}\n\
             pkgver = {version}\n\
             pkgrel = 1\n\
             arch = x86_64\n\
             {dep_lines}\n\
             pkgname = {pkgbase}\n"
        )
    }

    /// Create a local bare-ish git repository at `aur_root/{pkgbase}.git`
    /// containing a PKGBUILD + .SRCINFO, standing in for the real AUR git
    /// remote (`https://aur.archlinux.org/{pkgbase}.git`) in tests. Returns
    /// the repo's filesystem path, usable directly as a git remote URL.
    fn create_aur_git_repo(
        aur_root: &Path,
        pkgbase: &str,
        version: &str,
        depends: &[&str],
    ) -> PathBuf {
        let repo_path = aur_root.join(format!("{pkgbase}.git"));
        let repo = Repository::init(&repo_path).unwrap();

        let srcinfo = make_srcinfo(pkgbase, version, depends);
        let depends_arr = depends
            .iter()
            .map(|dep| format!("'{dep}'"))
            .collect::<Vec<_>>()
            .join(" ");
        let pkgbuild = format!(
            "pkgname={pkgbase}\npkgver={version}\npkgrel=1\narch=('x86_64')\ndepends=({depends_arr})\nsource=()\nsha256sums=()\npackage() {{\n  :\n}}\n"
        );

        fs::write(repo_path.join("PKGBUILD"), pkgbuild).unwrap();
        fs::write(repo_path.join(".SRCINFO"), srcinfo).unwrap();

        let mut index = repo.index().unwrap();
        index.add_path(Path::new("PKGBUILD")).unwrap();
        index.add_path(Path::new(".SRCINFO")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = Signature::now("Test", "test@example.com").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();

        repo_path
    }

    /// Build a `SnapshotStore` for tests: AUR sources resolve against local
    /// git repos under `aur_root` instead of the real AUR, and checkouts are
    /// kept under a fresh temp dir.
    fn test_store(aur_root: &Path) -> (SnapshotStore, tempfile::TempDir) {
        let checkout_dir = tempdir().unwrap();
        let store = SnapshotStore::with_checkout_root_and_aur_base(
            checkout_dir.path().to_path_buf(),
            aur_root.to_string_lossy().to_string(),
        );
        (store, checkout_dir)
    }

    fn git_pkgbuild(version: &str, depends: &[&str]) -> String {
        let depends = depends
            .iter()
            .map(|dep| format!("'{dep}'"))
            .collect::<Vec<_>>()
            .join(" ");
        format!(
            "pkgname=git-parent\npkgver={version}\npkgrel=1\narch=('x86_64')\ndepends=({depends})\nsource=()\nsha256sums=()\npackage() {{\n  :\n}}\n"
        )
    }

    fn git_srcinfo(version: &str, depends: &[&str]) -> String {
        let depends = depends
            .iter()
            .map(|dep| format!("    depends = {dep}\n"))
            .collect::<String>();
        format!(
            "pkgbase = git-parent\n    pkgver = {version}\n    pkgrel = 1\n    arch = x86_64\n{depends}\npkgname = git-parent\n"
        )
    }

    fn commit_pkgbuild(repo: &Repository, message: &str, version: &str, depends: &[&str]) {
        fs::write(
            repo.workdir().unwrap().join("PKGBUILD"),
            git_pkgbuild(version, depends),
        )
        .unwrap();
        fs::write(
            repo.workdir().unwrap().join(".SRCINFO"),
            git_srcinfo(version, depends),
        )
        .unwrap();

        let mut index = repo.index().unwrap();
        index.add_path(Path::new("PKGBUILD")).unwrap();
        index.add_path(Path::new(".SRCINFO")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = Signature::now("Test", "test@example.com").unwrap();
        let parents = repo
            .head()
            .ok()
            .and_then(|head| head.target())
            .and_then(|oid| repo.find_commit(oid).ok())
            .map(|commit| vec![commit])
            .unwrap_or_default();
        let parent_refs = parents.iter().collect::<Vec<_>>();
        repo.commit(
            Some("refs/heads/main"),
            &sig,
            &sig,
            message,
            &tree,
            &parent_refs,
        )
        .unwrap();
        repo.set_head("refs/heads/main").unwrap();
        repo.checkout_head(None).unwrap();
    }

    /// A client whose official-repo cache is present and empty.
    ///
    /// Resolution asks the official repositories about every dependency, and
    /// an unreadable cache is an error rather than an empty answer -- that is
    /// what stops a mirror outage from sending `git` to the AUR. A test that
    /// wants the repositories to hold nothing therefore has to say so, by
    /// handing over a cache that is present, fresh and empty.
    async fn client_with_empty_official_repos(rpc_url: String) -> (AurClient, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        for repo in ["core", "extra", "multilib"] {
            let mut archive = Vec::new();
            {
                let encoder =
                    flate2::write::GzEncoder::new(&mut archive, flate2::Compression::default());
                let mut builder = tar::Builder::new(encoder);
                builder.finish().unwrap();
                builder.into_inner().unwrap().finish().unwrap();
            }
            std::fs::write(dir.path().join(format!("{repo}.db.tar.gz")), &archive).unwrap();
        }
        let client = AurClient::with_urls_and_paths(
            rpc_url,
            // Never read: nothing in the cache is stale, so no download is
            // attempted and no mirror is needed.
            dir.path().join("no-mirrorlist"),
            dir.path().to_path_buf(),
        );
        // Present and empty: "the official repositories hold nothing", as
        // opposed to "they could not be read", which resolution refuses to
        // answer.
        client.official.refresh().await.unwrap();
        (client, dir)
    }

    #[tokio::test]
    async fn package_update_queues_dependency_builds_before_parent_when_constraints_tighten() {
        let server = MockServer::start().await;
        let (client, _official) =
            client_with_empty_official_repos(format!("{}/rpc/v5", server.uri())).await;
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        let (tx, _) = tokio::sync::broadcast::channel::<Action>(100);

        let aur_root = tempdir().unwrap();
        create_aur_git_repo(aur_root.path(), "parent", "2.0.0", &["child>=2.0"]);
        create_aur_git_repo(aur_root.path(), "child", "2.0.0", &[]);

        Mock::given(method("GET"))
            .and(path("/rpc/v5/info"))
            .and(query_param("arg[]", "child"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(multiinfo_json(vec![rpc_deps_json(
                    "child",
                    "child",
                    &[],
                    &[],
                    "2.0.0",
                )])),
            )
            .mount(&server)
            .await;

        let parent = packages::ActiveModel {
            name: Set("parent".to_string()),
            status: Set(BuildStates::SUCCESSFUL_BUILD),
            out_of_date: Set(0),
            upstream_version: Set(Some("1.0.0".to_string())),
            latest_build: Set(None),
            build_flags: Set("--noconfirm;--noprogressbar".to_string()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(packages::SourceType::Aur),
            source_data: Set(SourceData::Aur {
                name: "parent".into(),
            }),
            directly_requested: Set(true),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap()
        .try_into_model()
        .unwrap();

        let child = packages::ActiveModel {
            name: Set("child".to_string()),
            status: Set(BuildStates::SUCCESSFUL_BUILD),
            out_of_date: Set(0),
            upstream_version: Set(Some("1.0.0".to_string())),
            latest_build: Set(None),
            build_flags: Set("--noconfirm;--noprogressbar".to_string()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(packages::SourceType::Aur),
            source_data: Set(SourceData::Aur {
                name: "child".into(),
            }),
            directly_requested: Set(false),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap()
        .try_into_model()
        .unwrap();

        dependencies::ActiveModel {
            dependent_id: Set(parent.id),
            dependee_id: Set(child.id),
            version_constraint: Set(">=1.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        builds::ActiveModel {
            pkg_id: Set(child.id),
            status: Set(Some(BuildStates::SUCCESSFUL_BUILD)),
            start_time: Set(Some(1)),
            end_time: Set(Some(2)),
            platform: Set(Platform::X86_64),
            version: Set("1.0.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        let (store, _checkout_dir) = test_store(aur_root.path());
        let results = package_update(
            &Services::new(
                db.clone(),
                tx.clone(),
                Arc::new(store),
                Arc::new(client),
                test_repo(),
                ActivityLog::discarding(),
            ),
            parent.clone(),
            false,
            BuildTrigger::User,
        )
        .await
        .unwrap();

        assert!(
            results.iter().all(|r| !r.enqueued),
            "parent should wait for dependency rebuild"
        );

        let updated_dep = Dependencies::find()
            .filter(dependencies::Column::DependentId.eq(parent.id))
            .filter(dependencies::Column::DependeeId.eq(child.id))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated_dep.version_constraint, ">=2.0");

        let parent_builds = builds::Entity::find()
            .filter(builds::Column::PkgId.eq(parent.id))
            .all(&db)
            .await
            .unwrap();
        assert_eq!(
            parent_builds.len(),
            1,
            "parent should have a WAITING_FOR_DEPS build while dependency rebuilds"
        );
        assert_eq!(
            parent_builds[0].status,
            Some(BuildStates::WAITING_FOR_DEPS),
            "parent build should be WAITING_FOR_DEPS"
        );
        // The request's own trigger, on the build left waiting as much as on
        // one queued outright -- it used to be `AutoUpdate` for everything,
        // which made an operator's rebuild indistinguishable from the scheduler.
        assert_eq!(parent_builds[0].trigger, BuildTriggers::USER);

        let child_builds = builds::Entity::find()
            .filter(builds::Column::PkgId.eq(child.id))
            .all(&db)
            .await
            .unwrap();
        assert_eq!(
            child_builds.len(),
            2,
            "dependency should get a new rebuild queued"
        );
        let rebuild = child_builds
            .iter()
            .find(|b| b.status != Some(BuildStates::SUCCESSFUL_BUILD))
            .expect("the dependency's new build");
        assert_eq!(
            rebuild.trigger,
            BuildTriggers::USER,
            "a dependency rebuilt for the request is part of the request"
        );

        let parent_after = Packages::find_by_id(parent.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(parent_after.status, BuildStates::WAITING_FOR_DEPS);
        assert_eq!(parent_after.upstream_version.as_deref(), Some("2.0.0-1"));
    }

    #[tokio::test]
    async fn package_update_does_not_queue_non_leaf_dependency_builds() {
        let server = MockServer::start().await;
        let (client, _official) =
            client_with_empty_official_repos(format!("{}/rpc/v5", server.uri())).await;
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        let (tx, _) = tokio::sync::broadcast::channel::<Action>(100);

        let aur_root = tempdir().unwrap();
        create_aur_git_repo(aur_root.path(), "parent", "2.0.0", &["child>=2.0"]);
        create_aur_git_repo(aur_root.path(), "child", "2.0.0", &["grandchild>=2.0"]);
        create_aur_git_repo(aur_root.path(), "grandchild", "2.0.0", &[]);

        Mock::given(method("GET"))
            .and(path("/rpc/v5/info"))
            .and(query_param("arg[]", "child"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(multiinfo_json(vec![rpc_deps_json(
                    "child",
                    "child",
                    &["grandchild>=2.0"],
                    &[],
                    "2.0.0",
                )])),
            )
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/rpc/v5/info"))
            .and(query_param("arg[]", "grandchild"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(multiinfo_json(vec![rpc_deps_json(
                    "grandchild",
                    "grandchild",
                    &[],
                    &[],
                    "2.0.0",
                )])),
            )
            .mount(&server)
            .await;

        let parent = packages::ActiveModel {
            name: Set("parent".to_string()),
            status: Set(BuildStates::SUCCESSFUL_BUILD),
            out_of_date: Set(0),
            upstream_version: Set(Some("1.0.0".to_string())),
            latest_build: Set(None),
            build_flags: Set("--noconfirm;--noprogressbar".to_string()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(packages::SourceType::Aur),
            source_data: Set(SourceData::Aur {
                name: "parent".into(),
            }),
            directly_requested: Set(true),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap()
        .try_into_model()
        .unwrap();

        let child = packages::ActiveModel {
            name: Set("child".to_string()),
            status: Set(BuildStates::SUCCESSFUL_BUILD),
            out_of_date: Set(0),
            upstream_version: Set(Some("1.0.0".to_string())),
            latest_build: Set(None),
            build_flags: Set("--noconfirm;--noprogressbar".to_string()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(packages::SourceType::Aur),
            source_data: Set(SourceData::Aur {
                name: "child".into(),
            }),
            directly_requested: Set(false),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap()
        .try_into_model()
        .unwrap();

        let grandchild = packages::ActiveModel {
            name: Set("grandchild".to_string()),
            status: Set(BuildStates::SUCCESSFUL_BUILD),
            out_of_date: Set(0),
            upstream_version: Set(Some("1.0.0".to_string())),
            latest_build: Set(None),
            build_flags: Set("--noconfirm;--noprogressbar".to_string()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(packages::SourceType::Aur),
            source_data: Set(SourceData::Aur {
                name: "grandchild".into(),
            }),
            directly_requested: Set(false),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap()
        .try_into_model()
        .unwrap();

        dependencies::ActiveModel {
            dependent_id: Set(parent.id),
            dependee_id: Set(child.id),
            version_constraint: Set(">=1.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        dependencies::ActiveModel {
            dependent_id: Set(child.id),
            dependee_id: Set(grandchild.id),
            version_constraint: Set(">=1.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        builds::ActiveModel {
            pkg_id: Set(child.id),
            status: Set(Some(BuildStates::SUCCESSFUL_BUILD)),
            start_time: Set(Some(1)),
            end_time: Set(Some(2)),
            platform: Set(Platform::X86_64),
            version: Set("1.0.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        builds::ActiveModel {
            pkg_id: Set(grandchild.id),
            status: Set(Some(BuildStates::SUCCESSFUL_BUILD)),
            start_time: Set(Some(1)),
            end_time: Set(Some(2)),
            platform: Set(Platform::X86_64),
            version: Set("1.0.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        let (store, _checkout_dir) = test_store(aur_root.path());
        let results = package_update(
            &Services::new(
                db.clone(),
                tx.clone(),
                Arc::new(store),
                Arc::new(client),
                test_repo(),
                ActivityLog::discarding(),
            ),
            parent.clone(),
            false,
            BuildTrigger::User,
        )
        .await
        .unwrap();

        assert!(
            results.iter().all(|r| !r.enqueued),
            "parent should wait for transitive dependency rebuilds"
        );

        let parent_builds = builds::Entity::find()
            .filter(builds::Column::PkgId.eq(parent.id))
            .all(&db)
            .await
            .unwrap();
        assert_eq!(
            parent_builds.len(),
            1,
            "parent should have a WAITING_FOR_DEPS build while transitive dependency rebuilds"
        );
        assert_eq!(
            parent_builds[0].status,
            Some(BuildStates::WAITING_FOR_DEPS),
            "parent build should be WAITING_FOR_DEPS"
        );

        let child_builds = builds::Entity::find()
            .filter(builds::Column::PkgId.eq(child.id))
            .all(&db)
            .await
            .unwrap();
        assert_eq!(
            child_builds.len(),
            2,
            "non-leaf dependency should get a WAITING_FOR_DEPS build while grandchild rebuilds"
        );
        let child_pending = child_builds
            .iter()
            .find(|b| b.status == Some(BuildStates::WAITING_FOR_DEPS));
        assert!(
            child_pending.is_some(),
            "child should have a WAITING_FOR_DEPS build"
        );

        let grandchild_build_count = builds::Entity::find()
            .filter(builds::Column::PkgId.eq(grandchild.id))
            .count(&db)
            .await
            .unwrap();
        assert_eq!(
            grandchild_build_count, 2,
            "leaf transitive dependency should be queued first"
        );

        let child_after = Packages::find_by_id(child.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(child_after.status, BuildStates::WAITING_FOR_DEPS);
        assert_eq!(child_after.upstream_version.as_deref(), Some("2.0.0-1"));
    }

    #[tokio::test]
    async fn force_rebuild_does_not_queue_non_leaf_dependency_builds() {
        let server = MockServer::start().await;
        let (client, _official) =
            client_with_empty_official_repos(format!("{}/rpc/v5", server.uri())).await;
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        let (tx, _) = tokio::sync::broadcast::channel::<Action>(100);

        let aur_root = tempdir().unwrap();
        create_aur_git_repo(aur_root.path(), "parent", "2.0.0", &["child>=2.0"]);
        create_aur_git_repo(aur_root.path(), "child", "2.0.0", &["grandchild>=2.0"]);
        create_aur_git_repo(aur_root.path(), "grandchild", "2.0.0", &[]);

        Mock::given(method("GET"))
            .and(path("/rpc/v5/info"))
            .and(query_param("arg[]", "child"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(multiinfo_json(vec![rpc_deps_json(
                    "child",
                    "child",
                    &["grandchild>=2.0"],
                    &[],
                    "2.0.0",
                )])),
            )
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/rpc/v5/info"))
            .and(query_param("arg[]", "grandchild"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(multiinfo_json(vec![rpc_deps_json(
                    "grandchild",
                    "grandchild",
                    &[],
                    &[],
                    "2.0.0",
                )])),
            )
            .mount(&server)
            .await;

        let parent = packages::ActiveModel {
            name: Set("parent".to_string()),
            status: Set(BuildStates::SUCCESSFUL_BUILD),
            out_of_date: Set(0),
            upstream_version: Set(Some("1.0.0".to_string())),
            latest_build: Set(None),
            build_flags: Set("--noconfirm;--noprogressbar".to_string()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(packages::SourceType::Aur),
            source_data: Set(SourceData::Aur {
                name: "parent".into(),
            }),
            directly_requested: Set(true),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap()
        .try_into_model()
        .unwrap();

        let child = packages::ActiveModel {
            name: Set("child".to_string()),
            status: Set(BuildStates::SUCCESSFUL_BUILD),
            out_of_date: Set(0),
            upstream_version: Set(Some("1.0.0".to_string())),
            latest_build: Set(None),
            build_flags: Set("--noconfirm;--noprogressbar".to_string()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(packages::SourceType::Aur),
            source_data: Set(SourceData::Aur {
                name: "child".into(),
            }),
            directly_requested: Set(false),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap()
        .try_into_model()
        .unwrap();

        let grandchild = packages::ActiveModel {
            name: Set("grandchild".to_string()),
            status: Set(BuildStates::SUCCESSFUL_BUILD),
            out_of_date: Set(0),
            upstream_version: Set(Some("1.0.0".to_string())),
            latest_build: Set(None),
            build_flags: Set("--noconfirm;--noprogressbar".to_string()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(packages::SourceType::Aur),
            source_data: Set(SourceData::Aur {
                name: "grandchild".into(),
            }),
            directly_requested: Set(false),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap()
        .try_into_model()
        .unwrap();

        dependencies::ActiveModel {
            dependent_id: Set(parent.id),
            dependee_id: Set(child.id),
            version_constraint: Set(">=1.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        dependencies::ActiveModel {
            dependent_id: Set(child.id),
            dependee_id: Set(grandchild.id),
            version_constraint: Set(">=1.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        builds::ActiveModel {
            pkg_id: Set(child.id),
            status: Set(Some(BuildStates::SUCCESSFUL_BUILD)),
            start_time: Set(Some(1)),
            end_time: Set(Some(2)),
            platform: Set(Platform::X86_64),
            version: Set("1.0.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        builds::ActiveModel {
            pkg_id: Set(grandchild.id),
            status: Set(Some(BuildStates::SUCCESSFUL_BUILD)),
            start_time: Set(Some(1)),
            end_time: Set(Some(2)),
            platform: Set(Platform::X86_64),
            version: Set("1.0.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        let (store, _checkout_dir) = test_store(aur_root.path());
        let results = package_update(
            &Services::new(
                db.clone(),
                tx.clone(),
                Arc::new(store),
                Arc::new(client),
                test_repo(),
                ActivityLog::discarding(),
            ),
            parent.clone(),
            true,
            BuildTrigger::User,
        )
        .await
        .unwrap();

        assert!(
            results.iter().all(|r| !r.enqueued),
            "forced rebuild should still wait for transitive dependency rebuilds"
        );

        let parent_builds = builds::Entity::find()
            .filter(builds::Column::PkgId.eq(parent.id))
            .all(&db)
            .await
            .unwrap();
        assert_eq!(
            parent_builds.len(),
            1,
            "forced rebuild should insert a WAITING_FOR_DEPS build while transitive deps rebuild"
        );
        assert_eq!(
            parent_builds[0].status,
            Some(BuildStates::WAITING_FOR_DEPS),
            "parent build should be WAITING_FOR_DEPS"
        );

        let child_builds = builds::Entity::find()
            .filter(builds::Column::PkgId.eq(child.id))
            .all(&db)
            .await
            .unwrap();
        assert_eq!(
            child_builds.len(),
            2,
            "forced rebuild should give non-leaf dependency a WAITING_FOR_DEPS build"
        );
        let child_pending = child_builds
            .iter()
            .find(|b| b.status == Some(BuildStates::WAITING_FOR_DEPS));
        assert!(
            child_pending.is_some(),
            "child should have a WAITING_FOR_DEPS build during forced rebuild"
        );

        let grandchild_build_count = builds::Entity::find()
            .filter(builds::Column::PkgId.eq(grandchild.id))
            .count(&db)
            .await
            .unwrap();
        assert_eq!(
            grandchild_build_count, 2,
            "forced rebuild should enqueue only the leaf transitive dependency first"
        );
    }

    #[tokio::test]
    async fn git_update_refreshes_dependency_rows() {
        let server = MockServer::start().await;
        let (client, _official) =
            client_with_empty_official_repos(format!("{}/rpc/v5", server.uri())).await;
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        let (tx, _) = tokio::sync::broadcast::channel::<Action>(100);

        Mock::given(method("GET"))
            .and(path("/rpc/v5/info"))
            .and(query_param("arg[]", "new-dep"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(multiinfo_json(vec![rpc_deps_json(
                    "new-dep",
                    "new-dep",
                    &[],
                    &[],
                    "2.0.0",
                )])),
            )
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        commit_pkgbuild(&repo, "initial", "1.0.0", &["old-dep>=1.0"]);

        let parent = packages::ActiveModel {
            name: Set("git-parent".to_string()),
            status: Set(BuildStates::SUCCESSFUL_BUILD),
            out_of_date: Set(0),
            upstream_version: Set(Some("1.0.0".to_string())),
            latest_build: Set(None),
            build_flags: Set("--noconfirm;--noprogressbar".to_string()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(packages::SourceType::Git),
            source_data: Set(packages::SourceData::Git {
                spec: packages::GitSourceSpec {
                    url: dir.path().to_string_lossy().to_string(),
                    r#ref: "main".to_string(),
                    subfolder: ".".to_string(),
                },
            }),
            directly_requested: Set(true),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap()
        .try_into_model()
        .unwrap();

        let old_dep = packages::ActiveModel {
            name: Set("old-dep".to_string()),
            status: Set(BuildStates::SUCCESSFUL_BUILD),
            out_of_date: Set(0),
            upstream_version: Set(Some("1.0.0".to_string())),
            latest_build: Set(None),
            build_flags: Set("--noconfirm;--noprogressbar".to_string()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(packages::SourceType::Aur),
            source_data: Set(SourceData::Aur {
                name: "old-dep".into(),
            }),
            directly_requested: Set(false),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap()
        .try_into_model()
        .unwrap();

        let new_dep = packages::ActiveModel {
            name: Set("new-dep".to_string()),
            status: Set(BuildStates::SUCCESSFUL_BUILD),
            out_of_date: Set(0),
            upstream_version: Set(Some("2.0.0".to_string())),
            latest_build: Set(None),
            build_flags: Set("--noconfirm;--noprogressbar".to_string()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(packages::SourceType::Aur),
            source_data: Set(SourceData::Aur {
                name: "new-dep".into(),
            }),
            directly_requested: Set(false),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap()
        .try_into_model()
        .unwrap();

        dependencies::ActiveModel {
            dependent_id: Set(parent.id),
            dependee_id: Set(old_dep.id),
            version_constraint: Set(">=1.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        builds::ActiveModel {
            pkg_id: Set(parent.id),
            status: Set(Some(BuildStates::SUCCESSFUL_BUILD)),
            start_time: Set(Some(1)),
            end_time: Set(Some(2)),
            platform: Set(Platform::X86_64),
            version: Set("1.0.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        builds::ActiveModel {
            pkg_id: Set(new_dep.id),
            status: Set(Some(BuildStates::SUCCESSFUL_BUILD)),
            start_time: Set(Some(1)),
            end_time: Set(Some(2)),
            platform: Set(Platform::X86_64),
            version: Set("2.0.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        commit_pkgbuild(&repo, "updated", "2.0.0", &["new-dep>=2.0"]);

        let checkout_dir = tempdir().unwrap();
        let store = SnapshotStore::with_checkout_root(checkout_dir.path().to_path_buf());
        let build_ids = package_update(
            &Services::new(
                db.clone(),
                tx.clone(),
                Arc::new(store),
                Arc::new(client),
                test_repo(),
                ActivityLog::discarding(),
            ),
            parent.clone(),
            false,
            BuildTrigger::User,
        )
        .await
        .unwrap();

        assert_eq!(
            build_ids.len(),
            1,
            "parent should enqueue once deps are refreshed"
        );

        let deps = Dependencies::find()
            .filter(dependencies::Column::DependentId.eq(parent.id))
            .all(&db)
            .await
            .unwrap();
        assert_eq!(deps.len(), 1, "stale git dependency rows should be removed");
        assert_eq!(deps[0].dependee_id, new_dep.id);
        assert_eq!(deps[0].version_constraint, ">=2.0");

        let parent_after = Packages::find_by_id(parent.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(parent_after.upstream_version.as_deref(), Some("2.0.0-1"));
    }

    /// Set up a git package declaring `mydep`, with two tracked packages able
    /// to satisfy it: `mydep`, which carries the name, and `mydep-git`, which
    /// only provides it. Ranking prefers the first; the returned ids are
    /// `(parent, mydep, mydep-git)`.
    async fn two_providers_of_one_name(
        db: &DatabaseConnection,
        repo_path: &Path,
    ) -> (packages::Model, i32, i32) {
        let parent = packages::ActiveModel {
            name: Set("git-parent".to_string()),
            status: Set(BuildStates::SUCCESSFUL_BUILD),
            out_of_date: Set(0),
            upstream_version: Set(Some("1.0.0".to_string())),
            latest_build: Set(None),
            build_flags: Set(String::new()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(packages::SourceType::Git),
            source_data: Set(packages::SourceData::Git {
                spec: packages::GitSourceSpec {
                    url: repo_path.to_string_lossy().to_string(),
                    r#ref: "main".to_string(),
                    subfolder: String::new(),
                },
            }),
            directly_requested: Set(true),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(db)
        .await
        .unwrap()
        .try_into_model()
        .unwrap();

        let provider = async |name: &str, provides: Option<serde_json::Value>| {
            packages::ActiveModel {
                name: Set(name.to_string()),
                status: Set(BuildStates::SUCCESSFUL_BUILD),
                out_of_date: Set(0),
                upstream_version: Set(Some("1.0.0".to_string())),
                latest_build: Set(None),
                build_flags: Set(String::new()),
                platforms: Set("x86_64".to_string()),
                source_type: Set(packages::SourceType::Aur),
                source_data: Set(SourceData::Aur { name: name.into() }),
                // Requested, so the loser is not swept as an orphan and the
                // assertion is about the edge and nothing else.
                directly_requested: Set(true),
                split_packages: Set(None),
                provides: Set(provides.map(|value| value.to_string())),
                ..Default::default()
            }
            .save(db)
            .await
            .unwrap()
            .id
            .unwrap()
        };

        let by_name = provider("mydep", None).await;
        let by_provides = provider("mydep-git", Some(json!(["mydep"]))).await;
        (parent, by_name, by_provides)
    }

    /// A dependency someone repointed by hand survives re-resolution.
    ///
    /// Every edge is recomputed from the declared names, so the existing edge
    /// has to be an input to that or the ranking takes the choice straight
    /// back: `mydep` carries the name and `mydep-git` only provides it.
    #[tokio::test]
    async fn a_repointed_dependency_survives_a_resync() {
        let server = MockServer::start().await;
        let (client, _official) =
            client_with_empty_official_repos(format!("{}/rpc/v5", server.uri())).await;
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        let (tx, _) = tokio::sync::broadcast::channel::<Action>(100);

        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        commit_pkgbuild(&repo, "initial", "1.0.0", &["mydep"]);

        let (parent, _by_name, by_provides) = two_providers_of_one_name(&db, dir.path()).await;

        // The hand-picked edge, as an "update this link" action would leave it.
        dependencies::ActiveModel {
            dependent_id: Set(parent.id),
            dependee_id: Set(by_provides),
            version_constraint: Set(String::new()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        let checkout_dir = tempdir().unwrap();
        let store = SnapshotStore::with_checkout_root(checkout_dir.path().to_path_buf());
        super::package_resync_dependencies(
            &Services::new(
                db.clone(),
                tx.clone(),
                Arc::new(store),
                Arc::new(client),
                test_repo(),
                ActivityLog::discarding(),
            ),
            &parent,
        )
        .await
        .unwrap();

        let deps = Dependencies::find()
            .filter(dependencies::Column::DependentId.eq(parent.id))
            .all(&db)
            .await
            .unwrap();
        assert_eq!(deps.len(), 1);
        assert_eq!(
            deps[0].dependee_id, by_provides,
            "the existing edge should outrank the better-ranked candidate"
        );
    }

    /// The control: with no edge to prefer, the same two candidates resolve
    /// the other way round. Without this the test above would pass on a
    /// resolver that simply never changed its mind.
    #[tokio::test]
    async fn ranking_picks_the_provider_when_there_is_no_edge_yet() {
        let server = MockServer::start().await;
        let (client, _official) =
            client_with_empty_official_repos(format!("{}/rpc/v5", server.uri())).await;
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        let (tx, _) = tokio::sync::broadcast::channel::<Action>(100);

        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        commit_pkgbuild(&repo, "initial", "1.0.0", &["mydep"]);

        let (parent, by_name, _by_provides) = two_providers_of_one_name(&db, dir.path()).await;

        let checkout_dir = tempdir().unwrap();
        let store = SnapshotStore::with_checkout_root(checkout_dir.path().to_path_buf());
        super::package_resync_dependencies(
            &Services::new(
                db.clone(),
                tx.clone(),
                Arc::new(store),
                Arc::new(client),
                test_repo(),
                ActivityLog::discarding(),
            ),
            &parent,
        )
        .await
        .unwrap();

        let deps = Dependencies::find()
            .filter(dependencies::Column::DependentId.eq(parent.id))
            .all(&db)
            .await
            .unwrap();
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].dependee_id, by_name);
    }

    #[tokio::test]
    async fn force_rebuild_after_failure_queues_new_build() {
        let server = MockServer::start().await;
        let (client, _official) =
            client_with_empty_official_repos(format!("{}/rpc/v5", server.uri())).await;
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        let (tx, mut rx) = tokio::sync::broadcast::channel::<Action>(100);

        let aur_root = tempdir().unwrap();
        create_aur_git_repo(aur_root.path(), "mypkg", "1.0.0", &[]);

        // Simulate package that previously failed its first build.
        let pkg = packages::ActiveModel {
            name: Set("mypkg".to_string()),
            status: Set(BuildStates::FAILED_BUILD),
            out_of_date: Set(0),
            upstream_version: Set(Some("1.0.0".to_string())),
            latest_build: Set(None),
            build_flags: Set("--noconfirm;--noprogressbar".to_string()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(packages::SourceType::Aur),
            source_data: Set(SourceData::Aur {
                name: "mypkg".into(),
            }),
            directly_requested: Set(true),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap()
        .try_into_model()
        .unwrap();

        // The first (failed) build record.
        builds::ActiveModel {
            pkg_id: Set(pkg.id),
            status: Set(Some(BuildStates::FAILED_BUILD)),
            start_time: Set(Some(1)),
            end_time: Set(Some(2)),
            platform: Set(Platform::X86_64),
            version: Set("1.0.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        // Drain any stale messages before the force-rebuild call.
        while rx.try_recv().is_ok() {}

        let (store, _checkout_dir) = test_store(aur_root.path());
        let build_ids = package_update(
            &Services::new(
                db.clone(),
                tx.clone(),
                Arc::new(store),
                Arc::new(client),
                test_repo(),
                ActivityLog::discarding(),
            ),
            pkg.clone(),
            true,
            BuildTrigger::User,
        )
        .await
        .unwrap();

        assert_eq!(
            build_ids.len(),
            1,
            "force rebuild should queue exactly one build"
        );

        // Verify Action::Build was sent.
        assert!(
            rx.try_recv().is_ok(),
            "Action::Build should have been sent on the channel"
        );

        // Verify the new build row exists with ENQUEUED status.
        let enqueued_build = builds::Entity::find()
            .filter(builds::Column::PkgId.eq(pkg.id))
            .filter(builds::Column::Status.eq(Some(BuildStates::ENQUEUED_BUILD)))
            .one(&db)
            .await
            .unwrap();
        assert!(
            enqueued_build.is_some(),
            "a new ENQUEUED build row should exist after force rebuild"
        );

        // The total build count should be 2 (the original failed + new enqueued).
        let total_builds = builds::Entity::find()
            .filter(builds::Column::PkgId.eq(pkg.id))
            .count(&db)
            .await
            .unwrap();
        assert_eq!(total_builds, 2, "there should be 2 build records total");
    }

    /// Auto-update rebuilds an unchanged version only for a package that tracks
    /// VCS sources. For anything else the flag with no newer source is a
    /// snapshot that has not caught up, and forcing it would rebuild the same
    /// version on every pass.
    #[tokio::test]
    async fn auto_update_forces_only_packages_with_vcs_sources() {
        let server = MockServer::start().await;
        let (client, _official) =
            client_with_empty_official_repos(format!("{}/rpc/v5", server.uri())).await;
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        let (tx, _rx) = tokio::sync::broadcast::channel::<Action>(100);

        let aur_root = tempdir().unwrap();
        let mut ids = std::collections::HashMap::new();
        for name in ["plain", "tracks-git"] {
            create_aur_git_repo(aur_root.path(), name, "1.0.0", &[]);
            // Built at the version its source still reports, and flagged anyway.
            let pkg = packages::ActiveModel {
                name: Set(name.to_string()),
                status: Set(BuildStates::SUCCESSFUL_BUILD),
                out_of_date: Set(1),
                upstream_version: Set(Some("1.0.0-1".to_string())),
                latest_build: Set(None),
                build_flags: Set(String::new()),
                platforms: Set("x86_64".to_string()),
                source_type: Set(packages::SourceType::Aur),
                source_data: Set(SourceData::Aur { name: name.into() }),
                directly_requested: Set(true),
                split_packages: Set(None),
                ..Default::default()
            }
            .save(&db)
            .await
            .unwrap()
            .try_into_model()
            .unwrap();
            builds::ActiveModel {
                pkg_id: Set(pkg.id),
                status: Set(Some(BuildStates::SUCCESSFUL_BUILD)),
                start_time: Set(Some(1)),
                end_time: Set(Some(2)),
                platform: Set(Platform::X86_64),
                version: Set("1.0.0-1".to_string()),
                ..Default::default()
            }
            .save(&db)
            .await
            .unwrap();
            ids.insert(name, pkg.id);
        }
        aurcache_db::package_vcs_sources::ActiveModel {
            package_id: Set(ids["tracks-git"]),
            source_url: Set("git+https://example.com/repo.git".to_string()),
            last_commit: Set("abc".to_string()),
            updated_at: Set(1),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        let (store, _checkout_dir) = test_store(aur_root.path());
        let services = Services::new(
            db.clone(),
            tx,
            Arc::new(store),
            Arc::new(client),
            test_repo(),
            ActivityLog::discarding(),
        );
        super::package_update_all_outdated(&services).await.unwrap();

        let queued_for = |name: &'static str| {
            let db = db.clone();
            let id = ids[name];
            async move {
                builds::Entity::find()
                    .filter(builds::Column::PkgId.eq(id))
                    .count(&db)
                    .await
                    .unwrap()
                    - 1
            }
        };
        assert_eq!(
            queued_for("tracks-git").await,
            1,
            "the VCS package rebuilds"
        );
        assert_eq!(
            queued_for("plain").await,
            0,
            "an unchanged version with no VCS source is not rebuilt"
        );
        let scheduled = builds::Entity::find()
            .filter(builds::Column::PkgId.eq(ids["tracks-git"]))
            .filter(builds::Column::Status.eq(BuildStates::ENQUEUED_BUILD))
            .one(&db)
            .await
            .unwrap()
            .expect("the queued rebuild");
        assert_eq!(
            scheduled.trigger,
            BuildTriggers::AUTO_UPDATE,
            "the scheduler's builds say they are the scheduler's"
        );
    }

    #[tokio::test]
    async fn update_removes_orphaned_dependency_package() {
        let server = MockServer::start().await;
        let (client, _official) =
            client_with_empty_official_repos(format!("{}/rpc/v5", server.uri())).await;
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        let (tx, _) = tokio::sync::broadcast::channel::<Action>(100);

        // A v2.0.0 no longer depends on B
        let aur_root = tempdir().unwrap();
        create_aur_git_repo(aur_root.path(), "parent", "2.0.0", &[]);

        // Insert parent (directly requested, with a successful build)
        let parent = packages::ActiveModel {
            name: Set("parent".to_string()),
            status: Set(BuildStates::SUCCESSFUL_BUILD),
            out_of_date: Set(0),
            upstream_version: Set(Some("1.0.0".to_string())),
            latest_build: Set(None),
            build_flags: Set("--noconfirm;--noprogressbar".to_string()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(packages::SourceType::Aur),
            source_data: Set(SourceData::Aur {
                name: "parent".into(),
            }),
            directly_requested: Set(true),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap()
        .try_into_model()
        .unwrap();

        // Insert child (not directly requested)
        let child = packages::ActiveModel {
            name: Set("child".to_string()),
            status: Set(BuildStates::SUCCESSFUL_BUILD),
            out_of_date: Set(0),
            upstream_version: Set(Some("1.0.0".to_string())),
            latest_build: Set(None),
            build_flags: Set("--noconfirm;--noprogressbar".to_string()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(packages::SourceType::Aur),
            source_data: Set(SourceData::Aur {
                name: "child".into(),
            }),
            directly_requested: Set(false),
            split_packages: Set(None),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap()
        .try_into_model()
        .unwrap();

        // Dependency link: parent -> child
        dependencies::ActiveModel {
            dependent_id: Set(parent.id),
            dependee_id: Set(child.id),
            version_constraint: Set(">=1.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        // Successful build record for child
        builds::ActiveModel {
            pkg_id: Set(child.id),
            status: Set(Some(BuildStates::SUCCESSFUL_BUILD)),
            start_time: Set(Some(1)),
            end_time: Set(Some(2)),
            platform: Set(Platform::X86_64),
            version: Set("1.0.0".to_string()),
            ..Default::default()
        }
        .save(&db)
        .await
        .unwrap();

        let (store, _checkout_dir) = test_store(aur_root.path());
        package_update(
            &Services::new(
                db.clone(),
                tx.clone(),
                Arc::new(store),
                Arc::new(client),
                test_repo(),
                ActivityLog::discarding(),
            ),
            parent.clone(),
            false,
            BuildTrigger::User,
        )
        .await
        .unwrap();

        // Dependency link should be removed
        let dep_count = dependencies::Entity::find()
            .filter(dependencies::Column::DependentId.eq(parent.id))
            .filter(dependencies::Column::DependeeId.eq(child.id))
            .count(&db)
            .await
            .unwrap();
        assert_eq!(
            dep_count, 0,
            "dependency link from parent to child should be removed"
        );

        // Child package should be deleted (no dependents left, not directly requested)
        let child_in_db = packages::Entity::find_by_id(child.id)
            .one(&db)
            .await
            .unwrap();
        assert!(
            child_in_db.is_none(),
            "orphaned child package should be removed from the DB"
        );

        // Build records for the deleted package should also be gone
        let build_count = builds::Entity::find()
            .filter(builds::Column::PkgId.eq(child.id))
            .count(&db)
            .await
            .unwrap();
        assert_eq!(
            build_count, 0,
            "build records for deleted package should be removed"
        );
    }
}
