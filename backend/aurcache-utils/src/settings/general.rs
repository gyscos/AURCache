use crate::settings::parser::{ByteSize, ParseSetting};
use aurcache_common::settings::{
    ApplicationSettings, Setting, SettingSource, SettingsEntry, SettingsMeta,
};
use aurcache_db::settings;
use sea_orm::{ActiveValue::Set, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use std::collections::HashMap;
use std::future::Future;

/// `pkg_id` standing for "applies to the whole server".
///
/// A sentinel rather than NULL because the column is NOT NULL, which the
/// `UNIQUE (pkg_id, key)` constraint depends on: NULLs do not compare equal, so
/// a nullable column would let the same global key be inserted twice.
pub const GLOBAL_PKG_ID: i32 = -1;

async fn set_settings_bulk<I>(entries: I, db: &DatabaseConnection) -> anyhow::Result<()>
where
    I: IntoIterator<Item = (Setting, Option<i32>, Option<String>)>,
{
    let mut inserts = Vec::new();
    let mut deletes = Vec::new();

    for (st, pkg_id, value) in entries {
        let s = st.meta();
        let internal_pkg_id = pkg_id.unwrap_or(GLOBAL_PKG_ID); // Use -1 for global

        match value {
            Some(v) => {
                inserts.push(settings::ActiveModel {
                    key: Set(s.key.to_string()),
                    pkg_id: Set(Some(internal_pkg_id)),
                    value: Set(Some(v)),
                    ..Default::default()
                });
            }
            None => {
                deletes.push((s.key.to_string(), internal_pkg_id));
            }
        }
    }

    // 1️⃣ DELETE overrides
    if !deletes.is_empty() {
        let mut condition = sea_orm::Condition::any();

        for (key, pid) in deletes {
            let c = sea_orm::Condition::all()
                .add(settings::Column::Key.eq(key))
                .add(settings::Column::PkgId.eq(pid));

            condition = condition.add(c);
        }

        settings::Entity::delete_many()
            .filter(condition)
            .exec(db)
            .await?;
    }

    // 2️⃣ UPSERT remaining values
    if !inserts.is_empty() {
        settings::Entity::insert_many(inserts)
            .on_conflict(
                sea_orm::sea_query::OnConflict::columns([
                    settings::Column::Key,
                    settings::Column::PkgId,
                ])
                .update_column(settings::Column::Value)
                .to_owned(),
            )
            .exec(db)
            .await?;
    }

    Ok(())
}

async fn get_setting<T>(
    setting_type: Setting,
    pkg_id: Option<i32>,
    db: &DatabaseConnection,
) -> SettingsEntry<T>
where
    T: ParseSetting,
{
    let setting = setting_type.meta();

    // Helper to parse a string or fallback to default with a warning
    let parse_or_default = |val: &str, context: &str| -> T {
        match T::parse_setting(val) {
            Ok(parsed) => parsed,
            Err(e) => {
                tracing::warn!(
                    "Failed to parse {context}: {e}. Using default '{}'.",
                    setting.default
                );
                parse_default(&setting)
            }
        }
    };

    // Resolution order, highest → lowest:
    //   1. Per-package row  — explicit user intent for one package wins
    //                         over the deployment-wide env baseline.
    //   2. ENV variable     — admin's deploy-time override of the global default.
    //   3. Global row       — UI-set baseline.
    //   4. Static default.

    // 1. Per-package row. Having no row is the common case and not an error;
    // only a failed query is worth reporting.
    if let Some(pid) = pkg_id {
        match settings::Entity::find()
            .filter(settings::Column::Key.eq(setting.key))
            .filter(settings::Column::PkgId.eq(pid))
            .one(db)
            .await
        {
            Ok(Some(settings::Model { value: Some(v), .. })) => {
                return SettingsEntry {
                    value: parse_or_default(
                        &v,
                        &format!("pkg setting {} pkg={}", setting.key, pid),
                    ),
                    source: SettingSource::Package,
                };
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(
                "Failed to fetch pkg-specific setting {} pkg={pid}: {e}. Falling back.",
                setting.key
            ),
        }
    }

    // 2. ENV variable
    if let Some(env_name) = setting.env_name
        && let Ok(env_value) = std::env::var(env_name)
    {
        return SettingsEntry {
            value: parse_or_default(&env_value, &format!("ENV {env_name}")),
            source: SettingSource::Env,
        };
    }

    // 3. Global row
    if let Ok(Some(global)) = settings::Entity::find()
        .filter(settings::Column::Key.eq(setting.key))
        .filter(settings::Column::PkgId.eq(GLOBAL_PKG_ID))
        .one(db)
        .await
        && let Some(v) = global.value
    {
        return SettingsEntry {
            value: parse_or_default(&v, &format!("global setting {}", setting.key)),
            source: SettingSource::Global,
        };
    }

    // 4. Static default.
    SettingsEntry {
        value: parse_default(&setting),
        source: SettingSource::Default,
    }
}

/// Resolve one setting for many packages in two queries, whatever the count.
///
/// The dashboard polls, so per-package lookups would repeat every tick, and a
/// release wave can put dozens of packages out of date at once. Package
/// overrides come back in one `pkg_id IN (…)` query, the env and global value
/// once each — keeping the `Package -> Env -> Global -> Default` order in one
/// place rather than re-deriving it at the call site.
async fn get_settings_many<T>(
    setting_type: Setting,
    pkg_ids: &[i32],
    db: &DatabaseConnection,
) -> anyhow::Result<Vec<SettingsEntry<T>>>
where
    T: ParseSetting,
{
    if pkg_ids.is_empty() {
        return Ok(Vec::new());
    }
    let setting = setting_type.meta();

    let parse_or_default = |val: &str, context: &str| -> T {
        match T::parse_setting(val) {
            Ok(parsed) => parsed,
            Err(e) => {
                tracing::warn!(
                    "Failed to parse {context}: {e}. Using default '{}'.",
                    setting.default
                );
                parse_default(&setting)
            }
        }
    };

    let overrides: HashMap<i32, String> = settings::Entity::find()
        .filter(settings::Column::Key.eq(setting.key))
        .filter(settings::Column::PkgId.is_in(pkg_ids.to_vec()))
        .all(db)
        .await?
        .into_iter()
        .filter_map(|row| {
            let pkg_id = row.pkg_id?;
            let value = row.value?;
            Some((pkg_id, value))
        })
        .collect();

    let env_value = setting
        .env_name
        .and_then(|env_name| std::env::var(env_name).ok());

    let global_value: Option<String> = settings::Entity::find()
        .filter(settings::Column::Key.eq(setting.key))
        .filter(settings::Column::PkgId.eq(GLOBAL_PKG_ID))
        .one(db)
        .await?
        .and_then(|row| row.value);

    Ok(pkg_ids
        .iter()
        .map(|pkg_id| {
            if let Some(v) = overrides.get(pkg_id) {
                return SettingsEntry {
                    value: parse_or_default(
                        v,
                        &format!("pkg setting {} pkg={pkg_id}", setting.key),
                    ),
                    source: SettingSource::Package,
                };
            }
            if let Some(env_value) = &env_value
                && let Some(env_name) = setting.env_name
            {
                return SettingsEntry {
                    value: parse_or_default(env_value, &format!("ENV {env_name}")),
                    source: SettingSource::Env,
                };
            }
            if let Some(v) = &global_value {
                return SettingsEntry {
                    value: parse_or_default(v, &format!("global setting {}", setting.key)),
                    source: SettingSource::Global,
                };
            }
            SettingsEntry {
                value: parse_default(&setting),
                source: SettingSource::Default,
            }
        })
        .collect())
}

/// Parse a setting's built-in default. The defaults are compile-time constants
/// chosen to match each setting's type, so a failure here is a programming bug.
fn parse_default<T: ParseSetting>(setting: &SettingsMeta) -> T {
    T::parse_setting(setting.default).unwrap_or_else(|e| {
        panic!(
            "built-in default '{}' for setting {} is not parseable: {e}",
            setting.default, setting.key
        )
    })
}

pub trait SettingsTraits {
    fn get_all(
        db: &DatabaseConnection,
        pkgid: Option<i32>,
    ) -> impl Future<Output = anyhow::Result<ApplicationSettings>> + Send;
    fn get<T: ParseSetting>(
        setting: Setting,
        pkgid: Option<i32>,
        db: &DatabaseConnection,
    ) -> impl Future<Output = SettingsEntry<T>> + Send;
    /// Resolve one setting for a set of packages in two queries, whatever the
    /// count — package overrides in one `pkg_id IN (…)` query, env and global
    /// once — preserving `Package -> Env -> Global -> Default` order.
    fn get_many<T: ParseSetting>(
        setting: Setting,
        pkg_ids: &[i32],
        db: &DatabaseConnection,
    ) -> impl Future<Output = anyhow::Result<Vec<SettingsEntry<T>>>> + Send;
    fn patch<I>(
        db: &DatabaseConnection,
        settings: I,
    ) -> impl Future<Output = anyhow::Result<()>> + Send
    where
        I: IntoIterator<Item = (Setting, Option<i32>, Option<String>)> + Send;
}

impl SettingsTraits for ApplicationSettings {
    async fn get_all(db: &DatabaseConnection, pkgid: Option<i32>) -> anyhow::Result<Self> {
        // Independent reads over one pooled connection: eight serial awaits
        // would pay up to sixteen point queries (package + global each) in
        // turn on every scheduler tick and settings read.
        let (
            version_check_interval,
            auto_update_interval,
            job_timeout,
            max_artifact_size,
            date_format,
            build_on_new_version,
            persistent_builddir,
            parse_network,
        ) = tokio::join!(
            get_setting(Setting::VersionCheckInterval, pkgid, db),
            get_setting(Setting::AutoUpdateInterval, pkgid, db),
            get_setting(Setting::JobTimeout, pkgid, db),
            get_setting::<ByteSize>(Setting::MaxArtifactSize, pkgid, db),
            get_setting(Setting::DateFormat, pkgid, db),
            get_setting(Setting::BuildOnNewVersion, pkgid, db),
            get_setting(Setting::PersistentBuilddir, pkgid, db),
            get_setting(Setting::ParseNetwork, pkgid, db),
        );
        Ok(Self {
            version_check_interval,
            auto_update_interval,
            job_timeout,
            max_artifact_size: SettingsEntry {
                value: max_artifact_size.value.0,
                source: max_artifact_size.source,
            },
            date_format,
            build_on_new_version,
            persistent_builddir,
            parse_network,
        })
    }

    async fn get<T: ParseSetting>(
        setting: Setting,
        pkgid: Option<i32>,
        db: &DatabaseConnection,
    ) -> SettingsEntry<T> {
        get_setting(setting, pkgid, db).await
    }

    async fn patch<I>(db: &DatabaseConnection, settings: I) -> anyhow::Result<()>
    where
        I: IntoIterator<Item = (Setting, Option<i32>, Option<String>)> + Send,
    {
        set_settings_bulk(settings, db).await
    }

    async fn get_many<T: ParseSetting>(
        setting: Setting,
        pkg_ids: &[i32],
        db: &DatabaseConnection,
    ) -> anyhow::Result<Vec<SettingsEntry<T>>> {
        get_settings_many(setting, pkg_ids, db).await
    }
}
