//! The route table.
//!
//! Paths mirror the Dart app's (`frontend/lib/components/routing/router.dart`)
//! so existing links keep working. Each variant names a component of the same
//! name, and the fields are that component's props — a route that does not line
//! up with its screen is a compile error rather than a blank page.

// `unreachable_code` fires inside the `Routable` expansion rather than on
// anything written here, and the derive generates an `impl` an attribute on the
// enum does not reach -- so the allow has to sit on the module. It started
// warning with the 2026-08-28 nightly; the alternative is failing every build
// on a lint about code we do not write.
//
// The scope is this file, which is the route table, one match over it and its
// tests. None of that has any business containing unreachable code, so the
// suppression is unlikely to hide anything of ours.
#![allow(unreachable_code)]

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

        // `#:q` carries the filter. In the fragment rather than a query string
        // because dioxus's query handling has two defects a search term walks
        // straight into: an empty value still writes `?q=`, so every unfiltered
        // URL grows a dangling marker, and the value is not escaped on write
        // while being split on `&` on read, so a term containing one is
        // truncated. A fragment writes nothing when empty and is one opaque
        // string. Nothing here is server-rendered and the filtering is
        // client-side, so keeping the term out of the request costs nothing.
        #[route("/builds#:q")]
        Builds { q: String },


        #[route("/packages#:q")]
        Packages { q: String },
        // A dialog over the list rather than a page, but with a URL of its own
        // so it can be linked to and Back closes it.
        #[route("/packages/add#:q")]
        PackageAdd { q: String },
        #[route("/package/:pkgbase")]
        Package { pkgbase: String },
        #[route("/package/:pkgbase/builds")]
        PackageBuilds { pkgbase: String },
        #[route("/package/:pkgbase/build/:number")]
        Build { pkgbase: String, number: i32 },
        // Named for what it holds. It was "settings" while it also carried
        // platforms, flags and removal; those live on the package page now,
        // and two config files are not a settings page.
        #[route("/package/:pkgbase/config-files")]
        PackageConfigFiles { pkgbase: String },
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
            Route::Builds { .. } => Some(MenuEntry::Builds),
            // A single build lives under its package now — same URL, same
            // breadcrumb, same header — so it highlights Packages with the
            // rest of them rather than jumping the menu to Builds.
            Route::Packages { .. }
            | Route::PackageAdd { .. }
            | Route::Package { .. }
            | Route::PackageBuilds { .. }
            | Route::Build { .. }
            | Route::PackageSource { .. }
            | Route::PackageConfigFiles { .. } => Some(MenuEntry::Packages),
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

        assert_eq!(entry_for("/packages"), Some(MenuEntry::Packages));
        assert_eq!(entry_for("/package/hello"), Some(MenuEntry::Packages));
        assert_eq!(
            entry_for("/package/hello/builds"),
            Some(MenuEntry::Packages)
        );
        assert_eq!(
            entry_for("/package/hello/build/3"),
            Some(MenuEntry::Packages)
        );
        assert_eq!(
            entry_for("/package/hello/source/PKGBUILD"),
            Some(MenuEntry::Packages)
        );
        assert_eq!(
            entry_for("/package/hello/config-files"),
            Some(MenuEntry::Packages)
        );
    }

    /// A page with no search must not carry a marker for one. This is the
    /// concrete reason the term is in the fragment: dioxus writes `?q=` even
    /// for an empty value, so every unfiltered URL would have grown one.
    #[test]
    fn an_empty_search_leaves_no_trace_in_the_url() {
        assert_eq!(
            Route::Packages { q: String::new() }.to_string(),
            "/packages"
        );
        assert_eq!(Route::Builds { q: String::new() }.to_string(), "/builds");
        assert_eq!(
            Route::PackageAdd { q: String::new() }.to_string(),
            "/packages/add"
        );
    }

    /// A URL with no fragment is the same page as one with an empty search,
    /// which is what makes a plain `/packages` link work.
    #[test]
    fn a_url_without_a_fragment_is_an_empty_search() {
        assert_eq!(
            Route::from_str("/packages").unwrap(),
            Route::Packages { q: String::new() }
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
            Route::Builds { q: String::new() },
            // A search term goes in the fragment, and these are the shapes that
            // broke it as a query parameter: `&` split the value in two, and an
            // empty one still wrote a marker into every unfiltered URL.
            Route::Builds {
                q: "a&b".to_string(),
            },
            Route::Packages {
                q: "aewm++".to_string(),
            },
            Route::PackageAdd {
                q: "two words".to_string(),
            },
            Route::Build {
                pkgbase: "hello".into(),
                number: 42,
            },
            Route::Packages { q: String::new() },
            Route::Package {
                pkgbase: "hello".into(),
            },
            Route::PackageSource {
                pkgbase: "hello".into(),
                path: vec!["PKGBUILD".into()],
            },
            Route::PackageConfigFiles {
                pkgbase: "hello".into(),
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
