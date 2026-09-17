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
    // Chars, not bytes: a byte length miscounts non-ASCII queries against the
    // minimum the info endpoint needs. One shared tail, so the two branches
    // cannot drift apart again.
    let result = if query.chars().count() < 3 {
        // Iterate over the Option, giving either a single result or an empty list.
        get_package_info(query)
            .await
            .map(|pkg| pkg.into_iter().collect::<Vec<_>>())
    } else {
        query_aur(query).await
    };
    let packages = result.map_err(|e| BadRequest(e.to_string()))?;
    Ok(Json(packages.into_iter().map(ApiPackage::from).collect()))
}
