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
