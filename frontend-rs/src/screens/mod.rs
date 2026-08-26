//! One module per route.
//!
//! Screens that are not ported yet render [`NotPorted`] rather than nothing, so
//! the menu is honest about what the Rust frontend does and does not cover.

mod build;
mod builds;
mod not_found;
mod package;
mod package_builds;
mod package_source;
mod packages;
mod placeholder;

pub use build::Build;
pub use builds::Builds;
pub use not_found::NotFound;
pub use package::Package;
pub use package_builds::PackageBuilds;
pub use package_source::PackageSource;
pub use packages::Packages;
pub use placeholder::NotPorted;

use crate::routes::Route;
use dioxus::prelude::*;

/// A way back to the package a sub-page belongs to.
///
/// The source editor had none: opening it to look at a PKGBUILD and deciding
/// not to change anything left the browser's back button as the only exit,
/// which is not an exit the page offers.
#[component]
pub fn PackageBreadcrumb(pkgbase: String, here: String) -> Element {
    rsx! {
        div { class: "flex items-center gap-2 text-sm",
            Link {
                class: "link link-primary font-mono",
                to: Route::Package { pkgbase: pkgbase.clone() },
                "← {pkgbase}"
            }
            span { class: "opacity-40", "/" }
            span { class: "opacity-70", "{here}" }
        }
    }
}

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

#[component]
pub fn Activities() -> Element {
    rsx! { NotPorted { title: "Activities", note: "" } }
}

#[component]
pub fn Settings() -> Element {
    rsx! { NotPorted { title: "Settings", note: "" } }
}

#[component]
pub fn ConfigFiles() -> Element {
    rsx! { NotPorted { title: "Config files", note: "" } }
}
