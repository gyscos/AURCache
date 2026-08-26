//! The route table.
//!
//! Paths mirror the Dart app's (`frontend/lib/components/routing/router.dart`)
//! so existing links keep working. Each variant names a component of the same
//! name, and the fields are that component's props — a route that does not line
//! up with its screen is a compile error rather than a blank page.

use crate::screens::*;
use crate::shell::MenuShell;
use dioxus::prelude::*;

#[derive(Routable, Clone, PartialEq, Debug)]
#[rustfmt::skip]
pub enum Route {
    // Everything inside the shell renders in the drawer's content area, so the
    // side menu is mounted once rather than per screen.
    #[layout(MenuShell)]
        #[route("/")]
        Dashboard {},

        #[route("/builds")]
        Builds {},
        #[route("/build/:id")]
        Build { id: i32 },

        #[route("/packages")]
        Packages {},
        #[route("/package/:pkgbase")]
        Package { pkgbase: String },
        // `:..path` is a catch-all: source paths contain slashes, so a single
        // segment would only ever match files at the top level.
        #[route("/package/:pkgbase/source/:..path")]
        PackageSource { pkgbase: String, path: Vec<String> },

        #[route("/workers")]
        Workers {},
        #[route("/activities")]
        Activities {},
        #[route("/settings")]
        Settings {},
        #[route("/config-files")]
        ConfigFiles {},

        #[route("/:..segments")]
        NotFound { segments: Vec<String> },
}

/// A top-level entry in the side menu.
///
/// Separate from [`Route`] because several routes belong to one entry: viewing
/// a single build keeps "Builds" highlighted, the way the Dart menu matched on
/// a path prefix.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MenuEntry {
    Dashboard,
    Builds,
    Packages,
    Activities,
    Workers,
    Settings,
    ConfigFiles,
}

impl Route {
    /// Which menu entry should be highlighted while this route is open.
    ///
    /// `None` for routes with no home in the menu, so nothing is highlighted
    /// rather than something arbitrary.
    pub fn menu_entry(&self) -> Option<MenuEntry> {
        match self {
            Route::Dashboard { .. } => Some(MenuEntry::Dashboard),
            Route::Builds { .. } | Route::Build { .. } => Some(MenuEntry::Builds),
            Route::Packages { .. } | Route::Package { .. } | Route::PackageSource { .. } => {
                Some(MenuEntry::Packages)
            }
            Route::Activities { .. } => Some(MenuEntry::Activities),
            Route::Workers { .. } => Some(MenuEntry::Workers),
            Route::Settings { .. } => Some(MenuEntry::Settings),
            Route::ConfigFiles { .. } => Some(MenuEntry::ConfigFiles),
            Route::NotFound { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn entry_for(path: &str) -> Option<MenuEntry> {
        Route::from_str(path)
            .unwrap_or_else(|e| panic!("{path} should parse: {e}"))
            .menu_entry()
    }

    /// The whole point of the menu entry being derived from the route: a
    /// sub-page keeps its section highlighted instead of clearing the menu.
    #[test]
    fn sub_pages_highlight_their_section() {
        assert_eq!(entry_for("/builds"), Some(MenuEntry::Builds));
        assert_eq!(entry_for("/build/3"), Some(MenuEntry::Builds));

        assert_eq!(entry_for("/packages"), Some(MenuEntry::Packages));
        assert_eq!(entry_for("/package/hello"), Some(MenuEntry::Packages));
        assert_eq!(
            entry_for("/package/hello/source/PKGBUILD"),
            Some(MenuEntry::Packages)
        );
    }

    /// `/` is the landing route, and hash routing depends on it: an empty
    /// fragment is turned into `/`, so this is what a first visit renders.
    #[test]
    fn the_index_route_is_the_dashboard() {
        assert_eq!(entry_for("/"), Some(MenuEntry::Dashboard));
    }

    /// Nothing in the menu corresponds to a bad URL, so nothing is highlighted.
    #[test]
    fn an_unknown_path_highlights_nothing() {
        assert_eq!(entry_for("/no/such/page"), None);
    }

    /// Round-trips through the string form the URL actually carries. Catches a
    /// route whose `Display` and parser disagree, which would make a `Link`
    /// navigate somewhere it cannot parse back.
    #[test]
    fn routes_round_trip_through_their_url() {
        for route in [
            Route::Dashboard {},
            Route::Builds {},
            Route::Build { id: 42 },
            Route::Packages {},
            Route::Package {
                pkgbase: "hello".into(),
            },
            Route::PackageSource {
                pkgbase: "hello".into(),
                path: vec!["PKGBUILD".into()],
            },
            // A nested source file: the catch-all segment has to survive both
            // directions, or deep links into subdirectories break.
            Route::PackageSource {
                pkgbase: "hello".into(),
                path: vec!["subdir".into(), "patch.diff".into()],
            },
            Route::Workers {},
            Route::Activities {},
            Route::Settings {},
            Route::ConfigFiles {},
        ] {
            let url = route.to_string();
            let parsed =
                Route::from_str(&url).unwrap_or_else(|e| panic!("{url} should parse back: {e}"));
            assert_eq!(parsed, route, "round trip through {url}");
        }
    }

    /// A package name with characters that are legal in pkgbase but meaningful
    /// in a URL. `+` is the one that bit the HTTP layer before.
    #[test]
    fn package_names_with_url_significant_characters_survive() {
        for name in ["gtk3+", "lib32-glibc", "python-3.11", "1337"] {
            let route = Route::Package {
                pkgbase: name.to_string(),
            };
            let url = route.to_string();
            assert_eq!(
                Route::from_str(&url).ok(),
                Some(route),
                "pkgbase {name} broke on the way through {url}"
            );
        }
    }

    /// The package screen links to the editor with no file selected, so the
    /// empty catch-all has to survive the trip too.
    #[test]
    fn the_source_editor_link_works_with_no_file_selected() {
        let route = Route::PackageSource {
            pkgbase: "hello".into(),
            path: vec![],
        };
        let url = route.to_string();
        assert_eq!(
            Route::from_str(&url).ok(),
            Some(route),
            "empty source path did not round trip through {url}"
        );
    }
}
