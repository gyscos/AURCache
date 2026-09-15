use crate::models::authenticated::Authenticated;
use crate::models::settings::{SettingResponse, SettingValue};
use crate::utils::error::{ApiError, err};
use aurcache_common::settings::{ApplicationSettings, Setting};
use aurcache_utils::settings::general::SettingsTraits;
use rocket::http::Status;
use rocket::serde::json::Json;
use rocket::{State, delete, get, patch};
use sea_orm::DatabaseConnection;
use utoipa::OpenApi;

#[derive(OpenApi)]
#[openapi(paths(
    settings,
    package_settings,
    setting_get,
    package_setting_get,
    setting_patch,
    package_setting_patch,
    setting_reset,
    package_setting_reset
))]
pub struct SettingsApi;

fn parse_setting(key: &str) -> Result<Setting, ApiError> {
    Setting::from_key(key)
        .ok_or_else(|| err(Status::NotFound, format!("Unknown setting key: {key}")))
}

// Per-package settings are a sub-resource of the package rather than a
// `?pkgbase=` filter. That is not only tidier: a pkgbase may contain `+` (187
// AUR packages do, such as `aewm++`), and a `+` in a query value decodes to a
// space, so the filter form silently looked up the wrong name unless every
// caller remembered to percent-encode. In a path segment `+` is literal.
//
// Each operation therefore has one implementation and two thin routes: the
// global one, and the package-scoped one.

async fn settings_impl(
    db: &DatabaseConnection,
    pkg_id: Option<i32>,
) -> Result<Json<ApplicationSettings>, ApiError> {
    ApplicationSettings::get_all(db, pkg_id)
        .await
        .map(Json)
        .map_err(|e| err(Status::InternalServerError, e))
}

async fn setting_get_impl(
    db: &DatabaseConnection,
    key: &str,
    pkg_id: Option<i32>,
) -> Result<Json<SettingResponse>, ApiError> {
    let setting = parse_setting(key)?;
    let entry = ApplicationSettings::get::<String>(setting, pkg_id, db).await;
    Ok(Json(SettingResponse {
        value: entry.value,
        source: entry.source,
    }))
}

async fn setting_patch_impl(
    db: &DatabaseConnection,
    key: &str,
    pkg_id: Option<i32>,
    value: String,
) -> Result<(), ApiError> {
    let setting = parse_setting(key)?;
    setting
        .validate(&value)
        .map_err(|e| err(Status::BadRequest, e))?;
    ApplicationSettings::patch(db, [(setting, pkg_id, Some(value))])
        .await
        .map_err(|e| err(Status::InternalServerError, e))
}

async fn setting_reset_impl(
    db: &DatabaseConnection,
    key: &str,
    pkg_id: Option<i32>,
) -> Result<(), ApiError> {
    let setting = parse_setting(key)?;
    ApplicationSettings::patch(db, [(setting, pkg_id, None)])
        .await
        .map_err(|e| err(Status::InternalServerError, e))
}

#[utoipa::path(responses((status = 200, description = "Get all settings", body = ApplicationSettings)))]
#[get("/settings")]
pub async fn settings(
    db: &State<DatabaseConnection>,
    _a: Authenticated,
) -> Result<Json<ApplicationSettings>, ApiError> {
    settings_impl(db.inner(), None).await
}

#[utoipa::path(
    responses((status = 200, description = "Get all settings for a package", body = ApplicationSettings)),
    params(("pkgbase" = String, Path, description = "pkgbase of the package"))
)]
#[get("/package/<pkgbase>/settings")]
pub async fn package_settings(
    db: &State<DatabaseConnection>,
    pkgbase: &str,
    _a: Authenticated,
) -> Result<Json<ApplicationSettings>, ApiError> {
    let pkg_id = crate::package::package_id_for(db.inner(), Some(pkgbase)).await?;
    settings_impl(db.inner(), pkg_id).await
}

/// Fetch a single setting (any key, including the large config-file blobs).
#[utoipa::path(
    responses(
        (status = 200, description = "Get a single setting", body = SettingResponse),
        (status = 404, description = "Unknown setting key"),
    ),
    params(("key" = String, Path, description = "Setting key"))
)]
#[get("/settings/<key>")]
pub async fn setting_get(
    db: &State<DatabaseConnection>,
    key: &str,
    _a: Authenticated,
) -> Result<Json<SettingResponse>, ApiError> {
    setting_get_impl(db.inner(), key, None).await
}

#[utoipa::path(
    responses(
        (status = 200, description = "Get a single setting for a package", body = SettingResponse),
        (status = 404, description = "Unknown setting key or package"),
    ),
    params(
        ("pkgbase" = String, Path, description = "pkgbase of the package"),
        ("key" = String, Path, description = "Setting key"),
    )
)]
#[get("/package/<pkgbase>/settings/<key>")]
pub async fn package_setting_get(
    db: &State<DatabaseConnection>,
    pkgbase: &str,
    key: &str,
    _a: Authenticated,
) -> Result<Json<SettingResponse>, ApiError> {
    let pkg_id = crate::package::package_id_for(db.inner(), Some(pkgbase)).await?;
    setting_get_impl(db.inner(), key, pkg_id).await
}

#[utoipa::path(
    responses(
        (status = 200, description = "Update a single setting"),
        (status = 400, description = "Value not valid for this setting"),
        (status = 404, description = "Unknown setting key"),
    ),
    params(("key" = String, Path, description = "Setting key"))
)]
#[patch("/settings/<key>", data = "<input>")]
pub async fn setting_patch(
    db: &State<DatabaseConnection>,
    key: &str,
    input: Json<SettingValue>,
    _a: Authenticated,
) -> Result<(), ApiError> {
    setting_patch_impl(db.inner(), key, None, input.into_inner().value).await
}

#[utoipa::path(
    responses(
        (status = 200, description = "Update a single setting for a package"),
        (status = 400, description = "Value not valid for this setting"),
        (status = 404, description = "Unknown setting key or package"),
    ),
    params(
        ("pkgbase" = String, Path, description = "pkgbase of the package"),
        ("key" = String, Path, description = "Setting key"),
    )
)]
#[patch("/package/<pkgbase>/settings/<key>", data = "<input>")]
pub async fn package_setting_patch(
    db: &State<DatabaseConnection>,
    pkgbase: &str,
    key: &str,
    input: Json<SettingValue>,
    _a: Authenticated,
) -> Result<(), ApiError> {
    let pkg_id = crate::package::package_id_for(db.inner(), Some(pkgbase)).await?;
    setting_patch_impl(db.inner(), key, pkg_id, input.into_inner().value).await
}

/// Reset a setting back to its default by deleting any stored override.
#[utoipa::path(
    responses(
        (status = 200, description = "Reset a setting to its default"),
        (status = 404, description = "Unknown setting key"),
    ),
    params(("key" = String, Path, description = "Setting key"))
)]
#[delete("/settings/<key>")]
pub async fn setting_reset(
    db: &State<DatabaseConnection>,
    key: &str,
    _a: Authenticated,
) -> Result<(), ApiError> {
    setting_reset_impl(db.inner(), key, None).await
}

#[utoipa::path(
    responses(
        (status = 200, description = "Reset a package setting to its default"),
        (status = 404, description = "Unknown setting key or package"),
    ),
    params(
        ("pkgbase" = String, Path, description = "pkgbase of the package"),
        ("key" = String, Path, description = "Setting key"),
    )
)]
#[delete("/package/<pkgbase>/settings/<key>")]
pub async fn package_setting_reset(
    db: &State<DatabaseConnection>,
    pkgbase: &str,
    key: &str,
    _a: Authenticated,
) -> Result<(), ApiError> {
    let pkg_id = crate::package::package_id_for(db.inner(), Some(pkgbase)).await?;
    setting_reset_impl(db.inner(), key, pkg_id).await
}
