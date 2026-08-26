//! Every build of one package.
//!
//! The package page shows only the latest build and the one currently in the
//! repository; this is where the rest of the history lives.

use crate::api::client;
use crate::dates::DateOnly;
use crate::format::format_duration;
use crate::routes::Route;
use crate::status::BuildStatusBadge;
use aurcache_client::Build;
use dioxus::prelude::*;

const WIDE_ONLY: &str = "hidden md:table-cell";

async fn load(pkgbase: String) -> Result<Vec<Build>, String> {
    client()?
        // A sub-resource rather than a query filter: a pkgbase may contain `+`,
        // which is literal in a path but decodes to a space in a query value.
        .list_builds(Some(&pkgbase), Some(100), None)
        .await
        .map_err(|e| e.to_string())
}

#[component]
pub fn PackageBuilds(pkgbase: String) -> Element {
    let builds = use_resource({
        let pkgbase = pkgbase.clone();
        move || load(pkgbase.clone())
    });

    rsx! {
        div { class: "card bg-base-100 shadow-xl",
            div { class: "card-body",
                crate::screens::PackageBreadcrumb {
                    pkgbase: pkgbase.clone(),
                    here: "All builds",
                }

                match &*builds.read_unchecked() {
                    None => rsx! {
                        div { class: "flex justify-center p-8",
                            span { class: "loading loading-spinner loading-lg" }
                        }
                    },
                    Some(Err(e)) => rsx! {
                        div { class: "alert alert-error", span { "Could not load builds: {e}" } }
                    },
                    Some(Ok(list)) if list.is_empty() => rsx! {
                        div { class: "alert", span { "This package has never been built." } }
                    },
                    Some(Ok(list)) => rsx! {
                        div { class: "overflow-x-auto",
                            table { class: "table table-zebra",
                                thead {
                                    tr {
                                        th { "Build" }
                                        th { class: "{WIDE_ONLY}", "Version" }
                                        th { class: "{WIDE_ONLY}", "Started" }
                                        th { class: "{WIDE_ONLY}", "Duration" }
                                        th { class: "{WIDE_ONLY}", "Platform" }
                                        th { "Status" }
                                    }
                                }
                                tbody {
                                    for build in list.iter() {
                                        tr { key: "{build.id}", class: "hover",
                                            td {
                                                Link {
                                                    class: "link link-primary font-mono",
                                                    to: Route::Build { id: build.id },
                                                    "#{build.id}"
                                                }
                                            }
                                            td { class: "{WIDE_ONLY} font-mono text-sm", "{build.version}" }
                                            td { class: "{WIDE_ONLY} text-sm opacity-70",
                                                DateOnly { ts: build.start_time }
                                            }
                                            td { class: "{WIDE_ONLY} font-mono text-sm opacity-70",
                                                {format_duration(build.start_time, build.end_time)}
                                            }
                                            td { class: "{WIDE_ONLY} text-sm opacity-70", "{build.platform}" }
                                            td { BuildStatusBadge { status: build.status } }
                                        }
                                    }
                                }
                            }
                        }
                        div { class: "text-sm opacity-60 pt-2", "{list.len()} builds" }
                    },
                }
            }
        }
    }
}
