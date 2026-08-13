use crate::auth::username_for_api_token;
use rocket::Request;
use rocket::http::Status;
use rocket::request::{FromRequest, Outcome};
use sea_orm::DatabaseConnection;

#[derive(Debug, Clone)]
pub struct OauthEnabled(pub bool);

#[derive(Debug)]
pub struct Authenticated {
    pub username: Option<String>,
}

#[derive(Debug)]
pub enum LoginError {
    InvalidData,
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for Authenticated {
    type Error = LoginError;

    async fn from_request(req: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        let oauth_enabled = req
            .rocket()
            .state::<OauthEnabled>()
            .unwrap_or(&OauthEnabled(false));
        if oauth_enabled.0 {
            if let Some(authenticated) = req
                .cookies()
                .get_private("token")
                .and_then(|cookie| cookie.value().parse().ok())
                .map(|_: String| {
                    let username: Option<String> = req
                        .cookies()
                        .get_private("username")
                        .and_then(|cookie| cookie.value().parse().ok());

                    Authenticated { username }
                })
            {
                return Outcome::Success(authenticated);
            }

            let bearer_token = req
                .headers()
                .get_one("Authorization")
                .and_then(|value| value.strip_prefix("Bearer "))
                .map(str::trim)
                .filter(|token| !token.is_empty());

            let Some(bearer_token) = bearer_token else {
                return Outcome::Error((Status::Unauthorized, LoginError::InvalidData));
            };

            let Some(db) = req.rocket().state::<DatabaseConnection>() else {
                return Outcome::Error((Status::InternalServerError, LoginError::InvalidData));
            };

            match username_for_api_token(db, bearer_token).await {
                Ok(Some(username)) => Outcome::Success(Authenticated {
                    username: Some(username),
                }),
                Ok(None) => Outcome::Error((Status::Unauthorized, LoginError::InvalidData)),
                Err(_) => Outcome::Error((Status::InternalServerError, LoginError::InvalidData)),
            }
        } else {
            Outcome::Success(Authenticated { username: None })
        }
    }
}
