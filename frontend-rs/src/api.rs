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
///
/// The client is wired to send the page to the login flow when the session is
/// refused. A server restart turns every API call into a 401 — the cookie can
/// no longer be decoded — and the only way back in is OAuth again, exactly the
/// trip the server gives an unauthenticated full-page load. A plain page jump
/// rather than a router navigate: `/api/login` is a server route, and a SPA
/// navigation to it would render a frontend error screen instead of the login.
/// One process-wide client: building one per call would throw away connection
/// pooling on every poll tick, and re-read the DOM for a base URL that cannot
/// change without a page load.
static CLIENT: std::sync::LazyLock<Result<AurCacheClient, String>> =
    std::sync::LazyLock::new(|| {
        // Read once: each call reaches into the DOM, and the two reads below
        // must agree with each other anyway.
        let base = api_base();
        let login_url = format!("{base}/login");
        AurCacheClient::new(base, None)
            .map(|client| {
                client.on_unauthorized(move || {
                    if let Some(window) = web_sys::window() {
                        let _ = window.location().assign(&login_url);
                    }
                })
            })
            .map_err(|e| e.to_string())
    });

pub fn client() -> Result<AurCacheClient, String> {
    CLIENT.clone()
}
