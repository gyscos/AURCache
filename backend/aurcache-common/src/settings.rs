use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Where a setting's resolved value came from in the lookup hierarchy.
///
/// Resolution order (highest precedence first): `Package` → `Env` → `Global` → `Default`.
#[derive(ToSchema, Deserialize, Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SettingSource {
    /// Forced by an environment variable.
    Env,
    /// Stored on the per-package settings row.
    Package,
    /// Stored on the global settings row.
    Global,
    /// No row stored — using the static built-in default.
    Default,
}

#[derive(ToSchema, Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct SettingsEntry<T> {
    pub value: T,
    pub source: SettingSource,
}

#[derive(ToSchema, Deserialize, Serialize, Clone, Debug, PartialEq)]
pub struct ApplicationSettings {
    pub cpu_limit: SettingsEntry<u32>,
    pub memory_limit: SettingsEntry<i32>,
    pub max_concurrent_builds: SettingsEntry<u32>,
    pub version_check_interval: SettingsEntry<u32>,
    pub auto_update_interval: SettingsEntry<Option<String>>,
    pub job_timeout: SettingsEntry<u32>,
    pub builder_image: SettingsEntry<String>,
    /// Default date format for the web UI. A browser may override it
    /// locally; nothing writes a per-client choice back here.
    pub date_format: SettingsEntry<String>,
    pub build_on_new_version: SettingsEntry<bool>,
    /// Keep this package's build tree between builds instead of starting from
    /// an empty one. See `design/persistent-build-directory.md`.
    pub persistent_builddir: SettingsEntry<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SettingsMeta {
    pub key: &'static str,
    pub env_name: Option<&'static str>,
    pub default: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Setting {
    CpuLimit,
    MemoryLimit,
    MaxConcurrentBuilds,
    VersionCheckInterval,
    AutoUpdateInterval,
    BuildOnNewVersion,
    PersistentBuilddir,
    DateFormat,
    JobTimeout,
    BuilderImage,
    MakepkgConf,
    PacmanConf,
}

impl Setting {
    /// Every setting there is.
    ///
    /// [`Self::from_key`] is derived from this rather than written out
    /// separately. The two used to be independent lists, and they drifted:
    /// `date_format` and `build_on_new_version` were served by `GET /settings`
    /// but rejected by `PATCH /settings/<key>` as unknown, so neither could be
    /// changed through the API at all.
    pub const ALL: [Self; 12] = [
        Self::CpuLimit,
        Self::MemoryLimit,
        Self::MaxConcurrentBuilds,
        Self::VersionCheckInterval,
        Self::AutoUpdateInterval,
        Self::BuildOnNewVersion,
        Self::PersistentBuilddir,
        Self::DateFormat,
        Self::JobTimeout,
        Self::BuilderImage,
        Self::MakepkgConf,
        Self::PacmanConf,
    ];

    /// This setting's stable key, environment variable, and built-in default.
    #[must_use]
    pub const fn meta(&self) -> SettingsMeta {
        match self {
            Self::CpuLimit => SettingsMeta {
                key: "cpu_limit",
                env_name: Some("CPU_LIMIT"),
                default: "0",
            },
            Self::MemoryLimit => SettingsMeta {
                key: "memory_limit",
                env_name: Some("MEMORY_LIMIT"),
                default: "-1",
            },
            Self::MaxConcurrentBuilds => SettingsMeta {
                key: "max_concurrent_builds",
                env_name: Some("MAX_CONCURRENT_BUILDS"),
                default: "1",
            },
            Self::VersionCheckInterval => SettingsMeta {
                key: "version_check_interval",
                env_name: Some("VERSION_CHECK_INTERVAL"),
                default: "3600",
            },
            Self::AutoUpdateInterval => SettingsMeta {
                key: "auto_update_interval",
                env_name: Some("AUTO_UPDATE_SCHEDULE"),
                default: "", // parses to None
            },
            // How the web UI writes absolute dates: field order, zero
            // padding, and a 12- or 24-hour clock, e.g. `dmy-nopad-12`. A
            // browser can override it for itself, which is not stored here —
            // this is the shared starting point, not a per-user preference.
            Self::DateFormat => SettingsMeta {
                key: "date_format",
                env_name: Some("DATE_FORMAT"),
                default: "ymd-pad-24",
            },
            // Queue the rebuild the moment a new version is detected, rather
            // than waiting for the `auto_update_interval` window. The version
            // check is the only thing that knows a package is out of date —
            // including VCS packages whose upstream moved without a pkgver
            // bump — so that is where the build belongs.
            //
            // Off by default: the existing behaviour is to flag a package and
            // leave rebuilding to an opt-in schedule.
            Self::BuildOnNewVersion => SettingsMeta {
                key: "build_on_new_version",
                env_name: Some("BUILD_ON_NEW_VERSION"),
                default: "false",
            },
            // Off by default: a clean tree per build is the guarantee chroot
            // builds exist to provide, and reuse trades it away. Worth it only
            // where a rebuild costs hours.
            Self::PersistentBuilddir => SettingsMeta {
                key: "persistent_builddir",
                env_name: Some("PERSISTENT_BUILDDIR"),
                default: "false",
            },
            Self::JobTimeout => SettingsMeta {
                key: "job_timeout",
                env_name: Some("JOB_TIMEOUT"),
                default: "3600",
            },
            Self::BuilderImage => SettingsMeta {
                key: "builder_image",
                env_name: Some("BUILDER_IMAGE"),
                default: "ghcr.io/lukas-heiligenbrunner/aurcache-builder:latest",
            },
            Self::MakepkgConf => SettingsMeta {
                key: "makepkg_conf",
                env_name: None,
                default: "",
            },
            Self::PacmanConf => SettingsMeta {
                key: "pacman_conf",
                env_name: None,
                default: "",
            },
        }
    }

    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.meta().key == key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The guard on the drift described on [`Setting::ALL`]: a variant left out
    /// of that list is unreachable by key, which is exactly how two settings
    /// became unwritable without anything failing to compile.
    ///
    /// Iterating `ALL` cannot see a variant that is missing *from* `ALL`, so
    /// the position comes from an exhaustive match instead. A new variant does
    /// not compile until it is given a position here, and it then fails this
    /// test until it is added to `ALL` too.
    #[test]
    fn every_setting_is_reachable_by_its_key() {
        fn position(setting: Setting) -> usize {
            match setting {
                Setting::CpuLimit => 0,
                Setting::MemoryLimit => 1,
                Setting::MaxConcurrentBuilds => 2,
                Setting::VersionCheckInterval => 3,
                Setting::AutoUpdateInterval => 4,
                Setting::BuildOnNewVersion => 5,
                Setting::PersistentBuilddir => 6,
                Setting::DateFormat => 7,
                Setting::JobTimeout => 8,
                Setting::BuilderImage => 9,
                Setting::MakepkgConf => 10,
                Setting::PacmanConf => 11,
            }
        }

        let mut listed = [false; Setting::ALL.len()];
        for setting in Setting::ALL {
            let key = setting.meta().key;
            let found = Setting::from_key(key).unwrap_or_else(|| panic!("{key} is not in ALL"));
            assert_eq!(found.meta().key, key);
            listed[position(setting)] = true;
        }
        assert!(
            listed.iter().all(|&seen| seen),
            "a setting is missing from Setting::ALL"
        );
    }

    #[test]
    fn keys_and_env_names_are_unique() {
        let mut keys: Vec<_> = Setting::ALL.iter().map(|s| s.meta().key).collect();
        let count = keys.len();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), count, "two settings share a key");

        let mut envs: Vec<_> = Setting::ALL
            .iter()
            .filter_map(|s| s.meta().env_name)
            .collect();
        let count = envs.len();
        envs.sort_unstable();
        envs.dedup();
        assert_eq!(
            envs.len(),
            count,
            "two settings share an environment variable"
        );
    }
}
