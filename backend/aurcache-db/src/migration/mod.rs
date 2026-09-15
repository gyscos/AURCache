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
mod m20260818_000000_remote_workers;
mod m20260824_000000_worker_routing;
mod m20260826_000000_package_source_metadata;
mod m20260827_000000_build_number;
mod m20260827_000001_download_counts;
mod m20260831_000000_file_size;
mod m20260831_000001_files_package_id_index;
mod m20260831_000002_build_size;
mod m20260831_000003_bulk_adds;
mod m20260901_000000_operations;
mod m20260902_000000_build_logs_to_files;
mod m20260906_000000_worker_kind;
mod m20260907_000000_build_peak_memory;
mod m20260910_000000_files_package_fk;
mod m20260911_000000_build_trigger_end_reason;
mod m20260914_000000_publishing_is_pending;
mod m20260915_000000_retire_build_limit_settings;

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
            // These add columns to `packages` and must run before
            // `m20260508_..._dependency_resolution_combined`, which queries
            // `packages::Entity` using the *current* (compiled) entity shape —
            // so on a fresh database it selects every column the entity has
            // today, including ones added by later migrations. Anything that
            // adds a `packages` column belongs above that line, whatever its
            // date says.
            Box::new(m20260601_000000_package_patch::Migration),
            Box::new(m20260826_000000_package_source_metadata::Migration),
            Box::new(m20260508_000000_dependency_resolution_combined::Migration),
            Box::new(m20260515_000000_api_tokens::Migration),
            Box::new(m20260814_000000_package_vcs_sources::Migration),
            Box::new(m20260818_000000_remote_workers::Migration),
            Box::new(m20260824_000000_worker_routing::Migration),
            Box::new(m20260827_000000_build_number::Migration),
            Box::new(m20260827_000001_download_counts::Migration),
            Box::new(m20260831_000000_file_size::Migration),
            Box::new(m20260831_000001_files_package_id_index::Migration),
            Box::new(m20260831_000002_build_size::Migration),
            Box::new(m20260831_000003_bulk_adds::Migration),
            Box::new(m20260901_000000_operations::Migration),
            Box::new(m20260902_000000_build_logs_to_files::Migration),
            Box::new(m20260906_000000_worker_kind::Migration),
            Box::new(m20260907_000000_build_peak_memory::Migration),
            Box::new(m20260910_000000_files_package_fk::Migration),
            Box::new(m20260911_000000_build_trigger_end_reason::Migration),
            Box::new(m20260914_000000_publishing_is_pending::Migration),
            Box::new(m20260915_000000_retire_build_limit_settings::Migration),
        ]
    }
}
