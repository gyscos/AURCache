//! The builds list.

use crate::api::client;
use crate::dates::DateOnly;
use crate::format::format_duration;
use crate::listing::{
    ListControls, ListHeader, Pager, Sort, SortDir, SortKey, SortableHeader, StatusFilter,
    filter_builds, paginate, sort_builds, use_url_search,
};
use crate::routes::Route;
use crate::status::BuildStatusBadge;
use aurcache_client::Build;
use dioxus::prelude::*;

/// Columns that only appear once there is room for them, matching the Dart
/// table, which drops the same ones below 700px.
const WIDE_ONLY: &str = "hidden md:table-cell";

/// Every build, paged in the browser.
///
/// This asked for 100 with no way to reach a second page, so the history simply
/// stopped there and said nothing about it — and unlike the package list, a
/// build list only grows.
async fn load_builds() -> Result<Vec<Build>, String> {
    client()?
        .list_builds(None, None, None)
        .await
        .map_err(|e| e.to_string())
}

#[component]
pub fn Builds(q: String) -> Element {
    let builds = use_resource(load_builds);
    let query = use_url_search(q, true, |q| Route::Builds { q });
    let status = use_signal(|| StatusFilter::ANY);
    // Newest first: a build list is a log.
    let sort = use_signal(|| Sort {
        key: SortKey::Time,
        dir: SortDir::Desc,
    });
    let mut page = use_signal(|| 0usize);

    // Changing what is listed puts you back at the start; see the same effect
    // on the packages screen.
    use_effect(use_reactive(&(query(), status()), move |_| page.set(0)));

    rsx! {
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
                        div { class: "alert", span { "No builds yet." } }
                    },
                    Some(Ok(list)) => {
                        let mut shown = filter_builds(list, &query(), status());
                        sort_builds(&mut shown, sort());
                        let (found, total) = (shown.len(), list.len());
                        let current = paginate(&shown, page());
                        rsx! {
                        ListControls {
                            query,
                            status,
                            placeholder: "Filter by package or build…",
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
                                        // Sorting by build is sorting by
                                        // package: the package is the first
                                        // and most significant part of the
                                        // name now that there is no separate
                                        // column for it.
                                        SortableHeader { label: "Build", column: SortKey::Name, sort, class: "" }
                                        th { class: "{WIDE_ONLY}", "Version" }
                                        SortableHeader { label: "Started", column: SortKey::Time, sort, class: "{WIDE_ONLY}" }
                                        th { class: "{WIDE_ONLY}", "Duration" }
                                        th { class: "{WIDE_ONLY}", "Platform" }
                                        SortableHeader { label: "Status", column: SortKey::Status, sort, class: "" }
                                    }
                                }
                                tbody {
                                    for build in current.items.iter() {
                                        tr {
                                            key: "{build.pkg_name}/{build.number}",
                                            class: "hover cursor-pointer",
                                            // The row is the build: that is what
                                            // a build list is a list of.
                                            onclick: {
                                                let pkgbase = build.pkg_name.clone();
                                                let number = build.number;
                                                move |_| {
                                                    navigator().push(Route::Build {
                                                        pkgbase: pkgbase.clone(),
                                                        number,
                                                    });
                                                }
                                            },
                                            // A build's name already contains
                                            // its package, so a Package column
                                            // beside it said the same thing
                                            // twice. Every cell in the row now
                                            // leads to the same place, and the
                                            // package is one more click away,
                                            // from the build's own page.
                                            td {
                                                Link {
                                                    class: "font-mono",
                                                    to: Route::Build {
                                                        pkgbase: build.pkg_name.clone(),
                                                        number: build.number,
                                                    },
                                                    onclick: move |e: MouseEvent| e.stop_propagation(),
                                                    "{build.pkg_name}/{build.number}"
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
                            div { class: "text-sm opacity-60 pt-2", "{total} builds" }
                        }
                    }
                    },
                }
            }
        }
    }
}
