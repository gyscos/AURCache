use anyhow::Context;
use aurcache_db::api_tokens;
use aurcache_db::prelude::ApiTokens;
use rand::RngCore;
use reqwest::header::AUTHORIZATION;
use rocket::get;
use rocket::http::{Cookie, CookieJar, SameSite};
use rocket::response::Redirect;
use rocket::response::status::Unauthorized;
use rocket::serde::json::Json;
use rocket::{State, post};
use rocket_oauth2::{OAuth2, TokenResponse};
use sea_orm::{ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set};
use sha2::{Digest, Sha256};
use tracing::debug;
use utoipa::OpenApi;
use utoipa::ToSchema;

use crate::models::authenticated::Authenticated;

#[derive(OpenApi)]
#[openapi(paths(oauth_login, oauth_callback, regenerate_api_token_endpoint))]
pub struct AuthApi;

#[derive(serde::Deserialize, Debug)]
pub struct OauthUserInfo {
    //pub email: String,
    pub name: String,
    //pub preferred_username: String,
    //pub nickname: String,
}

#[derive(serde::Serialize, ToSchema)]
pub struct ApiTokenResponse {
    pub token: String,
}

pub fn hash_api_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

fn generate_api_token() -> String {
    let mut bytes = [0_u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

pub async fn username_for_api_token(
    db: &DatabaseConnection,
    token: &str,
) -> Result<Option<String>, sea_orm::DbErr> {
    ApiTokens::find()
        .filter(api_tokens::Column::TokenHash.eq(hash_api_token(token)))
        .one(db)
        .await
        .map(|token_row| token_row.map(|row| row.username))
}

pub async fn has_api_token(
    db: &DatabaseConnection,
    username: &str,
) -> Result<bool, sea_orm::DbErr> {
    Ok(ApiTokens::find()
        .filter(api_tokens::Column::Username.eq(username))
        .one(db)
        .await?
        .is_some())
}

pub async fn regenerate_api_token(
    db: &DatabaseConnection,
    username: &str,
) -> anyhow::Result<String> {
    let token = generate_api_token();
    let token_hash = hash_api_token(&token);

    if let Some(existing) = ApiTokens::find()
        .filter(api_tokens::Column::Username.eq(username))
        .one(db)
        .await?
    {
        let mut active: api_tokens::ActiveModel = existing.into();
        active.token_hash = Set(token_hash);
        active.save(db).await?;
    } else {
        api_tokens::ActiveModel {
            username: Set(username.to_string()),
            token_hash: Set(token_hash),
            ..Default::default()
        }
        .save(db)
        .await?;
    }

    Ok(token)
}

#[utoipa::path(
    responses(
            (status = 200, description = "Redirect to oidc login endpoint"),
    )
)]
#[get("/login")]
pub fn oauth_login(oauth2: OAuth2<OauthUserInfo>, cookies: &CookieJar<'_>) -> Redirect {
    oauth2
        .get_redirect(cookies, &["profile", "openid", "email"])
        .unwrap()
}

#[utoipa::path(
    responses(
            (status = 200, description = "Oauth callback (called by oidc provider)"),
    )
)]
#[get("/auth")]
pub async fn oauth_callback(
    token: TokenResponse<OauthUserInfo>,
    cookies: &CookieJar<'_>,
) -> Result<Redirect, Unauthorized<String>> {
    cookies.add_private(
        Cookie::build(("token", token.access_token().to_string()))
            .same_site(SameSite::Lax)
            .build(),
    );

    let user_info: OauthUserInfo = reqwest::Client::builder()
        .build()
        .context("failed to build reqwest client")
        .map_err(|e| Unauthorized(e.to_string()))?
        .get(std::env::var("OAUTH_USERINFO_URI").map_err(|e| Unauthorized(e.to_string()))?)
        .header(AUTHORIZATION, format!("Bearer {}", token.access_token()))
        .send()
        .await
        .context("failed to complete request")
        .map_err(|e| Unauthorized(e.to_string()))?
        .json()
        .await
        .context("failed to deserialize response")
        .map_err(|e| Unauthorized(e.to_string()))?;

    let real_name = user_info.name;
    debug!("Logged in username: {real_name}");

    // Set a private cookie with the user's name, and redirect to the home page.
    cookies.add_private(
        Cookie::build(("username", real_name.clone()))
            .same_site(SameSite::Lax)
            .build(),
    );

    Ok(Redirect::to("/"))
}

#[utoipa::path(
    responses(
        (status = 200, description = "Regenerate the signed-in user's API token", body = ApiTokenResponse),
    )
)]
#[post("/token/regenerate")]
pub async fn regenerate_api_token_endpoint(
    db: &State<DatabaseConnection>,
    a: Authenticated,
) -> Result<Json<ApiTokenResponse>, Unauthorized<String>> {
    let username = a
        .username
        .ok_or_else(|| Unauthorized("No authenticated user available".to_string()))?;
    let token = regenerate_api_token(db, &username)
        .await
        .map_err(|e| Unauthorized(e.to_string()))?;
    Ok(Json(ApiTokenResponse { token }))
}

#[cfg(test)]
mod tests {
    use super::{has_api_token, regenerate_api_token, username_for_api_token};
    use aurcache_db::migration::Migrator;
    use sea_orm::Database;
    use sea_orm_migration::MigratorTrait;

    #[tokio::test]
    async fn regenerated_token_replaces_previous_value() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();

        let first = regenerate_api_token(&db, "alice").await.unwrap();
        let second = regenerate_api_token(&db, "alice").await.unwrap();

        assert!(has_api_token(&db, "alice").await.unwrap());
        assert_ne!(first, second);
        assert_eq!(
            username_for_api_token(&db, &first).await.unwrap(),
            None,
            "old token should stop authenticating after regeneration"
        );
        assert_eq!(
            username_for_api_token(&db, &second).await.unwrap(),
            Some("alice".to_string())
        );
    }
}
