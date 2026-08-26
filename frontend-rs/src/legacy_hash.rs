//! Keeps links made against the Dart frontend working.
//!
//! Flutter web routes through the URL fragment unless told otherwise, and the
//! Dart app never opted out — so every link and bookmark anyone made points at
//! `/#/builds`. This frontend routes on the path, and the server now serves the
//! app shell for unknown paths, so `/builds` works directly.
//!
//! Those old URLs would otherwise land on `/` and silently show the dashboard
//! instead of the page that was asked for. Rewriting the fragment to a path
//! before the router starts is what makes them resolve.

use wasm_bindgen::JsValue;

/// The path a legacy fragment URL meant, if it is one.
///
/// `None` when there is nothing to migrate, so a normal URL is left alone.
fn path_for_legacy_hash(hash: &str) -> Option<String> {
    // Only `#/…`. A bare `#`, or a fragment that is a real anchor rather than a
    // route, is not a Dart route and must not be turned into one.
    let route = hash.strip_prefix("#/")?;
    Some(format!("/{route}"))
}

/// Rewrite a legacy `#/route` URL to `/route`, in place.
///
/// Uses `replaceState` rather than a redirect so the old URL leaves no history
/// entry — going back would otherwise return to it and bounce forward again.
/// Runs before the router reads the location, so the router only ever sees the
/// migrated path.
pub fn migrate_legacy_hash_url() {
    let Some(window) = web_sys::window() else {
        return;
    };
    let Ok(hash) = window.location().hash() else {
        return;
    };
    let Some(path) = path_for_legacy_hash(&hash) else {
        return;
    };
    if let Ok(history) = window.history() {
        let _ = history.replace_state_with_url(&JsValue::NULL, "", Some(&path));
    }
}

#[cfg(test)]
mod tests {
    use super::path_for_legacy_hash;

    /// The URLs the Dart app produced.
    #[test]
    fn dart_fragment_routes_become_paths() {
        assert_eq!(path_for_legacy_hash("#/builds"), Some("/builds".into()));
        assert_eq!(
            path_for_legacy_hash("#/package/hello"),
            Some("/package/hello".into())
        );
        assert_eq!(path_for_legacy_hash("#/"), Some("/".into()));
    }

    /// Anything that is not a fragment route is left alone, so a normal URL is
    /// never rewritten out from under the router.
    #[test]
    fn ordinary_urls_are_not_touched() {
        assert_eq!(path_for_legacy_hash(""), None);
        assert_eq!(path_for_legacy_hash("#"), None);
        // A real anchor, not a route.
        assert_eq!(path_for_legacy_hash("#section-2"), None);
    }
}
