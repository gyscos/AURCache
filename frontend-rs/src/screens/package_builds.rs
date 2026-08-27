//! Every build of one package.
//!
//! The package page shows only the latest build and the one currently in the
//! repository; this is where the rest of the history lives.

use crate::api::client;
use crate::dates::DateOnly;
use crate::format::format_duration;
use crate::listing::{ListHeader, Pager, paginate};
use crate::routes::Route;
use crate::status::BuildStatusBadge;
use aurcache_client::Build;
use dioxus::prelude::*;

const WIDE_ONLY: &str = "hidden md:table-cell";

async fn load(pkgbase: String) -> Result<Vec<Build>, String> {
    client()?
        // A sub-resource rather than a query filter: a pkgbase may contain `+`,
        // which is literal in a path but decodes to a space in a query value.
        // No limit: the page is paged in the browser, and a capped fetch
        // would have silently ended a long history at 100.
        .list_builds(Some(&pkgbase), None, None)
        .await
        .map_err(|e| e.to_string())
}

#[component]
pub fn PackageBuilds(pkgbase: String) -> Element {
    let package = use_resource({
        let pkgbase = pkgbase.clone();
        move || {
            let pkgbase = pkgbase.clone();
            async move {
                crate::api::client()?
                    .get_package(&pkgbase)
                    .await
                    .map_err(|e| e.to_string())
            }
        }
    });

    let builds = use_resource({
        let pkgbase = pkgbase.clone();
        move || load(pkgbase.clone())
    });
    let page = use_signal(|| 0usize);

    rsx! {
        div { class: "space-y-4",
        match &*package.read_unchecked() {
            Some(Ok(pkg)) => rsx! {
                crate::screens::PackageHeader {
                    pkg: pkg.clone(),
                    trail: vec![("Builds".to_string(), None)],
                }
            },
            _ => rsx! {},
        }
        div { class: "card bg-base-100 shadow-xl",
            div { class: "card-body",
                ListHeader { title: "Builds" }

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
                    Some(Ok(list)) => {
                        let current = paginate(list, page());
                        rsx! {
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
                                    for build in current.items.iter() {
                                        tr { key: "{build.number}", class: "hover",
                                            td {
                                                Link {
                                                    class: "link link-primary font-mono",
                                                    to: Route::Build {
                                                        pkgbase: build.pkg_name.clone(),
                                                        number: build.number,
                                                    },
                                                    "{build.number}"
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
                        Pager {
                            page,
                            index: current.index,
                            pages: current.pages,
                            first: current.first,
                            count: current.items.len(),
                            total: current.total,
                        }
                        if current.pages <= 1 {
                            div { class: "text-sm opacity-60 pt-2", "{list.len()} builds" }
                        }
                    }
                    },
                }
            }
        }
        }
    }
}
