//! Mirroring AUR metadata onto the package row.
//!
//! `/api/package/<pkgbase>` used to fetch this live on every request, which
//! made the route ~128ms where the rest of it is ~2ms and spent one of the
//! AUR's 4000 daily calls per page view. Persisting it turns that into a plain
//! column read.
//!
//! Written from two places, which between them cover a package's whole life:
//! [`refresh_aur_metadata`] when it is added, and the version-check scheduler
//! on its interval. Both use [`apply_aur_metadata`], so there is one definition
//! of how an AUR response maps onto the row.

use aurcache_db::packages;
use aurcache_db::prelude::Packages;
use sea_orm::ActiveValue::Set;
use sea_orm::{ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};

/// Copy the parts of an AUR response the package page shows onto a row.
///
/// Only the fields that are otherwise unavailable locally. `upstream_version`
/// is deliberately not set here: the caller decides whether a version change
/// also means the package is out of date, which is a different question.
pub fn apply_aur_metadata(model: &mut packages::ActiveModel, info: &aurcache_deps::Package) {
    model.aur_description = Set(info.description.clone());
    model.aur_maintainer = Set(info.maintainer.clone());
    model.aur_project_url = Set(info.url.clone());
    model.aur_licenses = Set(info.license.as_ref().map(|l| l.join(", ")));
    model.aur_first_submitted = Set(Some(info.first_submitted.into()));
    model.aur_last_modified = Set(Some(info.last_modified.into()));
    model.aur_flagged_outdated = Set(Some(info.out_of_date.unwrap_or(0) != 0));
}

/// Fetch and store AUR metadata for freshly added packages.
///
/// Called at add time so a package is never without it: the API has no live
/// fallback, and waiting for the next scheduled version check would leave a new
/// package with no description or maintainer for up to an hour.
///
/// One bulk request for the whole set, so adding a package with twenty
/// dependencies costs one AUR call rather than twenty.
///
/// Best-effort: a package that cannot be added because the AUR is briefly
/// unreachable would be a much worse outcome than one that renders without a
/// description until the next scheduled check.
pub async fn refresh_aur_metadata(
    client: &aurcache_deps::AurClient,
    db: &DatabaseConnection,
    pkgbases: &[String],
) {
    if pkgbases.is_empty() {
        return;
    }

    let names: Vec<&str> = pkgbases.iter().map(String::as_str).collect();
    let results = match client.multi_info_of(&names).await {
        Ok(results) => results,
        Err(e) => {
            tracing::warn!("could not fetch AUR metadata for new packages: {e}");
            return;
        }
    };

    for info in results {
        // The RPC answers by pkgname; `package_base` is what this codebase keys
        // a row on, and the two differ for split packages.
        let Ok(Some(row)) = Packages::find()
            .filter(packages::Column::Name.eq(info.package_base.clone()))
            .one(db)
            .await
        else {
            continue;
        };

        let mut model = packages::ActiveModel {
            id: Set(row.id),
            ..Default::default()
        };
        apply_aur_metadata(&mut model, &info);
        if let Err(e) = model.update(db).await {
            tracing::warn!(
                "could not store AUR metadata for {}: {e}",
                info.package_base
            );
        }
    }
}
