//! One module per route.
//!
//! Screens that are not ported yet render [`NotPorted`] rather than nothing, so
//! the menu is honest about what the Rust frontend does and does not cover.

mod build;
mod not_found;
mod package_source;
mod packages;
mod placeholder;

pub use build::Build;
pub use not_found::NotFound;
pub use package_source::PackageSource;
pub use packages::Packages;
pub use placeholder::NotPorted;

use crate::routes::Route;
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
pub fn Builds() -> Element {
    rsx! { NotPorted { title: "Builds", note: "Next up: the same table shape as Packages, linking into the build log." } }
}

#[component]
pub fn Package(pkgbase: String) -> Element {
    rsx! {
        div { class: "space-y-4",
            h1 { class: "text-2xl font-bold font-mono", "{pkgbase}" }
            NotPorted {
                title: "Package detail",
                note: "Being redesigned rather than ported — the Dart screen is not the target layout.",
            }
            Link {
                class: "btn btn-sm",
                to: Route::PackageSource { pkgbase: pkgbase.clone(), path: vec![] },
                "Edit sources"
            }
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
