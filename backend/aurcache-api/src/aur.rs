use crate::models::aur::ApiPackage;
use crate::models::authenticated::Authenticated;
use aurcache_utils::aur::api::{get_package_info, query_aur};
use rocket::get;
use rocket::response::status::BadRequest;
use rocket::serde::json::Json;
use utoipa::OpenApi;

#[derive(OpenApi)]
#[openapi(paths(search))]
pub struct AURApi;

#[utoipa::path(
    responses(
            (status = 200, description = "Matching AUR packages", body = [ApiPackage]),
    ),
    params(
        ("query", description = "AUR query"),
    )
)]
#[get("/search?<query>")]
pub async fn search(
    query: &str,
    _a: Authenticated,
) -> Result<Json<Vec<ApiPackage>>, BadRequest<String>> {
    if query.len() < 3 {
        // Iterate over the Option, giving either a single result or an empty list.
        return get_package_info(query)
            .await
            .map(|pkg| Json(pkg.into_iter().map(ApiPackage::from).collect()))
            .map_err(|e| BadRequest(e.to_string()));
    }

    query_aur(query)
        .await
        .map(|packages| Json(packages.into_iter().map(ApiPackage::from).collect()))
        .map_err(|e| BadRequest(e.to_string()))
}
