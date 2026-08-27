use crate::aur::AURApi;
use crate::auth::{OauthUserInfo, oauth_callback, oauth_login};
use crate::backend::build_api;
use crate::custom_file_server::CustomFileServer;
#[cfg(feature = "static")]
use crate::embed::CustomHandler;
use crate::models::authenticated::OauthEnabled;
use crate::utils::config::oauth_config_from_env;
use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_db::action::Action;
use aurcache_db::helpers::downloads::DownloadCounter;
use aurcache_utils::snapshot::SnapshotStore;
use rocket::config::SecretKey;
use rocket::fairing::AdHoc;
use rocket::http::private::cookie::Key;
use rocket::{Config, routes};
use rocket_oauth2::HyperRustlsAdapter;
use sea_orm::DatabaseConnection;
use std::env;
use std::net::Ipv4Addr;
use std::sync::Arc;
use tokio::sync::broadcast::Sender;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};
use utoipa::openapi::security::{AuthorizationCode, Flow, OAuth2, Scopes};
use utoipa::{Modify, OpenApi, openapi::security::SecurityScheme};
use utoipa_redoc::{Redoc, Servable as _};
use utoipa_scalar::{Scalar, Servable as _};

fn get_secret_key() -> SecretKey {
    if let Ok(secret_key) = env::var("SECRET_KEY") {
        SecretKey::from(secret_key.as_bytes())
    } else {
        warn!("`SECRET_KEY` env not set, generating random key.");
        SecretKey::from(
            Key::try_generate()
                .expect("no secure RNG available to generate a cookie key")
                .master(),
        )
    }
}

/// Build the mutual-TLS configuration for the worker protocol port: a server
/// certificate issued by the internal CA, with client certificates optional
/// (`mandatory = false`) so enrollment endpoints remain reachable without one.
fn worker_tls_config(ca: &aurcache_ca::Ca) -> Option<rocket::config::TlsConfig> {
    use rocket::config::{MutualTls, TlsConfig};

    let sans = env::var("AURCACHE_TLS_SANS")
        .ok()
        .map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect::<Vec<_>>()
        })
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| vec!["localhost".to_string()]);

    let (cert_pem, key_pem) = match ca.issue_server_cert(sans) {
        Ok(pair) => pair,
        Err(e) => {
            error!("Failed to issue server certificate, TLS disabled: {e}");
            return None;
        }
    };
    let ca_pem = ca.ca_cert_pem().as_bytes().to_vec();

    let tls = TlsConfig::from_bytes(cert_pem.as_bytes(), key_pem.as_bytes())
        .with_mutual(MutualTls::from_bytes(&ca_pem).mandatory(false));
    Some(tls)
}

/// Start the human-facing API/UI listener.
///
/// `store` is the process-wide [`SnapshotStore`]; it must be the same instance
/// handed to the schedulers and the worker listener so every path shares one set
/// of on-disk git checkouts (see `main.rs`).
#[must_use]
pub fn init_api(
    db: DatabaseConnection,
    tx: Sender<Action>,
    store: Arc<SnapshotStore>,
    downloads: Arc<DownloadCounter>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let config = Config {
            address: Ipv4Addr::UNSPECIFIED.into(),
            port: aurcache_types::ports::AURCACHE_HTTP_PORT,
            secret_key: get_secret_key(),
            ..Default::default()
        };

        #[derive(OpenApi)]
        #[openapi(
            nest(
                (path = "/api", api = AURApi, tags = ["AUR"]),
                (path = "/api", api = crate::auth::AuthApi, tags = ["Auth"]),
                (path = "/api", api = crate::build::BuildApi, tags = ["Build"]),
                (path = "/api", api = crate::health::HealthApi, tags = ["Health"]),
                (path = "/api", api = crate::package::PackageApi, tags = ["Package"]),
                (path = "/api", api = crate::stats::StatsApi, tags = ["Stats"]),
                (path = "/api", api = crate::activity::ActivityApi, tags = ["Activity"]),
                (path = "/api", api = crate::settings::SettingsApi, tags = ["Settings"]),
                (path = "/api", api = crate::worker::WorkerApi, tags = ["Worker"]),
            ),
            tags(
                (name = "AUR", description = "AUR management endpoints."),
                (name = "Build", description = "Build management endpoints."),
                (name = "Auth", description = "Authentication"),
                (name = "Health", description = "Health endpoints"),
                (name = "Package", description = "Package management endpoints."),
                (name = "Stats", description = "Statistics endpoints."),
                (name = "Activity", description = "Activity endpoints."),
                (name = "Settings", description = "Settings endpoints."),
                (name = "Worker", description = "Remote build worker protocol and management."),
            ),
            modifiers(&SecurityAddon)
        )]
        struct ApiDoc;

        struct SecurityAddon;

        impl Modify for SecurityAddon {
            fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
                let (Some(components), Ok(oauth_config)) =
                    (openapi.components.as_mut(), oauth_config_from_env())
                else {
                    return;
                };
                components.add_security_scheme(
                    "openid_connect",
                    SecurityScheme::OAuth2(OAuth2::new([Flow::AuthorizationCode(
                        AuthorizationCode::new(
                            oauth_config.provider().auth_uri(),
                            oauth_config.provider().token_uri(),
                            Scopes::new(),
                        ),
                    )])),
                );
            }
        }

        let oauth_config = oauth_config_from_env();
        let mut rock = rocket::custom(config)
            .manage(db.clone())
            .manage(tx)
            .manage(OauthEnabled(oauth_config.is_ok()))
            .manage(ActivityLog::new(db))
            .manage(store)
            // Shared with the repository server, so a package's count includes
            // downloads not yet flushed rather than stalling until they are.
            .manage(downloads)
            .mount("/api/", build_api())
            .mount("/api/", crate::worker::worker_admin_routes())
            .mount("/", Scalar::with_url("/docs", ApiDoc::openapi()))
            .mount("/", Redoc::with_url("/redoc", ApiDoc::openapi()));

        if let Ok(oauth_config) = oauth_config {
            rock = rock
                .mount("/api/", routes![oauth_login, oauth_callback])
                .attach(AdHoc::on_ignite("OAuth Config", |rocket| async {
                    rocket.attach(rocket_oauth2::OAuth2::<OauthUserInfo>::custom(
                        HyperRustlsAdapter::default(),
                        oauth_config,
                    ))
                }));
        }

        #[cfg(feature = "static")]
        let rock = rock.mount("/", CustomHandler);

        let rock = rock.launch().await;
        match rock {
            Ok(_) => info!("Rocket shut down gracefully."),
            Err(err) => error!("Rocket had an error: {err}"),
        }
    })
}

/// Dedicated remote-worker protocol listener: HTTPS with **optional** mutual TLS
/// on `AURCACHE_WORKER_PORT` (default 8083). Enrollment endpoints are reachable
/// without a client certificate; job endpoints require an approved worker's
/// certificate. This is intentionally separate from [`init_api`] so the
/// human-facing API/UI need no TLS of their own (front them with a reverse proxy
/// if desired) while worker mTLS is scoped to exactly this surface.
///
/// If a server certificate cannot be issued from the internal CA, the listener
/// is not started and a warning is logged (workers will be unable to connect).
///
/// `store` is the process-wide [`SnapshotStore`], shared with [`init_api`] and
/// the schedulers: job descriptors built during `claim` resolve sources through
/// the same on-disk checkouts the version-check loop maintains, so the two never
/// race on (or duplicate) a clone of the same package.
#[must_use]
pub fn init_worker_api(
    db: DatabaseConnection,
    ca: aurcache_ca::Ca,
    store: Arc<SnapshotStore>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let Some(tls) = worker_tls_config(&ca) else {
            error!("Worker TLS could not be configured; worker protocol listener disabled");
            return;
        };

        let port = env::var("AURCACHE_WORKER_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(aurcache_types::ports::AURCACHE_WORKER_PORT);

        let config = Config {
            address: Ipv4Addr::UNSPECIFIED.into(),
            port,
            secret_key: get_secret_key(),
            tls: Some(tls),
            ..Default::default()
        };

        info!("Starting remote-worker mTLS protocol listener on port {port}");
        let launch_result = rocket::custom(config)
            .manage(db)
            .manage(ca)
            .manage(store)
            .mount("/api/", crate::worker::worker_protocol_routes())
            .launch()
            .await;
        match launch_result {
            Ok(_) => info!("Worker protocol listener shut down gracefully."),
            Err(err) => error!("Worker protocol listener had an error: {err}"),
        }
    })
}

#[must_use]
pub fn init_repo(downloads: Arc<DownloadCounter>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let config = Config {
            address: Ipv4Addr::UNSPECIFIED.into(),
            port: aurcache_types::ports::AURCACHE_MIRROR_PORT,
            secret_key: get_secret_key(),
            ..Default::default()
        };

        let launch_result = rocket::custom(config)
            // The file server counts what it serves through this; without it
            // in state it simply counts nothing.
            .manage(downloads)
            .mount("/", CustomFileServer::new("./repo"))
            .launch()
            .await;
        match launch_result {
            Ok(_) => info!("Rocket shut down gracefully."),
            Err(err) => error!("Rocket had an error: {err}"),
        }
    })
}
