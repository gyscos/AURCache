//! Reading a dump back in.
//!
//! The order here is the whole design, and it is not the obvious one.
//!
//! Every package row is inserted *before* any dependency is resolved. That
//! removes the question of what order to import in -- there is no order, so a
//! dependency cycle is not a special case -- but on its own it is not enough. A
//! local package satisfies a dependency by its pkgbase, by one of its split
//! package names, or by something it `provides`, and a dump carries only the
//! first: the other two are derived from the PKGBUILD and are deliberately not
//! exported.
//!
//! So a git-sourced package that provides `libfoo` would not be recognised as
//! satisfying a dependency on `libfoo`, and resolution would fall through to
//! the AUR and adopt an AUR package in its place -- silently replacing the
//! source the operator chose. Which is why every package's source is resolved,
//! filling in `provides` and its split package names, *before* the dependency
//! graph is rebuilt. By then every package the dump carried is a complete
//! candidate and nothing can fall through.
//!
//! Restore has to read those sources anyway: `dependencies` is not exported
//! either -- an imported graph could disagree with today's AUR -- so the depends
//! lists have to be rediscovered from the sources regardless.

use std::collections::BTreeMap;
use std::io::Read;

use aurcache_common::api::dump::{
    DUMP_SCHEMA_VERSION, DumpManifest, DumpPackage, DumpPackages, DumpSettings, DumpWorker,
    ExistingPackagePolicy, MANIFEST_FILE, PACKAGES_FILE, PATCH_DIR, RestoreEntry, RestoreOptions,
    RestoreOutcome, SETTINGS_FILE, WORKERS_FILE,
};
use aurcache_common::builder::BuildStates;
use aurcache_db::action::Action;
use aurcache_db::packages::{SourceData, SourceType};
use aurcache_db::prelude::Packages;
use aurcache_db::{packages, settings};
use flate2::read::GzDecoder;
use sea_orm::ActiveValue::Set;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, TransactionTrait,
};
use tokio::sync::broadcast::Sender;
use tokio::sync::mpsc::UnboundedSender;
use tracing::warn;

use crate::package::add::{provides_json, split_packages_json};
use crate::snapshot::SnapshotStore;

/// A dump that has been read and checked, ready to apply.
#[derive(Debug)]
pub struct LoadedDump {
    pub manifest: DumpManifest,
    pub packages: DumpPackages,
    pub settings: DumpSettings,
    pub workers: Vec<DumpWorker>,
    pub patches: BTreeMap<String, String>,
}

/// Read and validate a `.tar.gz` dump.
///
/// Everything is checked before anything is applied. An import that fails on
/// package 90 of 200 leaves a database that is neither what it was nor what the
/// dump describes, which is worse than either.
pub fn load_dump(bytes: &[u8]) -> anyhow::Result<LoadedDump> {
    let mut files = BTreeMap::new();
    let mut archive = tar::Archive::new(GzDecoder::new(bytes));
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_string_lossy().into_owned();
        let mut content = String::new();
        // A dump holds JSON and patches; anything that is not text is not
        // something this format has ever contained.
        entry
            .read_to_string(&mut content)
            .map_err(|e| anyhow::anyhow!("{path} is not readable as text: {e}"))?;
        files.insert(path, content);
    }

    let manifest: DumpManifest = parse(&files, MANIFEST_FILE)?;
    // Checked before anything else is even parsed: a newer dump may describe
    // things this version has no idea how to apply, and guessing at them is how
    // a restore silently drops what it did not understand.
    if manifest.schema_version > DUMP_SCHEMA_VERSION {
        anyhow::bail!(
            "this dump is version {} but this AURCache understands up to {DUMP_SCHEMA_VERSION}; \
             upgrade AURCache to restore it",
            manifest.schema_version
        );
    }

    let packages: DumpPackages = parse(&files, PACKAGES_FILE)?;
    let settings: DumpSettings = parse(&files, SETTINGS_FILE)?;
    let workers: Vec<DumpWorker> = parse(&files, WORKERS_FILE)?;

    let mut patches = BTreeMap::new();
    for (path, content) in &files {
        if let Some(pkgbase) = path
            .strip_prefix(&format!("{PATCH_DIR}/"))
            .and_then(|name| name.strip_suffix(".patch"))
        {
            // A patch for a package the dump does not carry has nothing to
            // attach to. Refused rather than ignored: it means the archive was
            // assembled wrongly, and silently dropping it loses an edit someone
            // made deliberately.
            if !packages.contains_key(pkgbase) {
                anyhow::bail!("{path} has no matching package in {PACKAGES_FILE}");
            }
            patches.insert(pkgbase.to_string(), content.clone());
        }
    }

    // Settings naming a package the dump does not carry would have nowhere to
    // go, and would quietly not be applied.
    for pkgbase in settings.packages.keys() {
        if !packages.contains_key(pkgbase) {
            anyhow::bail!(
                "{SETTINGS_FILE} configures '{pkgbase}', which is not in {PACKAGES_FILE}"
            );
        }
    }

    for (pkgbase, package) in &packages {
        if package.platforms.is_empty() {
            anyhow::bail!("'{pkgbase}' lists no platforms, so nothing could build it");
        }
    }

    Ok(LoadedDump {
        manifest,
        packages,
        settings,
        workers,
        patches,
    })
}

fn parse<T: serde::de::DeserializeOwned>(
    files: &BTreeMap<String, String>,
    name: &str,
) -> anyhow::Result<T> {
    let content = files
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("{name} is missing; this is not an AURCache dump"))?;
    serde_json::from_str(content).map_err(|e| anyhow::anyhow!("{name} is not valid: {e}"))
}

/// Join a dump's list back into the database's semicolon-delimited column.
fn join_list(values: &[String]) -> String {
    values.join(";")
}

/// What a dump would do to one package, without doing it.
///
/// A dry run answers the only question worth asking before an import: which of
/// my packages does this touch. It reads, so it can be wrong about a race, but
/// nothing it reports has happened.
pub async fn preview(
    db: &DatabaseConnection,
    dump: &LoadedDump,
    options: &RestoreOptions,
) -> anyhow::Result<Vec<RestoreEntry>> {
    let mut entries = Vec::new();
    for pkgbase in dump.packages.keys() {
        let exists = package_row(db, pkgbase).await?.is_some();
        entries.push(RestoreEntry {
            pkgbase: pkgbase.clone(),
            outcome: match (exists, options.on_existing) {
                (false, _) => RestoreOutcome::Imported,
                (true, ExistingPackagePolicy::Skip) => RestoreOutcome::Skipped,
                (true, ExistingPackagePolicy::Overwrite) => RestoreOutcome::Overwritten,
            },
        });
    }
    Ok(entries)
}

async fn package_row(
    db: &DatabaseConnection,
    pkgbase: &str,
) -> anyhow::Result<Option<packages::Model>> {
    Ok(Packages::find()
        .filter(packages::Column::Name.eq(pkgbase))
        .one(db)
        .await?)
}

/// Apply a dump.
///
/// Three passes, and the order between the last two is the point -- see this
/// module's header. Rows first, with no network; then every package's source,
/// which is what fills in the names a dependency can be resolved by; only then
/// the dependency graph.
///
/// # What happens when part of it fails
///
/// The two halves fail differently, on purpose.
///
/// **Pass 1 is all or nothing.** It is one transaction over local rows, so
/// either every package the dump describes is written or none is. A dump that
/// half-applied would leave an instance that is neither what it was nor what
/// the dump says, which is worse than either.
///
/// **Passes 2 and 3 are per package, and a failure does not roll anything
/// back.** They read sources over the network and parse PKGBUILDs, so they fail
/// for reasons that have nothing to do with the rest of the dump: one
/// unparseable PKGBUILD, one unreachable remote, one dependency naming
/// something that does not exist. Undoing 199 good packages because the 200th
/// has a typo in it would be the wrong trade, and re-running the restore is
/// then the only way back.
///
/// The row is deliberately kept. It carries the platforms, flags and patch the
/// operator asked for, and the commonest reason to get here is a transient read
/// -- throwing that configuration away on a network blip would lose more than
/// it saves. What such a package does *not* have is its `provides` and split
/// package names, so a dependency naming one of those will not find it; the
/// entry says so, because it is the difference between a restore that worked
/// and one that looks like it did.
pub async fn apply(
    db: &DatabaseConnection,
    store: &SnapshotStore,
    tx: &Sender<Action>,
    dump: LoadedDump,
    options: RestoreOptions,
    progress: UnboundedSender<RestoreEntry>,
) {
    // PASS 1: rows. One transaction, because a half-applied dump is neither
    // what the instance was nor what the dump describes.
    let applied = match write_rows(db, &dump, &options).await {
        Ok(applied) => applied,
        Err(e) => {
            // The transaction rolled back, so nothing was written; say so
            // against the dump as a whole rather than blaming one package.
            let _ = progress.send(RestoreEntry {
                pkgbase: String::new(),
                outcome: RestoreOutcome::Failed {
                    error: format!("nothing was imported: {e}"),
                },
            });
            return;
        }
    };

    for entry in &applied.entries {
        let _ = progress.send(entry.clone());
    }

    // PASS 2: sources. Every imported package's `provides` and split package
    // names, so that pass 3 can recognise them. Until this is done for *all* of
    // them, resolving any one would fall through to the AUR and adopt a package
    // in place of one the dump carried.
    let mut unresolved = Vec::new();
    for pkgbase in &applied.touched {
        if let Err(e) = fill_source_facts(db, store, pkgbase).await {
            warn!("restore: could not read the source of {pkgbase}: {e}");
            let _ = progress.send(RestoreEntry {
                pkgbase: pkgbase.clone(),
                outcome: RestoreOutcome::Failed {
                    error: format!(
                        "imported, but its source could not be read, so other packages \
                         will not find it by its split package names or what it provides: {e}. \
                         Fix the source and restore again with --on-existing overwrite."
                    ),
                },
            });
            unresolved.push(pkgbase.clone());
        }
    }

    // PASS 3: the graph, and the builds it makes possible.
    let client = aurcache_deps::AurClient::new();
    for pkgbase in &applied.touched {
        // Its source could not be read, so its dependency list cannot be
        // either. Trying anyway would fail identically and report it twice.
        if unresolved.contains(pkgbase) {
            continue;
        }
        let Ok(Some(row)) = package_row(db, pkgbase).await else {
            continue;
        };
        if let Err(e) =
            crate::package::update::package_resync_dependencies(&client, store, db, tx, &row).await
        {
            warn!("restore: could not resolve dependencies for {pkgbase}: {e}");
            let _ = progress.send(RestoreEntry {
                pkgbase: pkgbase.clone(),
                outcome: RestoreOutcome::Failed {
                    error: format!(
                        "imported, but its dependencies could not be resolved, so it will not \
                         build until they are: {e}"
                    ),
                },
            });
        }
    }
}

/// What pass 1 did.
#[derive(Debug)]
pub struct Applied {
    pub entries: Vec<RestoreEntry>,
    /// Packages whose sources and graph the later passes must visit. A skipped
    /// package is not among them: it was already here, with its own resolution
    /// already done.
    pub touched: Vec<String>,
}

/// Insert and update the rows, in one transaction.
///
/// Public because it is the whole of the import that touches no network: the
/// passes after it read sources and resolve dependencies, so this is the part
/// whose effect on the database can be stated exactly and tested directly.
pub async fn write_rows(
    db: &DatabaseConnection,
    dump: &LoadedDump,
    options: &RestoreOptions,
) -> anyhow::Result<Applied> {
    let txn = db.begin().await?;
    let mut entries = Vec::new();
    let mut touched = Vec::new();
    let mut ids = BTreeMap::new();

    // Clearing happens inside the same transaction that writes the
    // replacement. If it did not, an import that failed after wiping would
    // leave an empty instance -- the one outcome worse than either keeping the
    // old state or taking the new one.
    let orphaned = if options.clear {
        clear_existing(&txn).await?
    } else {
        Vec::new()
    };

    for (pkgbase, package) in &dump.packages {
        let existing = Packages::find()
            .filter(packages::Column::Name.eq(pkgbase))
            .one(&txn)
            .await?;
        let patch = dump.patches.get(pkgbase).cloned();

        let outcome = match (existing, options.on_existing) {
            (Some(row), ExistingPackagePolicy::Skip) => {
                ids.insert(pkgbase.clone(), row.id);
                RestoreOutcome::Skipped
            }
            (Some(row), ExistingPackagePolicy::Overwrite) => {
                let id = row.id;
                let mut active: packages::ActiveModel = row.into();
                apply_config(&mut active, package, patch);
                active.update(&txn).await?;
                ids.insert(pkgbase.clone(), id);
                touched.push(pkgbase.clone());
                RestoreOutcome::Overwritten
            }
            (None, _) => {
                let id = insert_row(&txn, pkgbase, package, patch).await?;
                ids.insert(pkgbase.clone(), id);
                touched.push(pkgbase.clone());
                RestoreOutcome::Imported
            }
        };
        entries.push(RestoreEntry {
            pkgbase: pkgbase.clone(),
            outcome,
        });
    }

    write_settings(&txn, dump, &ids).await?;
    write_workers(&txn, dump).await?;
    txn.commit().await?;

    // Only now: a rolled-back transaction can put a row back, and nothing can
    // put back a deleted file.
    for file in &orphaned {
        crate::utils::remove_archive_file::forget_archive_file(file);
    }

    Ok(Applied { entries, touched })
}

/// Remove everything a dump replaces, returning the built artifacts whose files
/// the caller must forget once the transaction commits.
///
/// What gets cleared is decided by what a dump *contains*, not by a list of
/// tables: packages, settings and workers travel in every dump, so all three
/// go. Anything a dump does not carry is left alone -- which is what stops a
/// public dump, which has no CA, from destroying one.
///
/// Builds, files and VCS-source rows go with their packages. They are not in
/// the dump because they are derived, but leaving them would orphan them
/// against packages that no longer exist.
async fn clear_existing<C: sea_orm::ConnectionTrait>(
    txn: &C,
) -> anyhow::Result<Vec<aurcache_db::files::Model>> {
    let orphaned = aurcache_db::prelude::Files::find().all(txn).await?;

    aurcache_db::prelude::Dependencies::delete_many()
        .exec(txn)
        .await?;
    aurcache_db::prelude::PackageVcsSources::delete_many()
        .exec(txn)
        .await?;
    aurcache_db::prelude::Files::delete_many().exec(txn).await?;
    aurcache_db::prelude::Builds::delete_many()
        .exec(txn)
        .await?;
    aurcache_db::prelude::Settings::delete_many()
        .exec(txn)
        .await?;
    aurcache_db::prelude::Workers::delete_many()
        .exec(txn)
        .await?;
    Packages::delete_many().exec(txn).await?;

    Ok(orphaned)
}

/// Restore the workers a dump trusts.
///
/// Identified by fingerprint, not by name: the fingerprint is what a worker
/// proves on contact, and two instances can easily have used different names
/// for the same machine.
///
/// No certificate is imported -- a public dump has none, and one signed by
/// another instance's CA would mean nothing here. A worker whose fingerprint is
/// already known re-enrolls and is approved without anyone being asked, which is
/// what makes a migration invisible to it.
async fn write_workers<C: sea_orm::ConnectionTrait>(
    txn: &C,
    dump: &LoadedDump,
) -> anyhow::Result<()> {
    for worker in &dump.workers {
        let existing = aurcache_db::prelude::Workers::find()
            .filter(aurcache_db::workers::Column::CertFingerprint.eq(&worker.cert_fingerprint))
            .one(txn)
            .await?;
        // Already trusted here. Its routing is this instance's decision, not
        // the dump's, so it is left as it is.
        if existing.is_some() {
            continue;
        }
        aurcache_db::workers::ActiveModel {
            name: Set(worker.name.clone()),
            status: Set(aurcache_common::api::worker::ApprovalStatus::Approved),
            cert_fingerprint: Set(worker.cert_fingerprint.clone()),
            native_arches: Set(join_list(&worker.native_arches)),
            emulated_arches: Set(join_list(&worker.emulated_arches)),
            package_affinity: Set(join_list(&worker.package_affinity)),
            priority: Set(worker.priority),
            concurrency: Set(worker.concurrency),
            ..Default::default()
        }
        .insert(txn)
        .await?;
    }
    Ok(())
}

/// The columns a dump owns. Everything else on the row is derived and is left
/// to the passes that follow.
fn apply_config(active: &mut packages::ActiveModel, package: &DumpPackage, patch: Option<String>) {
    active.platforms = Set(join_list(&package.platforms));
    active.build_flags = Set(join_list(&package.build_flags));
    active.directly_requested = Set(package.directly_requested);
    active.source_type = Set(source_type_of(&package.source_data));
    active.source_data = Set(package.source_data.clone());
    active.patch = Set(patch);
}

fn source_type_of(source_data: &SourceData) -> SourceType {
    match source_data {
        SourceData::Aur { .. } => SourceType::Aur,
        SourceData::Git { .. } => SourceType::Git,
        SourceData::Upload { .. } => SourceType::Upload,
    }
}

async fn insert_row<C: sea_orm::ConnectionTrait>(
    db: &C,
    pkgbase: &str,
    package: &DumpPackage,
    patch: Option<String>,
) -> anyhow::Result<i32> {
    Ok(packages::ActiveModel {
        name: Set(pkgbase.to_string()),
        // Enqueued, not some neutral state: `resolve_local_dependency_resolutions`
        // only considers packages that are active, successful or enqueued, so a
        // row in any other state is invisible to resolution -- and a dependency
        // on it would fall through to the AUR, which is exactly what importing
        // the package was meant to prevent.
        status: Set(BuildStates::ENQUEUED_BUILD),
        out_of_date: Set(0),
        platforms: Set(join_list(&package.platforms)),
        build_flags: Set(join_list(&package.build_flags)),
        source_type: Set(source_type_of(&package.source_data)),
        source_data: Set(package.source_data.clone()),
        directly_requested: Set(package.directly_requested),
        patch: Set(patch),
        ..Default::default()
    }
    .insert(db)
    .await?
    .id)
}

/// Settings, keyed back from pkgbase onto row ids.
async fn write_settings<C: sea_orm::ConnectionTrait>(
    db: &C,
    dump: &LoadedDump,
    ids: &BTreeMap<String, i32>,
) -> anyhow::Result<()> {
    for (key, value) in &dump.settings.global {
        upsert_setting(db, key, value, crate::settings::general::GLOBAL_PKG_ID).await?;
    }
    for (pkgbase, values) in &dump.settings.packages {
        // Validation guarantees the package is in the dump; this only skips one
        // that was left alone by `Skip`, whose own settings are already right.
        let Some(id) = ids.get(pkgbase) else { continue };
        for (key, value) in values {
            upsert_setting(db, key, value, *id).await?;
        }
    }
    Ok(())
}

async fn upsert_setting<C: sea_orm::ConnectionTrait>(
    db: &C,
    key: &str,
    value: &str,
    pkg_id: i32,
) -> anyhow::Result<()> {
    let existing = settings::Entity::find()
        .filter(settings::Column::Key.eq(key))
        .filter(settings::Column::PkgId.eq(pkg_id))
        .one(db)
        .await?;
    match existing {
        Some(row) => {
            let mut active: settings::ActiveModel = row.into();
            active.value = Set(Some(value.to_string()));
            active.update(db).await?;
        }
        None => {
            settings::ActiveModel {
                key: Set(key.to_string()),
                value: Set(Some(value.to_string())),
                pkg_id: Set(Some(pkg_id)),
                ..Default::default()
            }
            .insert(db)
            .await?;
        }
    }
    Ok(())
}

/// Read a package's source and record what a dependency can find it by.
///
/// This is the step that makes the import self-contained. Without it the row
/// knows only its pkgbase, and a dependency naming one of its split packages or
/// something it provides would not match it.
async fn fill_source_facts(
    db: &DatabaseConnection,
    store: &SnapshotStore,
    pkgbase: &str,
) -> anyhow::Result<()> {
    let Some(row) = package_row(db, pkgbase).await? else {
        return Ok(());
    };
    let sourceinfo = store
        .sourceinfo(&row.source_data, row.patch.as_deref())
        .await?;
    let deps = aurcache_deps::deps_from_srcinfo(
        &sourceinfo,
        &crate::pkg::architectures_for_platforms(&row.platforms),
    );

    let mut active: packages::ActiveModel = row.into();
    active.split_packages = Set(split_packages_json(pkgbase, &deps.pkgnames)?);
    active.provides = Set(provides_json(&deps.provides)?);
    active.upstream_version = Set(Some(sourceinfo.base.version.to_string()));
    active.update(db).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{clear_existing, load_dump};
    use aurcache_common::api::dump::DUMP_SCHEMA_VERSION;

    /// Build an archive from explicit file contents, so a test can write a
    /// malformed one as easily as a good one.
    fn archive(files: &[(&str, &str)]) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(
                &mut bytes,
                flate2::Compression::default(),
            ));
            for (path, content) in files {
                let mut header = tar::Header::new_gnu();
                header.set_path(path).unwrap();
                header.set_size(content.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                tar.append(&header, content.as_bytes()).unwrap();
            }
            tar.finish().unwrap();
        }
        bytes
    }

    fn manifest(version: u32) -> String {
        format!(
            r#"{{"schema_version":{version},"aurcache_version":"t","created_at":0,"includes_secrets":false}}"#
        )
    }

    const ONE_PACKAGE: &str = r#"{"hello":{"source_data":{"type":"aur","name":"hello"},
        "platforms":["x86_64"],"build_flags":[],"directly_requested":true}}"#;

    fn good() -> Vec<(&'static str, String)> {
        vec![
            ("manifest.json", manifest(DUMP_SCHEMA_VERSION)),
            ("packages.json", ONE_PACKAGE.to_string()),
            (
                "settings.json",
                r#"{"global":{},"packages":{}}"#.to_string(),
            ),
            ("workers.json", "[]".to_string()),
        ]
    }

    fn load(files: Vec<(&str, String)>) -> anyhow::Result<super::LoadedDump> {
        let owned: Vec<(&str, &str)> = files.iter().map(|(a, b)| (*a, b.as_str())).collect();
        load_dump(&archive(&owned))
    }

    #[test]
    fn a_well_formed_dump_loads() {
        let loaded = load(good()).unwrap();
        assert!(loaded.packages.contains_key("hello"));
    }

    /// The manifest's version is the load-bearing part of the format: restoring
    /// into a *different* AURCache is the point, so one that cannot understand
    /// the file must say so rather than apply the half it recognises.
    #[test]
    fn a_newer_dump_is_refused() {
        let mut files = good();
        files[0] = ("manifest.json", manifest(DUMP_SCHEMA_VERSION + 1));
        let error = load(files).unwrap_err().to_string();
        assert!(error.contains("upgrade AURCache"), "unhelpful: {error}");
    }

    #[test]
    fn something_that_is_not_a_dump_is_refused() {
        let error = load_dump(&archive(&[("hello.txt", "hi")]))
            .unwrap_err()
            .to_string();
        assert!(error.contains("not an AURCache dump"), "unhelpful: {error}");
    }

    /// A patch is attached by filename, so one naming a package that is not
    /// there was assembled wrongly. Dropping it silently would lose an edit
    /// somebody made on purpose.
    #[test]
    fn a_patch_without_its_package_is_refused() {
        let mut files = good();
        files.push(("patches/ghost.patch", "--- a\n".to_string()));
        let error = load(files).unwrap_err().to_string();
        assert!(error.contains("no matching package"), "unhelpful: {error}");
    }

    /// Same reasoning for a setting: it would have nowhere to go.
    #[test]
    fn a_setting_for_an_absent_package_is_refused() {
        let mut files = good();
        files[2] = (
            "settings.json",
            r#"{"global":{},"packages":{"ghost":{"k":"v"}}}"#.to_string(),
        );
        let error = load(files).unwrap_err().to_string();
        assert!(error.contains("not in packages.json"), "unhelpful: {error}");
    }

    /// A package with no platforms could never build, so importing it would
    /// produce a row that sits there doing nothing.
    #[test]
    fn a_package_with_no_platforms_is_refused() {
        let mut files = good();
        files[1] = (
            "packages.json",
            r#"{"hello":{"source_data":{"type":"aur","name":"hello"},
                "platforms":[],"build_flags":[],"directly_requested":true}}"#
                .to_string(),
        );
        let error = load(files).unwrap_err().to_string();
        assert!(error.contains("no platforms"), "unhelpful: {error}");
    }

    /// Clearing is undone by a rollback.
    ///
    /// This is what lets `write_rows` clear and write in one transaction, and
    /// it is the property the whole `--clear` design rests on: if the write
    /// fails, the instance must still be what it was. An emptied instance is
    /// the one outcome worse than either keeping the old state or taking the
    /// new one, and it is not recoverable by re-running.
    #[tokio::test]
    async fn a_rolled_back_clear_puts_everything_back() {
        use aurcache_db::migration::Migrator;
        use aurcache_db::packages::{SourceData, SourceType};
        use sea_orm::{ActiveModelTrait, Database, EntityTrait, Set, TransactionTrait};
        use sea_orm_migration::MigratorTrait;

        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        aurcache_db::packages::ActiveModel {
            name: Set("precious".to_string()),
            status: Set(0),
            out_of_date: Set(0),
            build_flags: Set(String::new()),
            platforms: Set("x86_64".to_string()),
            source_type: Set(SourceType::Aur),
            source_data: Set(SourceData::Aur {
                name: "precious".to_string(),
            }),
            directly_requested: Set(true),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();

        let txn = db.begin().await.unwrap();
        clear_existing(&txn).await.unwrap();
        assert!(
            aurcache_db::prelude::Packages::find()
                .all(&txn)
                .await
                .unwrap()
                .is_empty(),
            "the clear did not take effect inside its transaction"
        );
        txn.rollback().await.unwrap();

        let survivors = aurcache_db::prelude::Packages::find()
            .all(&db)
            .await
            .unwrap();
        assert_eq!(
            survivors.len(),
            1,
            "a rolled-back clear destroyed data anyway"
        );
        assert_eq!(survivors[0].name, "precious");
    }

    /// A patch reaches the package it names.
    #[test]
    fn a_patch_is_matched_to_its_package() {
        let mut files = good();
        files.push(("patches/hello.patch", "--- a\n+++ b\n".to_string()));
        let loaded = load(files).unwrap();
        assert_eq!(
            loaded.patches.get("hello").map(String::as_str),
            Some("--- a\n+++ b\n")
        );
    }
}
