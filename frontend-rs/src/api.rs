//! Where the API lives, and how to reach it.

use aurcache_client::AurCacheClient;

/// The API is on the same origin the page was served from, so the base URL is
/// derived at runtime rather than baked in — the same rule the Dart client uses
/// for release builds. The fallback is the dev server, matching Dart's debug
/// build.
pub fn api_base() -> String {
    web_sys::window()
        .and_then(|w| w.location().origin().ok())
        .map_or_else(
            || "http://localhost:8080/api".to_string(),
            |origin| format!("{origin}/api"),
        )
}

/// A client against the current origin.
///
/// No token: the session is a cookie the server set during OAuth, and the
/// browser attaches it to same-origin requests on its own.
pub fn client() -> Result<AurCacheClient, String> {
    AurCacheClient::new(api_base(), None).map_err(|e| e.to_string())
}
