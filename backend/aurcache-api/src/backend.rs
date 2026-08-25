use crate::activity::activity;
use crate::aur::search;
use crate::auth::regenerate_api_token_endpoint;
use crate::build::{
    build_output, cancel_build, delete_build, get_build, list_builds, list_package_builds,
    retry_build,
};
use crate::health::health;
use crate::package::{
    get_package, package_add_endpoint, package_del, package_list, package_source_file,
    package_source_file_update, package_source_files, package_source_preview_file,
    package_source_preview_files, package_update_endpoint, package_update_entity_endpoint,
};
use crate::settings::{
    package_setting_get, package_setting_patch, package_setting_reset, package_settings,
    setting_get, setting_patch, setting_reset, settings,
};
use crate::stats::{dashboard_graph_data, stats, user_info};
use rocket::{Route, routes};

#[must_use]
pub fn build_api() -> Vec<Route> {
    routes![
        search,
        regenerate_api_token_endpoint,
        package_list,
        package_add_endpoint,
        package_del,
        package_update_entity_endpoint,
        build_output,
        delete_build,
        list_builds,
        list_package_builds,
        stats,
        dashboard_graph_data,
        user_info,
        get_build,
        get_package,
        retry_build,
        package_update_endpoint,
        package_source_files,
        package_source_file,
        package_source_file_update,
        package_source_preview_files,
        package_source_preview_file,
        cancel_build,
        health,
        activity,
        settings,
        package_settings,
        package_setting_get,
        package_setting_patch,
        package_setting_reset,
        setting_get,
        setting_patch,
        setting_reset
    ]
}
