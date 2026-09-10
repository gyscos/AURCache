use crate::utils::remove_archive_file::forget_archive_file;
use anyhow::anyhow;
use aurcache_db::prelude::{Builds, Dependencies, Files, PackageVcsSources, Packages, Settings};
use aurcache_db::{builds, dependencies, files, package_vcs_sources, settings};
use sea_orm::{
    ColumnTrait, Condition, DatabaseConnection, EntityTrait, ModelTrait, QueryFilter,
    TransactionTrait,
};

pub async fn package_delete(db: &DatabaseConnection, pkg_id: i32) -> anyhow::Result<()> {
    let txn = db.begin().await?;

    let pkg = Packages::find_by_id(pkg_id)
        .one(&txn)
        .await?
        .ok_or_else(|| anyhow!("id not found"))?;

    // Captured before the delete consumes the model: the logs are stored under
    // this name and have to be removed after the transaction commits.
    let pkgbase = pkg.name.clone();

    // The children first, and the package row last.
    //
    // Order used to be the other way round, which stopped being safe the moment
    // `files.package_id` became a real foreign key: `ON DELETE CASCADE` takes
    // the `files` rows with the package, so a read afterwards returns nothing
    // and every artifact stays on disk and in `repo.db` with no row left to
    // find it by. Deleting the children explicitly, before the parent, means
    // this function sees them whether or not the database would have removed
    // them on its own.

    // Read before the delete and removed from disk after the commit: a
    // rolled-back transaction can put a row back, and nothing can put back a
    // deleted file.
    let package_files: Vec<files::Model> = Files::find()
        .filter(files::Column::PackageId.eq(pkg_id))
        .all(&txn)
        .await?;

    Files::delete_many()
        .filter(files::Column::PackageId.eq(pkg_id))
        .exec(&txn)
        .await?;

    // remove corresponding builds
    Builds::delete_many()
        .filter(builds::Column::PkgId.eq(pkg_id))
        .exec(&txn)
        .await?;

    // delete the dependency links this package sits on either end of.
    //
    // Both directions: an orphan has nothing depending on it *now*, but this is
    // also how a directly-requested package is deleted, and that one can still
    // be needed by something. Leaving either side behind strands a row
    // referencing an id that no longer exists.
    //
    // Declared `ON DELETE CASCADE`, and done explicitly anyway, for the same
    // reason as the VCS sources below: SQLite only enforces a foreign key when
    // `foreign_keys` is on for the connection issuing the DELETE.
    Dependencies::delete_many()
        .filter(
            Condition::any()
                .add(dependencies::Column::DependentId.eq(pkg_id))
                .add(dependencies::Column::DependeeId.eq(pkg_id)),
        )
        .exec(&txn)
        .await?;

    // delete corresponding settings entries
    Settings::delete_many()
        .filter(settings::Column::PkgId.eq(pkg_id))
        .exec(&txn)
        .await?;

    // delete tracked VCS source commits (not relied upon `ON DELETE CASCADE`
    // alone, since SQLite only enforces it when foreign_keys is on for the
    // connection actually issuing the DELETE)
    PackageVcsSources::delete_many()
        .filter(package_vcs_sources::Column::PackageId.eq(pkg_id))
        .exec(&txn)
        .await?;

    // remove package db entry
    pkg.delete(&txn).await?;

    txn.commit().await?;

    for file in &package_files {
        forget_archive_file(file);
    }

    // Build logs live on disk rather than in a column, so nothing removes them
    // on our behalf. One directory, because they are stored under the package's
    // name. After the commit deliberately: a rollback must not leave rows
    // pointing at logs that are gone, while a stray directory after a commit is
    // only wasted bytes.
    crate::build_logger::remove_package_logs(&pkgbase).await;

    Ok(())
}
