//! The builds list.

use crate::api::client;
use crate::routes::Route;
use crate::status::BuildStatusBadge;
use aurcache_client::Build;
use dioxus::prelude::*;

/// Columns that only appear once there is room for them, matching the Dart
/// table, which drops the same ones below 700px.
const WIDE_ONLY: &str = "hidden md:table-cell";

/// Build timestamps are Unix **seconds**, not milliseconds — `stats.rs`
/// compares them against `strftime('%s', 'now')`.
fn now_secs() -> i64 {
    (js_sys::Date::now() / 1000.0) as i64
}

/// How long a build took, from its two timestamps.
///
/// `None` for either end means the build has not finished (or never started),
/// which is not a zero-length build — so it reads as unknown rather than `0s`.
fn format_duration(start: Option<i64>, end: Option<i64>) -> String {
    let (Some(start), Some(end)) = (start, end) else {
        return "—".to_string();
    };
    let secs = end - start;
    if secs < 0 {
        // A worker with a skewed clock can report an end before the start.
        // Showing a negative duration is worse than admitting it is unknown.
        return "—".to_string();
    }
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m {}s", secs / 60, secs % 60),
        _ => format!("{}h {}m", secs / 3600, (secs % 3600) / 60),
    }
}

/// How long ago something happened, relative to `now`.
///
/// Relative rather than absolute because it avoids a timezone and locale
/// story, and "3h ago" is what you actually want when scanning a build list.
fn format_age(ts: Option<i64>, now: i64) -> String {
    let Some(ts) = ts else {
        return "—".to_string();
    };
    let secs = now - ts;
    if secs < 0 {
        // Clock skew again: a build stamped in the future.
        return "just now".to_string();
    }
    match secs {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
    }
}

async fn load_builds() -> Result<Vec<Build>, String> {
    client()?
        .list_builds(None, Some(100), None)
        .await
        .map_err(|e| e.to_string())
}

#[component]
pub fn Builds() -> Element {
    let builds = use_resource(load_builds);
    // Read once per render rather than per row, so every age on the page is
    // measured from the same instant.
    let now = now_secs();

    rsx! {
        div { class: "card bg-base-100 shadow-xl",
            div { class: "card-body",
                h2 { class: "card-title", "Builds" }

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
                    Some(Ok(list)) => rsx! {
                        div { class: "overflow-x-auto",
                            table { class: "table table-zebra",
                                thead {
                                    tr {
                                        th { "Build" }
                                        th { "Package" }
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
                                            td {
                                                Link {
                                                    class: "link font-medium",
                                                    to: Route::Package { pkgbase: build.pkg_name.clone() },
                                                    "{build.pkg_name}"
                                                }
                                            }
                                            td { class: "{WIDE_ONLY} font-mono text-sm", "{build.version}" }
                                            td { class: "{WIDE_ONLY} text-sm opacity-70",
                                                {format_age(build.start_time, now)}
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_in_the_largest_useful_unit() {
        assert_eq!(format_duration(Some(0), Some(43)), "43s");
        assert_eq!(format_duration(Some(0), Some(123)), "2m 3s");
        assert_eq!(format_duration(Some(100), Some(100)), "0s");
        assert_eq!(format_duration(Some(0), Some(3600)), "1h 0m");
        assert_eq!(format_duration(Some(0), Some(7860)), "2h 11m");
    }

    /// A running build has a start but no end. That is unknown, not zero — a
    /// build in progress must not read as having taken no time.
    #[test]
    fn an_unfinished_build_has_no_duration() {
        assert_eq!(format_duration(Some(100), None), "—");
        assert_eq!(format_duration(None, None), "—");
        assert_eq!(format_duration(None, Some(100)), "—");
    }

    /// Builds are timed by whichever worker ran them, so the two ends can come
    /// from different clocks. A negative duration is nonsense to display.
    #[test]
    fn a_backwards_timestamp_pair_is_not_shown_as_negative() {
        assert_eq!(format_duration(Some(500), Some(100)), "—");
    }

    #[test]
    fn ages_read_in_the_largest_useful_unit() {
        let now = 1_000_000;
        assert_eq!(format_age(Some(now), now), "just now");
        assert_eq!(format_age(Some(now - 59), now), "just now");
        assert_eq!(format_age(Some(now - 60), now), "1m ago");
        assert_eq!(format_age(Some(now - 3599), now), "59m ago");
        assert_eq!(format_age(Some(now - 3600), now), "1h ago");
        assert_eq!(format_age(Some(now - 86400), now), "1d ago");
        assert_eq!(format_age(Some(now - 86400 * 30), now), "30d ago");
    }

    #[test]
    fn a_build_with_no_start_time_has_no_age() {
        assert_eq!(format_age(None, 1_000_000), "—");
    }

    /// A worker whose clock runs ahead stamps a build in the future. "in -3m"
    /// would be worse than rounding to the present.
    #[test]
    fn a_future_timestamp_reads_as_just_now() {
        assert_eq!(format_age(Some(2_000_000), 1_000_000), "just now");
    }
}
