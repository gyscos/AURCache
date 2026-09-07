use crate::utils::remove_archive_file::forget_archive_file;
use anyhow::anyhow;
use aurcache_db::prelude::{Builds, Files, PackageVcsSources, Packages, Settings};
use aurcache_db::{builds, files, package_vcs_sources, settings};
use sea_orm::{
    ColumnTrait, DatabaseConnection, EntityTrait, ModelTrait, QueryFilter,
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

    // remove package db entry
    pkg.delete(&txn).await?;

    // remove corresponding builds
    Builds::delete_many()
        .filter(builds::Column::PkgId.eq(pkg_id))
        .exec(&txn)
        .await?;

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
