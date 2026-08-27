//! One module per route.
//!
//! Screens that are not ported yet render [`NotPorted`] rather than nothing, so
//! the menu is honest about what the Rust frontend does and does not cover.

mod activities;
mod build;
mod builds;
mod config_files;
mod not_found;
mod package;
mod package_add;
mod package_builds;
mod package_source;
mod packages;
mod placeholder;
pub mod settings;

pub use activities::Activities;
pub use build::Build;
pub use builds::Builds;
pub use config_files::ConfigFiles;
pub use not_found::NotFound;
pub use package::{Package, PackageHeader};
pub use package_add::PackageAdd;
pub use package_builds::PackageBuilds;
pub use package_source::PackageSource;
pub use packages::Packages;
pub use placeholder::NotPorted;
pub use settings::Settings;

use dioxus::prelude::*;

// ---------------------------------------------------------------------------
// Not ported yet.
//
// Each is its own component rather than one shared stub so the route table
// keeps type-checking against real names, and porting one is a change in one
// place with no route churn.
// ---------------------------------------------------------------------------

#[component]
pub fn Dashboard() -> Element {
    rsx! {
        NotPorted {
            title: "Dashboard",
            note: "Needs a charting story — the Dart version uses fl_chart for the build history graph, which has no direct equivalent here.",
        }
    }
}

#[component]
pub fn Workers() -> Element {
    rsx! { NotPorted { title: "Workers", note: "" } }
}
