use sea_orm_migration::prelude::*;

mod create;
mod m20240907_131839_platform_buildflags;
mod m20250213_223900_activity_log;
mod m20251015_230000_pkg_sources;
mod m20251106_100000_build_version;
mod m20251107_000000_build_flags_no_install;
mod m20251204_160000_settings;
pub mod m20260508_000000_dependency_resolution_combined;
mod m20260515_000000_api_tokens;
mod m20260601_000000_package_patch;
mod m20260814_000000_package_vcs_sources;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(create::Migration),
            Box::new(m20240907_131839_platform_buildflags::Migration),
            Box::new(m20250213_223900_activity_log::Migration),
            Box::new(m20251106_100000_build_version::Migration),
            Box::new(m20251015_230000_pkg_sources::Migration),
            Box::new(m20251204_160000_settings::Migration),
            Box::new(m20251107_000000_build_flags_no_install::Migration),
            // Must run before `m20260508_..._dependency_resolution_combined`, which
            // queries `packages::Entity` using the *current* (compiled) entity shape.
            Box::new(m20260601_000000_package_patch::Migration),
            Box::new(m20260508_000000_dependency_resolution_combined::Migration),
            Box::new(m20260515_000000_api_tokens::Migration),
            Box::new(m20260814_000000_package_vcs_sources::Migration),
        ]
    }
}
