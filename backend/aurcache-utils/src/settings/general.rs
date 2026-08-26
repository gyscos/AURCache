use crate::settings::meta::SettingsMetaTrait;
use crate::settings::parser::ParseSetting;
use aurcache_db::settings;
use aurcache_types::settings::{
    ApplicationSettings, Setting, SettingSource, SettingsEntry, SettingsMeta,
};
use sea_orm::{ActiveValue::Set, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use std::future::Future;

const GLOBAL_PKG_ID: i32 = -1;

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
    fn patch<I>(
        db: &DatabaseConnection,
        settings: I,
    ) -> impl Future<Output = anyhow::Result<()>> + Send
    where
        I: IntoIterator<Item = (Setting, Option<i32>, Option<String>)> + Send;
}

impl SettingsTraits for ApplicationSettings {
    async fn get_all(db: &DatabaseConnection, pkgid: Option<i32>) -> anyhow::Result<Self> {
        Ok(Self {
            cpu_limit: get_setting(Setting::CpuLimit, pkgid, db).await,
            memory_limit: get_setting(Setting::MemoryLimit, pkgid, db).await,
            max_concurrent_builds: get_setting(Setting::MaxConcurrentBuilds, pkgid, db).await,
            version_check_interval: get_setting(Setting::VersionCheckInterval, pkgid, db).await,
            auto_update_interval: get_setting(Setting::AutoUpdateInterval, pkgid, db).await,
            job_timeout: get_setting(Setting::JobTimeout, pkgid, db).await,
            builder_image: get_setting(Setting::BuilderImage, pkgid, db).await,
            date_format: get_setting(Setting::DateFormat, pkgid, db).await,
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
}
