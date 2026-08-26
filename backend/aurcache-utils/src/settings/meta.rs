use aurcache_types::settings::{Setting, SettingsMeta};

pub trait SettingsMetaTrait {
    fn meta(&self) -> SettingsMeta;
}

impl SettingsMetaTrait for Setting {
    fn meta(&self) -> SettingsMeta {
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
            // Queue the rebuild the moment a new version is detected, rather
            // than waiting for the `auto_update_interval` window. The version
            // check is the only thing that knows a package is out of date —
            // including VCS packages whose upstream moved without a pkgver
            // bump — so that is where the build belongs.
            //
            // Off by default: the existing behaviour is to flag a package and
            // leave rebuilding to an opt-in schedule.
            // The default the web UI starts from. A browser can override it
            // for itself, which is not stored here — this is the shared
            // starting point, not a per-user preference.
            // How the web UI writes absolute dates: field order, zero
            // padding, and a 12- or 24-hour clock, e.g. `dmy-nopad-12`. A
            // browser can override it for itself, which is not stored here —
            // this is the shared starting point, not a per-user preference.
            Self::DateFormat => SettingsMeta {
                key: "date_format",
                env_name: Some("DATE_FORMAT"),
                default: "ymd-pad-24",
            },
            Self::BuildOnNewVersion => SettingsMeta {
                key: "build_on_new_version",
                env_name: Some("BUILD_ON_NEW_VERSION"),
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
}
