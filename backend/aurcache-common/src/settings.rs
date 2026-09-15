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
    pub max_concurrent_builds: SettingsEntry<u32>,
    pub version_check_interval: SettingsEntry<u32>,
    pub auto_update_interval: SettingsEntry<Option<String>>,
    pub job_timeout: SettingsEntry<u32>,
    /// Largest package file a worker may upload, in bytes. Written as a size
    /// (`20G`); see [`Setting::MaxArtifactSize`].
    pub max_artifact_size: SettingsEntry<u64>,
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
    MaxConcurrentBuilds,
    VersionCheckInterval,
    AutoUpdateInterval,
    BuildOnNewVersion,
    PersistentBuilddir,
    DateFormat,
    JobTimeout,
    MaxArtifactSize,
    BuilderImage,
    MakepkgConf,
    PacmanConf,
}

/// Keys of settings that no longer exist, which may still be stored or dumped.
///
/// `cpu_limit` and `memory_limit` were server settings that nothing read once
/// builds moved to workers: limits apply where the build runs, so a worker sets
/// its own (`WORKER_BUILD_MEMORY_MAX`, `WORKER_BUILD_CPUS`). A migration deletes
/// their rows, and a restore skips them so an older dump does not bring them
/// back.
pub const RETIRED_SETTING_KEYS: &[&str] = &["cpu_limit", "memory_limit"];

impl Setting {
    /// Every setting there is.
    ///
    /// [`Self::from_key`] is derived from this rather than written out
    /// separately. The two used to be independent lists, and they drifted:
    /// `date_format` and `build_on_new_version` were served by `GET /settings`
    /// but rejected by `PATCH /settings/<key>` as unknown, so neither could be
    /// changed through the API at all.
    pub const ALL: [Self; 11] = [
        Self::MaxConcurrentBuilds,
        Self::VersionCheckInterval,
        Self::AutoUpdateInterval,
        Self::BuildOnNewVersion,
        Self::PersistentBuilddir,
        Self::DateFormat,
        Self::JobTimeout,
        Self::MaxArtifactSize,
        Self::BuilderImage,
        Self::MakepkgConf,
        Self::PacmanConf,
    ];

    /// This setting's stable key, environment variable, and built-in default.
    #[must_use]
    pub const fn meta(&self) -> SettingsMeta {
        match self {
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
            // On by default. This is what an AUR helper on a workstation
            // already does -- paru and yay keep their build trees between
            // builds and trouble is rare -- and the failure that was assumed
            // to make it risky does not exist: `extract_sources` unpacks over
            // the tree and bsdtar overwrites, so `prepare()` always patches
            // pristine sources. The chroot is still rebuilt from a fresh
            // snapshot every time; only the tree survives.
            //
            // Kept as a setting rather than made unconditional so a package
            // that does turn out to mind has a remedy that is not "delete a
            // directory on the worker by hand".
            Self::PersistentBuilddir => SettingsMeta {
                key: "persistent_builddir",
                env_name: Some("PERSISTENT_BUILDDIR"),
                default: "true",
            },
            Self::JobTimeout => SettingsMeta {
                key: "job_timeout",
                env_name: Some("JOB_TIMEOUT"),
                default: "3600",
            },
            // Largest package file a worker may upload, checked per package so
            // one outsized package (an engine, a game) can be allowed more
            // without raising the ceiling for everything. A cap exists at all
            // because the upload lands on the server's disk before anything
            // looks at it. Written as a size, `20G`, the way the worker's own
            // limits are; see `units::parse_size`.
            Self::MaxArtifactSize => SettingsMeta {
                key: "max_artifact_size",
                env_name: Some("MAX_ARTIFACT_SIZE"),
                default: "20G",
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

    /// Whether `value` is one this setting can use, checked before it is stored.
    ///
    /// Reading falls back to the default when a stored value does not parse,
    /// which is right for a value that is already there and wrong for one being
    /// saved: `max_artifact_size = 40GiB` misspelled as `40 gigs` would quietly
    /// mean 20G. Only the settings that are not plain numbers or free text are
    /// checked here; the rest parse as they always did.
    ///
    /// # Errors
    /// A message saying what was expected.
    pub fn validate(&self, value: &str) -> Result<(), String> {
        match self {
            Self::MaxArtifactSize => crate::units::parse_size(value)
                .filter(|&bytes| bytes > 0)
                .map(|_| ())
                .ok_or_else(|| {
                    format!("{value:?} is not a size (expected e.g. 20G, 512M or a byte count)")
                }),
            _ => Ok(()),
        }
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
                Setting::MaxConcurrentBuilds => 0,
                Setting::VersionCheckInterval => 1,
                Setting::AutoUpdateInterval => 2,
                Setting::BuildOnNewVersion => 3,
                Setting::PersistentBuilddir => 4,
                Setting::DateFormat => 5,
                Setting::JobTimeout => 6,
                Setting::MaxArtifactSize => 7,
                Setting::BuilderImage => 8,
                Setting::MakepkgConf => 9,
                Setting::PacmanConf => 10,
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
