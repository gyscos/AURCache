//! The packages list.
//!
//! Shares `aurcache_client::SimplePackage` with the server rather than
//! redeclaring it, which is the main argument for a Rust frontend: the
//! hand-maintained models in the Dart tree stop existing, and a change to a
//! response shape becomes a compile error here.

use crate::api::client;
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
pub fn Packages() -> Element {
    let packages = use_resource(load_packages);

    rsx! {
        div { class: "card bg-base-100 shadow-xl",
            div { class: "card-body",
                div { class: "flex items-center",
                    h2 { class: "card-title", "Packages" }
                    div { class: "flex-1" }
                    button { class: "btn btn-primary btn-sm", "Add package" }
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
                    Some(Ok(list)) => rsx! {
                        div { class: "overflow-x-auto",
                            table { class: "table table-zebra",
                                thead {
                                    tr {
                                        th { "Package" }
                                        th { "Version" }
                                        // Upstream and Actions are dropped on a
                                        // narrow screen rather than scrolled to.
                                        // With all five columns the status badge
                                        // is clipped mid-word on a phone, which
                                        // reads as missing data. The Dart table
                                        // drops the same two below 700px.
                                        th { class: "{WIDE_ONLY}", "Upstream" }
                                        th { "Status" }
                                        th { class: "{WIDE_ONLY} text-right", "Actions" }
                                    }
                                }
                                tbody {
                                    for pkg in list.iter() {
                                        tr { key: "{pkg.name}", class: "hover",
                                            td {
                                                Link {
                                                    class: "link link-primary font-medium",
                                                    // pkgbase is the public identifier.
                                                    to: Route::Package { pkgbase: pkg.name.clone() },
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
                                                button { class: "btn btn-ghost btn-xs", "Update" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        div { class: "text-sm opacity-60 pt-2", "{list.len()} packages" }
                    },
                }
            }
        }
    }
}
