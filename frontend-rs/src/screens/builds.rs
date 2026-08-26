//! The builds list.

use crate::api::client;
use crate::dates::DateOnly;
use crate::format::format_duration;
use crate::listing::{
    ListControls, ListHeader, Sort, SortDir, SortKey, SortableHeader, StatusFilter, filter_builds,
    sort_builds,
};
use crate::routes::Route;
use crate::status::BuildStatusBadge;
use aurcache_client::Build;
use dioxus::prelude::*;

/// Columns that only appear once there is room for them, matching the Dart
/// table, which drops the same ones below 700px.
const WIDE_ONLY: &str = "hidden md:table-cell";

async fn load_builds() -> Result<Vec<Build>, String> {
    client()?
        .list_builds(None, Some(100), None)
        .await
        .map_err(|e| e.to_string())
}

#[component]
pub fn Builds() -> Element {
    let builds = use_resource(load_builds);
    let query = use_signal(String::new);
    let status = use_signal(|| StatusFilter::ANY);
    // Newest first: a build list is a log.
    let sort = use_signal(|| Sort {
        key: SortKey::Time,
        dir: SortDir::Desc,
    });

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
                        rsx! {
                        ListControls {
                            query,
                            status,
                            placeholder: "Filter by package…",
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
                                        th { "Build" }
                                        SortableHeader { label: "Package", column: SortKey::Name, sort, class: "" }
                                        th { class: "{WIDE_ONLY}", "Version" }
                                        SortableHeader { label: "Started", column: SortKey::Time, sort, class: "{WIDE_ONLY}" }
                                        th { class: "{WIDE_ONLY}", "Duration" }
                                        th { class: "{WIDE_ONLY}", "Platform" }
                                        SortableHeader { label: "Status", column: SortKey::Status, sort, class: "" }
                                    }
                                }
                                tbody {
                                    for build in shown.iter() {
                                        tr {
                                            key: "{build.id}",
                                            class: "hover cursor-pointer",
                                            // The row is the build: that is what
                                            // a build list is a list of.
                                            onclick: {
                                                let id = build.id;
                                                move |_| {
                                                    navigator().push(Route::Build { id });
                                                }
                                            },
                                            td {
                                                Link {
                                                    class: "font-mono",
                                                    to: Route::Build { id: build.id },
                                                    onclick: move |e: MouseEvent| e.stop_propagation(),
                                                    "#{build.id}"
                                                }
                                            }
                                            // The one cell that goes somewhere
                                            // else, so it gets its own hover
                                            // colour: the row highlight alone
                                            // would suggest the whole row shares
                                            // a single destination.
                                            td {
                                                class: "hover:bg-primary/20 transition-colors",
                                                title: "Open package",
                                                Link {
                                                    class: "font-mono",
                                                    to: Route::Package { pkgbase: build.pkg_name.clone() },
                                                    onclick: move |e: MouseEvent| e.stop_propagation(),
                                                    "{build.pkg_name}"
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
                        div { class: "text-sm opacity-60 pt-2", "{total} builds" }
                    }
                    },
                }
            }
        }
    }
}
