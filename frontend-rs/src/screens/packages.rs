//! The packages list.
//!
//! Shares `aurcache_client::SimplePackage` with the server rather than
//! redeclaring it, which is the main argument for a Rust frontend: the
//! hand-maintained models in the Dart tree stop existing, and a change to a
//! response shape becomes a compile error here.

use crate::api::client;
use crate::listing::{
    ListControls, ListHeader, Sort, SortDir, SortKey, SortableHeader, StatusFilter,
    filter_packages, sort_packages, use_url_search,
};
use crate::routes::Route;
use crate::status::StatusBadge;
use aurcache_client::SimplePackage;
use dioxus::prelude::*;

/// Columns that only appear once there is room for them.
const WIDE_ONLY: &str = "hidden md:table-cell";

async fn load_packages() -> Result<Vec<SimplePackage>, String> {
    client()?
        .list_packages(Some(100), None)
        .await
        .map_err(|e| e.to_string())
}

#[component]
pub fn Packages(
    q: String,
    /// Whether this list owns the URL's fragment.
    ///
    /// False behind the add dialog, which owns it there — two components
    /// writing the same fragment would fight, and the list would win by
    /// clearing the dialog's search on every keystroke.
    #[props(default = true)]
    sync_url: bool,
) -> Element {
    let packages = use_resource(load_packages);
    let query = use_url_search(q, sync_url, |q| Route::Packages { q });
    let status = use_signal(|| StatusFilter::ANY);
    let sort = use_signal(|| Sort {
        key: SortKey::Name,
        dir: SortDir::Asc,
    });

    rsx! {
        div { class: "card bg-base-100 shadow-xl",
            div { class: "card-body",
                ListHeader { title: "Packages",
                    Link { class: "btn btn-primary btn-sm", to: Route::PackageAdd { q: String::new() },
                        "Add package"
                    }
                }

                match &*packages.read_unchecked() {
                    None => rsx! {
                        div { class: "flex justify-center p-8",
                            span { class: "loading loading-spinner loading-lg" }
                        }
                    },
                    Some(Err(e)) => rsx! {
                        div { class: "alert alert-error",
                            span { "Could not load packages: {e}" }
                        }
                    },
                    Some(Ok(list)) if list.is_empty() => rsx! {
                        div { class: "alert", span { "No packages yet." } }
                    },
                    Some(Ok(list)) => {
                        let mut shown = filter_packages(list, &query(), status());
                        sort_packages(&mut shown, sort());
                        let (found, total) = (shown.len(), list.len());
                        rsx! {
                        ListControls {
                            query,
                            status,
                            placeholder: "Filter packages…",
                            shown: found,
                            total,
                        }
                        if shown.is_empty() {
                            div { class: "alert mt-2", span { "Nothing matches that filter." } }
                        } else {
                        div { class: "overflow-x-auto",
                            table { class: "table table-zebra",
                                thead {
                                    tr {
                                        SortableHeader { label: "Package", column: SortKey::Name, sort, class: "" }
                                        th { "Version" }
                                        // Upstream and Actions are dropped on a
                                        // narrow screen rather than scrolled to.
                                        th { class: "{WIDE_ONLY}", "Upstream" }
                                        SortableHeader { label: "Status", column: SortKey::Status, sort, class: "" }
                                        th { class: "{WIDE_ONLY} text-right", "Actions" }
                                    }
                                }
                                tbody {
                                    for pkg in shown.iter() {
                                        tr {
                                            key: "{pkg.name}",
                                            class: "hover cursor-pointer",
                                            // The whole row is the target, but
                                            // the name stays a real link so the
                                            // address is copyable, middle-click
                                            // opens a tab, and keyboard users
                                            // have something to focus — none of
                                            // which a bare row handler gives.
                                            onclick: {
                                                let pkgbase = pkg.name.clone();
                                                move |_| {
                                                    navigator().push(Route::Package {
                                                        pkgbase: pkgbase.clone(),
                                                    });
                                                }
                                            },
                                            td {
                                                Link {
                                                    // Monospace, matching the
                                                    // package page's heading:
                                                    // a pkgbase is an identifier
                                                    // and reads as one.
                                                    //
                                                    // Not `link link-primary`:
                                                    // when everything in the row
                                                    // navigates, underlining one
                                                    // cell implies the rest does
                                                    // not.
                                                    class: "font-mono",
                                                    // pkgbase is the public identifier.
                                                    to: Route::Package { pkgbase: pkg.name.clone() },
                                                    // Otherwise the click reaches
                                                    // the row too and pushes the
                                                    // same route twice, leaving a
                                                    // duplicate history entry.
                                                    onclick: move |e: MouseEvent| e.stop_propagation(),
                                                    "{pkg.name}"
                                                }
                                            }
                                            td { class: "font-mono text-sm",
                                                {pkg.latest_version.clone().unwrap_or_else(|| "—".into())}
                                            }
                                            td { class: "{WIDE_ONLY} font-mono text-sm opacity-70",
                                                {pkg.upstream_version.clone().unwrap_or_else(|| "—".into())}
                                            }
                                            td { StatusBadge { status: pkg.status, outofdate: pkg.outofdate } }
                                            td { class: "{WIDE_ONLY} text-right",
                                                button {
                                                    class: "btn btn-ghost btn-xs",
                                                    // A button inside a clickable
                                                    // row has to claim its own
                                                    // click, or pressing it also
                                                    // navigates away.
                                                    onclick: move |e: MouseEvent| e.stop_propagation(),
                                                    "Update"
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        }
                        div { class: "text-sm opacity-60 pt-2", "{total} packages" }
                    }
                    },
                }
            }
        }
    }
}
