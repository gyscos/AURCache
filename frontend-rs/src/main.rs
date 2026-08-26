//! A Rust frontend for AURCache, in Dioxus.
//!
//! Shares the API types with the server through `aurcache-types` rather than
//! redeclaring them, which is the main argument for the port: the
//! hand-maintained models in the Dart tree stop existing, and a change to a
//! response shape becomes a compile error here instead of a runtime surprise.
//!
//! The port is partial. Routes that are not implemented render a "not ported"
//! card, so the gap is visible rather than hidden.

// `rsx!` turns `"{value}"` interpolation into a `format!`, and the lint reads
// that expansion as redundant. Its suggested `.to_string()` is not valid where
// a text node is expected, so every hit is a false positive from macro output.
#![allow(clippy::useless_format)]

mod api;
mod format;
mod legacy_hash;
mod routes;
mod screens;
mod shell;
mod status;
mod theme;

use dioxus::prelude::*;
use routes::Route;

fn main() {
    // Before the router reads the URL, so a `#/builds` bookmark from the Dart
    // frontend resolves to `/builds` instead of quietly showing the dashboard.
    legacy_hash::migrate_legacy_hash_url();

    dioxus::launch(App);
}

#[component]
fn App() -> Element {
    // Routes are real paths. That works on a reload or a pasted link because
    // the server answers unknown non-API paths with the app shell; see
    // `aurcache_api::spa`.
    rsx! {
        Router::<Route> {}
    }
}
