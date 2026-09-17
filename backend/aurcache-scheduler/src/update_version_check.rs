use anyhow::anyhow;
use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_activitylog::events::{Event, RefreshTarget, SourceinfoPurpose};
use aurcache_activitylog::failure_activity::VersionCheckFailedActivity;
use aurcache_common::settings::{ApplicationSettings, Setting, SettingsEntry};
use aurcache_db::activities::ActivityType;
use aurcache_db::helpers::builds::latest_successful_version_any_platform;
use aurcache_db::packages;
use aurcache_db::packages::{SourceData, SourceType};
use aurcache_db::prelude::Packages;
use aurcache_utils::package::metadata::apply_source_metadata;
use aurcache_utils::package::update::package_update_all_outdated;
use aurcache_utils::pkg::vercmp;
use aurcache_utils::services::Services;
use aurcache_utils::settings::general::SettingsTraits;
use aurcache_utils::vcs_check::{RoundCache, sync_vcs_sources};
use sea_orm::{ActiveModelTrait, ActiveValue::Set, DatabaseConnection, EntityTrait};
use std::collections::HashMap;
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::{error, info};

#[must_use]
pub fn start_update_version_checking(services: Services) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            info!("performing aur version checks");
            if let Err(e) = check_versions(&services).await {
                error!("Failed to perform aur version check: {e}");
                // Nothing was found to be out of date this pass, which looks
                // exactly like nothing *being* out of date unless it says so.
                services.activity.record(
                    VersionCheckFailedActivity {
                        reason: format!("{e:#}"),
                    },
                    ActivityType::VersionCheckFailed,
                    None,
                );
            }

            let check_interval: SettingsEntry<u64> =
                ApplicationSettings::get(Setting::VersionCheckInterval, None, &services.db).await;
            tokio::time::sleep(Duration::from_secs(check_interval.value.max(1))).await;
        }
    })
}

async fn check_versions(services: &Services) -> anyhow::Result<()> {
    let Services {
        db,
        tx: _,
        store,
        client,
        activity,
        ..
    } = services;
    let packages = Packages::find().all(db).await?;
    let aur_query_names: Vec<String> = packages
        .iter()
        .filter(|x| x.source_type == SourceType::Aur)
        .map(|x| {
            // AUR RPC /info matches by pkgname, not pkgbase.  For packages whose
            // pkgbase differs from any child pkgname (e.g. czkawka → czkawka-cli),
            // query by the first split child so the RPC returns a result whose
            // package_base field can be matched below.
            x.split_packages
                .as_deref()
                .and_then(|sp| serde_json::from_str::<Vec<String>>(sp).ok())
                .filter(|names| names.len() > 1)
                .and_then(|names| names.first().cloned())
                .unwrap_or_else(|| x.name.clone())
        })
        .collect();

    let aur_name_refs: Vec<&str> = aur_query_names.iter().map(String::as_str).collect();

    let results = if aur_name_refs.is_empty() {
        Vec::new()
    } else {
        client
            .multi_info_of(&aur_name_refs)
            .await
            .map_err(|e| anyhow!("couldn't download version update: {e}"))?
    };
    // Indexed once: the per-package loop below looks every package up by its
    // pkgbase, which a linear scan would make quadratic in the package count.
    let by_base: HashMap<&str, _> = results
        .iter()
        .map(|result| (result.package_base.as_str(), result))
        .collect();

    // One pass, one answer per remote: several packages can name the same
    // upstream, and what its ref points at does not change between two of them.
    let mut round = RoundCache::new();

    for package in packages {
        let package_id = package.id;
        let mut package_model: packages::ActiveModel = package.clone().into();

        // Not scoped to a platform: this is compared against the upstream
        // version to decide whether the package is out of date, which is a
        // question about the package rather than about one architecture.
        let latest_version = latest_successful_version_any_platform(db, package_id)
            .await?
            .filter(|version| !version.is_empty());

        let source_data = package.source_data;
        match source_data {
            SourceData::Aur { .. } => {
                match by_base.get(package.name.as_str()) {
                    None => {
                        // Removed from the AUR. Recorded so the package page
                        // can say so: its metadata still comes from the
                        // checkout, which continues to exist, so absent
                        // metadata no longer implies absence from the AUR.
                        activity.emit(Event::AurMissing {
                            pkg: package.name.as_str().into(),
                        });
                        package_model.aur_missing = Set(Some(true));
                    }
                    Some(result) => {
                        package_model.upstream_version = Set(Some(result.version.clone()));
                        // The AUR's own out-of-date marker is the one thing
                        // here it alone knows; the rest of the page's metadata
                        // is read from the checkout below, which also covers
                        // git-sourced packages that have no AUR entry.
                        package_model.aur_flagged_outdated =
                            Set(Some(result.out_of_date.unwrap_or(0) != 0));
                        package_model.aur_missing = Set(Some(false));
                        // Only mark out of date when upstream is strictly newer than the
                        // locally built version.  This prevents VCS packages (-git etc.)
                        // from looping: the AUR-reported version is the one from when the
                        // PKGBUILD was last touched, which may be *older* than what was
                        // actually built from the live VCS source.
                        let mut is_outdated = upstream_is_newer(
                            &result.version,
                            latest_version.as_deref(),
                            &package.name,
                            activity,
                        );

                        // `pkgver` alone doesn't catch VCS packages (-git etc.)
                        // whose upstream repo moved without the AUR PKGBUILD's
                        // version being bumped. Resolve any git+ VCS sources
                        // and flag out-of-date if any of them changed.
                        match store
                            .sourceinfo_and_metadata(&source_data, package.patch.as_deref())
                            .await
                        {
                            Ok((sourceinfo, metadata)) => {
                                apply_source_metadata(&mut package_model, &metadata);
                                match sync_vcs_sources(db, package_id, &sourceinfo, &mut round)
                                    .await
                                {
                                    Ok(vcs_changed) => is_outdated = is_outdated || vcs_changed,
                                    Err(e) => activity.emit(Event::VcsSyncFailed {
                                        pkg: package.name.as_str().into(),
                                        error: format!("{e:#}"),
                                    }),
                                }
                            }
                            Err(e) => activity.emit(Event::SourceinfoFailed {
                                pkg: package.name.as_str().into(),
                                purpose: SourceinfoPurpose::Vcs,
                                error: format!("{e:#}"),
                            }),
                        }

                        package_model.out_of_date = Set(i32::from(is_outdated));

                        // The AUR RPC `/info` response is a cheap way to know
                        // whether the package has actually changed upstream
                        // (via `version`/`last_modified`); only refresh the
                        // (git-backed) snapshot cache -- which requires a
                        // `git fetch` -- when it looks like something changed,
                        // instead of unconditionally re-fetching every package
                        // on every check.
                        if is_outdated && let Err(e) = store.refresh(&source_data).await {
                            activity.emit(Event::SourceRefreshFailed {
                                pkg: package.name.as_str().into(),
                                target: RefreshTarget::Snapshot,
                                error: format!("{e:#}"),
                            });
                        }
                    }
                }
            }
            SourceData::Git { .. } => {
                // No cheap upstream-metadata API for arbitrary git remotes,
                // so always refresh: this is an incremental `git fetch`
                // against the persistent checkout, not a full re-clone.
                if let Err(e) = store.refresh(&source_data).await {
                    activity.emit(Event::SourceRefreshFailed {
                        pkg: package.name.as_str().into(),
                        target: RefreshTarget::Git,
                        error: format!("{e:#}"),
                    });
                    save_package(db, activity, package_model, &package.name).await;
                    continue;
                }
                // A failure here (e.g. a patch that no longer applies
                // cleanly against a new upstream commit) must not abort
                // version-checking for the remaining packages - it only
                // means this package's own out-of-date/version tracking
                // can't be updated this round; the actual build for this
                // package will separately fail later with the same error,
                // which is the desired outcome for an unapplicable patch.
                let (sourceinfo, metadata) = match store
                    .sourceinfo_and_metadata(&source_data, package.patch.as_deref())
                    .await
                {
                    Ok(resolved) => resolved,
                    Err(e) => {
                        activity.emit(Event::SourceinfoFailed {
                            pkg: package.name.as_str().into(),
                            purpose: SourceinfoPurpose::Version,
                            error: format!("{e:#}"),
                        });
                        save_package(db, activity, package_model, &package.name).await;
                        continue;
                    }
                };
                // This still only tracks the version in PKGBUILD/.SRCINFO; a ref
                // moving without a version bump will not mark the package outdated
                // by itself - the VCS-source check below covers that case.
                let version = sourceinfo.base.version.to_string();

                package_model.upstream_version = Set(Some(version.clone()));
                // A git-sourced package has no AUR entry, so this is the only
                // place its description, licenses and maintainer come from.
                apply_source_metadata(&mut package_model, &metadata);
                // Same logic as for AUR packages: only mark out of date when the
                // upstream PKGBUILD version is strictly newer than what was built.
                let mut is_outdated =
                    upstream_is_newer(&version, latest_version.as_deref(), &package.name, activity);

                match sync_vcs_sources(db, package_id, &sourceinfo, &mut round).await {
                    Ok(vcs_changed) => is_outdated = is_outdated || vcs_changed,
                    Err(e) => activity.emit(Event::VcsSyncFailed {
                        pkg: package.name.as_str().into(),
                        error: format!("{e:#}"),
                    }),
                }

                package_model.out_of_date = Set(i32::from(is_outdated));
            }
            SourceData::Upload { .. } => {
                // noop since update is only triggered by new upload
            }
        }

        save_package(db, activity, package_model, &package.name).await;
    }

    // Detection is the only thing that knows a package went out of date —
    // including a VCS package whose upstream moved without a pkgver bump — so
    // with this on, the rebuild is queued here rather than waiting for the
    // separate `auto_update_interval` window, which could be a day away.
    //
    // Reuses the auto-update job's own selection rather than restating it:
    // out-of-date packages whose last build succeeded. A package whose build
    // is failing stays flagged for a human instead of being retried in a loop.
    let build_now: SettingsEntry<bool> =
        ApplicationSettings::get(Setting::BuildOnNewVersion, None, db).await;
    if build_now.value
        && let Err(e) = package_update_all_outdated(services).await
    {
        activity.emit(Event::UpdateQueueFailed {
            error: format!("{e:#}"),
        });
    }

    Ok(())
}

/// Whether `upstream` is a newer version than what was last built.
///
/// A package that has never been built counts as outdated. When the two
/// versions cannot be compared (either side is not valid alpm syntax) we fall
/// back to "did the string change", which errs towards scheduling a build:
/// reporting "not newer" would silently freeze the package forever, whereas a
/// spurious rebuild is merely wasted work.
fn upstream_is_newer(
    upstream: &str,
    built: Option<&str>,
    package: &str,
    activity: &ActivityLog,
) -> bool {
    let Some(built) = built else {
        return true;
    };
    match vercmp(upstream, built) {
        Some(ordering) => ordering == std::cmp::Ordering::Greater,
        None => {
            activity.emit(Event::VersionCompareFallback {
                pkg: package.into(),
                upstream_version: upstream.to_string(),
                built_version: built.to_string(),
            });
            upstream != built
        }
    }
}

/// Persist the version-check outcome for one package. A write failure only
/// costs this package one round of tracking, so it is logged, not propagated.
async fn save_package(
    db: &DatabaseConnection,
    activity: &ActivityLog,
    model: packages::ActiveModel,
    name: &str,
) {
    if let Err(e) = model.update(db).await {
        activity.emit(Event::VersionCheckStoreFailed {
            pkg: name.into(),
            error: format!("{e:#}"),
        });
    }
}
