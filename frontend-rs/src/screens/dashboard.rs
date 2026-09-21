//! The landing page: how the instance is doing, at a glance.

use crate::format::{format_bytes, format_secs};
use crate::listing::ViewParams;
use crate::routes::Route;
use aurcache_client::{GraphDataPoint, ListStats};
use dioxus::prelude::*;
use dioxus_charts::LineChart;

#[component]
pub fn Dashboard() -> Element {
    let stats = use_resource(|| async move {
        crate::api::client()?
            .stats()
            .await
            .map_err(|e| e.to_string())
    });
    let graph = use_resource(|| async move {
        crate::api::client()?
            .graph()
            .await
            .map_err(|e| e.to_string())
    });

    // The landing page is somewhere to leave open; a slow tick keeps the
    // totals and the chart roughly current without a reload. No "busy" input —
    // nothing here is worth a fast poll.
    crate::poll::use_poll(stats, false);
    crate::poll::use_poll(graph, false);

    rsx! {
        div { class: "space-y-4",
            match &*stats.read_unchecked() {
                None => rsx! {
                    div { class: "flex justify-center p-8",
                        span { class: "loading loading-spinner loading-lg" }
                    }
                },
                Some(Err(e)) => rsx! {
                    div { class: "alert alert-error", span { "Could not load statistics: {e}" } }
                },
                Some(Ok(stats)) => rsx! {
                    StatTiles { stats: stats.clone() }
                },
            }

            div { class: "card bg-base-100 shadow-xl",
                div { class: "card-body",
                    div { class: "flex items-baseline gap-4 flex-wrap",
                h2 { class: "card-title text-base", "Builds per month" }
                div { class: "flex items-center gap-3 text-xs opacity-70",
                    span { class: "flex items-center gap-1",
                        span { class: "inline-block w-3 h-0.5 bg-primary" }
                        "started"
                    }
                    span { class: "flex items-center gap-1",
                        span { class: "inline-block w-3 h-0.5 bg-success" }
                        "succeeded"
                    }
                }
            }
                    match &*graph.read_unchecked() {
                        None => rsx! {
                            div { class: "flex justify-center p-8",
                                span { class: "loading loading-spinner loading-lg" }
                            }
                        },
                        Some(Err(e)) => rsx! {
                            div { class: "alert alert-error", span { "Could not load the graph: {e}" } }
                        },
                        Some(Ok(points)) => rsx! {
                            BuildsChart { points: points.clone() }
                        },
                    }
                }
            }
        }
    }
}

/// The share of finished builds that succeeded.
///
/// Over builds that *finished*: counting the ones still running as failures
/// would make a busy queue look like a broken server. `None` when nothing has
/// finished, because zero out of zero is not zero per cent.
fn success_rate(successful: u32, failed: u32) -> Option<f64> {
    // Added wide: the counters run over an unbounded history, so `u32 + u32`
    // would wrap at 2^32 builds into a wrong percentage.
    let finished = u64::from(successful) + u64::from(failed);
    (finished > 0).then(|| u64::from(successful) as f64 / finished as f64 * 100.0)
}

/// A rate as a percentage, or a dash.
fn format_rate(rate: Option<f64>) -> String {
    rate.map_or_else(|| "—".to_string(), |rate| format!("{rate:.0}%"))
}

/// The headline numbers.
#[component]
fn StatTiles(stats: ListStats) -> Element {
    // Added wide, like `success_rate` below: the behaviour on overflow should
    // not differ between two tiles of the same page.
    let all_packages = u64::from(stats.requested_packages) + u64::from(stats.dependency_packages);

    rsx! {
        div { class: "grid gap-4 grid-cols-2 lg:grid-cols-3 xl:grid-cols-5",
            StatTile {
                label: "Packages",
                value: "{stats.requested_packages}",
                // The requested count is the one people mean by "how many
                // packages do I have"; dependencies are along for the ride, so
                // they sit underneath rather than inflating the headline.
                note: "{all_packages} with dependencies",
                to: Route::Packages { view: ViewParams::default(), q: String::new() },
            }
            StatTile {
                label: "Builds",
                value: "{stats.total_builds}",
                note: "{stats.recent_builds} this week",
                to: Route::Builds { view: ViewParams::default(), q: String::new() },
            }
            StatTile {
                label: "Succeeded",
                value: format_rate(success_rate(stats.successful_builds, stats.failed_builds)),
                // A run of failures this week vanishes into a year of old
                // successes, which is exactly when it matters most.
                note: "{format_rate(success_rate(stats.recent_successful, stats.recent_failed))} this week",
            }
            StatTile {
                label: "Average build",
                value: format_secs(stats.avg_build_time),
            }
            StatTile {
                label: "Repository",
                value: format_bytes(stats.repo_size),
            }
        }
    }
}

/// One number, an optional second reading of it, and optionally a way to go
/// and look at what it counts.
#[component]
fn StatTile(label: String, value: String, note: Option<String>, to: Option<Route>) -> Element {
    let body = rsx! {
        div { class: "card-body p-4",
            div { class: "text-xs opacity-60", "{label}" }
            div { class: "text-2xl font-semibold", "{value}" }
            // Reserved whether or not there is one, so tiles with a note and
            // tiles without still line up in the row.
            div { class: "text-xs opacity-50 min-h-4",
                {note.unwrap_or_default()}
            }
        }
    };

    match to {
        Some(to) => rsx! {
            Link { class: "card bg-base-100 shadow-xl hover:bg-base-200 transition-colors", to, {body} }
        },
        None => rsx! {
            div { class: "card bg-base-100 shadow-xl", {body} }
        },
    }
}

/// Builds per month over the last year.
///
/// Static: `dioxus-charts` renders SVG and takes CSS classes, but has no hover
/// or event props, so there is no tooltip. Adding that upstream is its own
/// piece of work — see the note on the crate in `Cargo.toml`.
#[component]
fn BuildsChart(points: Vec<GraphDataPoint>) -> Element {
    if points.is_empty() {
        return rsx! {
            div { class: "alert", span { "No builds in the last year." } }
        };
    }

    // The server returns newest first; a time axis reads the other way.
    let mut points = points;
    points.sort_by_key(|p| (p.year, p.month));

    let labels: Vec<String> = points.iter().map(|p| month_label(p.month)).collect();
    // Two lines rather than a stack, which the crate cannot draw: total on top,
    // successes below it. The gap between them is the failures, which is what
    // anyone is actually looking for — and unlike a stack, both lines are read
    // against the same axis rather than one against a moving baseline.
    let started: Vec<f32> = points.iter().map(|p| p.count as f32).collect();
    let succeeded: Vec<f32> = points.iter().map(|p| p.successful as f32).collect();

    // The chart scales everything — text included — to its viewBox, and its
    // width defaults to 100%. Left alone it grows to whatever the card allows
    // and the axis labels come out enormous. A wide, short viewBox in a fixed
    // height gives a chart shaped like a chart.
    rsx! {
        // `chart-box` is what stops the chart growing with the window; see the
        // rule in index.html. Without it the fixed height below is advice the
        // svg is free to ignore, and does.
        div { class: "chart-box h-64",
            LineChart {
                width: "100%",
                height: "100%",
                viewbox_width: 900,
                viewbox_height: 260,
                padding_top: 20,
                padding_left: 50,
                padding_right: 30,
                padding_bottom: 45,
                series: vec![started, succeeded],
                labels,
                // Named explicitly rather than relying on the crate's default,
                // which is not the class its own docs imply: each series is
                // grouped as `{class_line}-{n}`, and the CSS below keys on it.
                class_line: "series",
                // Its own series labels are drawn past the last point, where
                // the viewBox clips them. The legend above the chart says the
                // same thing with room for it.
                show_line_labels: false,
                show_grid_ticks: true,
                show_dotted_grid: false,
                // Counts start at zero, and letting the axis float makes a
                // quiet month look like a collapse.
                lowest: 0.0,
                // Whole builds; halves on the axis are meaningless.
                max_ticks: 5,
                // The crate hardcodes a stroke as an SVG presentation
                // attribute, which any CSS rule outranks — so a class is
                // enough to put the lines back under the theme. It puts each
                // series in a `dx-chart-line-{n}` group, and the rules that
                // colour them separately live in index.html, since Tailwind
                // cannot express "a path inside that group".
                class_line_path: "chart-line",
                class_line_dot: "chart-line",
            }
        }
    }
}

/// A month number as a short name.
///
/// Names rather than numbers because the axis is read at a glance, and `01`
/// against `12` is harder to place than `Jan` against `Dec`. English only, in
/// line with the rest of the UI.
fn month_label(month: i32) -> String {
    const NAMES: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    usize::try_from(month)
        .ok()
        .and_then(|m| m.checked_sub(1))
        .and_then(|m| NAMES.get(m))
        .map_or_else(|| month.to_string(), ToString::to_string)
}

#[cfg(test)]
mod tests {
    use super::{StatTiles, StatTilesProps, format_rate, month_label, success_rate};
    use aurcache_client::ListStats;
    use dioxus::prelude::*;

    #[test]
    fn months_read_as_names() {
        assert_eq!(month_label(1), "Jan");
        assert_eq!(month_label(12), "Dec");
        // Nothing the server should send, but a bad value must not panic or
        // silently show the wrong month.
        assert_eq!(month_label(0), "0");
        assert_eq!(month_label(13), "13");
        assert_eq!(month_label(-1), "-1");
    }

    /// The two linked tiles are absent from this markup: a `Link` renders
    /// nothing without a `Router` above it, and standing one up here would
    /// need the network the dashboard fetches from. That they appear at all is
    /// the browser test's job; what is worth testing here is the arithmetic.
    fn tiles(successful: u32, failed: u32, total: u32) -> String {
        let mut dom = VirtualDom::new_with_props(
            StatTiles,
            StatTilesProps {
                stats: ListStats {
                    total_builds: total,
                    successful_builds: successful,
                    failed_builds: failed,
                    // A deliberately worse week than the lifetime figures.
                    recent_builds: 4,
                    recent_successful: 1,
                    recent_failed: 3,
                    avg_build_time: 90,
                    repo_size: 3 * 1024 * 1024 * 1024,
                    requested_packages: 12,
                    dependency_packages: 4,
                    total_build_trend: 0.0,
                    avg_build_time_trend: 0.0,
                },
            },
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    /// Over finished builds, not all of them. Counting the ones still running
    /// as failures would make a busy queue look like a broken server.
    #[test]
    fn the_success_rate_ignores_builds_still_running() {
        // Nine done, one still going: 8 of 9, not 8 of 10.
        assert_eq!(format_rate(success_rate(8, 1)), "89%");
    }

    /// A fresh instance has finished nothing; zero out of zero is not 0%.
    #[test]
    fn nothing_finished_means_no_rate_rather_than_zero() {
        assert_eq!(success_rate(0, 0), None);
        assert_eq!(format_rate(None), "—");
    }

    /// The lifetime rate and the weekly one are computed the same way but from
    /// different counts, and the tile shows both. A year of old successes
    /// hiding a bad week is the case this exists for.
    #[test]
    fn a_bad_week_shows_even_when_the_lifetime_rate_is_good() {
        let html = tiles(8, 1, 10);
        assert!(html.contains("89%"), "lifetime rate: {html}");
        // The fixture below gives the recent counts as 1 succeeded, 3 failed.
        assert!(html.contains("25% this week"), "weekly rate: {html}");
    }

    #[test]
    fn the_tiles_carry_their_numbers() {
        let html = tiles(8, 1, 10);
        assert!(html.contains("1m 30s"), "average build: {html}");
        assert!(html.contains("3.0 GiB"), "repository size: {html}");
    }
}
