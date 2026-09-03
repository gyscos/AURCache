//! A build's log output.

use crate::api::api_base;
use aurcache_client::AurCacheClient;
use aurcache_common::build_state::BuildState;
use dioxus::prelude::*;

/// The screen behind `/package/:pkgbase/build/:number`.
#[component]
pub fn Build(pkgbase: String, number: i32) -> Element {
    // `use_reactive` so the fetch follows the route. Navigating between two
    // packages reuses this component -- same route, different parameter -- and
    // a resource whose closure captured the old name simply never re-runs: the
    // URL changes, no request is made, and the previous package stays on
    // screen looking like the one that was clicked.
    let build = use_resource(use_reactive(
        &(pkgbase.clone(), number),
        |(pkgbase, number)| async move {
            crate::api::client()?
                .get_build(&pkgbase, number)
                .await
                .map_err(|e| e.to_string())
        },
    ));

    // Fetched from the build's package so this page carries the same header
    // as every other package-scoped page, with the trail in the same place.
    let package = use_resource(use_reactive(&pkgbase, |pkgbase| async move {
        crate::api::client()?
            .get_package(&pkgbase)
            .await
            .map_err(|e| e.to_string())
            .map(Some)
    }));

    rsx! {
        // `h-full` so the log below can flex into what the header leaves,
        // rather than guessing a fraction of the viewport and overshooting it.
        div { class: "space-y-4 h-full flex flex-col min-h-0",
            match (&*package.read_unchecked(), &*build.read_unchecked()) {
                (Some(Ok(Some(pkg))), Some(Ok(build))) => rsx! {
                    crate::screens::PackageHeader {
                        pkg: pkg.clone(),
                        trail: vec![
                            (
                                "Builds".to_string(),
                                Some(crate::routes::Route::PackageBuilds {
                                    pkgbase: build.pkg_name.clone(),
                                }),
                            ),
                            (build.number.to_string(), None),
                        ],
                    }
                },
                // The log is what this page is for, so a failed lookup costs
                // the header rather than the page.
                _ => rsx! {},
            }
            BuildLog { pkgbase, number }
        }
    }
}

// ---------------------------------------------------------------------------
// Build log
//
// The interesting screen: output arrives while the build runs. The API is
// incremental rather than streaming — `?startline=N` returns everything from
// line N on — so this polls, appends, and stops once the build reaches a
// terminal state. That is the same contract the Dart component uses; the
// point here is to see what it costs to express in Dioxus.
// ---------------------------------------------------------------------------

/// How often to ask for more output while a build is running.
const POLL_INTERVAL_MS: u32 = 3_000;

#[component]
pub fn BuildLog(pkgbase: String, number: i32) -> Element {
    // Cloned for the polling future, which outlives this render.
    let polled = pkgbase.clone();
    // One string, one text node. Per-line elements would only earn their keep
    // for per-line features — ANSI colour, line numbers, deep links — and a
    // build emits thousands of lines, so the browser would lay out thousands
    // of elements and the VirtualDom would walk them on every poll.
    let mut log = use_signal(String::new);
    // Two counters, because they answer different questions. `byte_offset` is
    // what the API takes: the server seeks to it in the log file, so a poll
    // costs what it reads rather than the size of the whole log. `line_count`
    // is only for display, counted from the chunks as they arrive so the server
    // never has to think in lines -- which is what let the transport switch to
    // bytes without losing the "N lines" readout.
    let mut byte_offset = use_signal(|| 0u64);
    let mut line_count = use_signal(|| 0i32);
    let mut finished = use_signal(|| false);
    let mut error = use_signal(|| Option::<String>::None);
    // "Follow" pins the view to the bottom as output arrives; switching it off
    // is what lets someone read back through a long log while it is still
    // being written.
    let mut following = use_signal(|| true);

    use_future(move || {
        let pkgbase = polled.clone();
        async move {
            let client = match AurCacheClient::new(api_base(), None) {
                Ok(c) => c,
                Err(e) => {
                    error.set(Some(e.to_string()));
                    return;
                }
            };

            loop {
                // Ask only for what we do not already have.
                let have = byte_offset();
                match client.build_output(&pkgbase, number, Some(have)).await {
                    Ok(chunk) if !chunk.is_empty() => {
                        let added = chunk.lines().count() as i32;
                        byte_offset += chunk.len() as u64;
                        log.with_mut(|text| {
                            if !text.is_empty() && !text.ends_with('\n') {
                                text.push('\n');
                            }
                            text.push_str(&chunk);
                        });
                        line_count += added;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        error.set(Some(e.to_string()));
                        return;
                    }
                }

                // Stop polling once the build reaches a terminal state, but only
                // after the fetch above, so the last lines are never missed.
                if let Ok(build) = client.get_build(&pkgbase, number).await
                    && !matches!(BuildState::from_i32(build.status), Some(BuildState::Active))
                {
                    finished.set(true);
                    return;
                }

                gloo_timers::future::TimeoutFuture::new(POLL_INTERVAL_MS).await;
            }
        }
    });

    // Keep the view pinned to the newest output while "Follow" is on.
    //
    // This has to be an effect, not a call from the poll loop: appending to
    // `log` only marks the component dirty, and the re-render that actually puts
    // the new lines in the DOM happens afterwards. Scrolling before that measured
    // the height of text that was not on screen yet and stopped short of the
    // bottom -- which is exactly the "jumps down once, then never again" the poll
    // loop produced. An effect runs after the DOM is patched, so `scrollHeight`
    // is finally correct. It re-runs when `line_count` grows (a new chunk landed)
    // or when `following` flips back on (re-pin immediately).
    use_effect(move || {
        line_count();
        if following() {
            scroll_log_to_bottom();
        }
    });

    rsx! {
        div { class: "card bg-base-100 shadow-xl flex-1 min-h-0",
            div { class: "card-body flex flex-col min-h-0",
                div { class: "flex items-center gap-3",
                    if finished() {
                        span { class: "badge badge-ghost badge-sm", "finished" }
                    } else {
                        span { class: "badge badge-info badge-sm gap-1",
                            span { class: "loading loading-spinner loading-xs" }
                            "running"
                        }
                    }
                    div { class: "flex-1" }
                    label { class: "label cursor-pointer gap-2",
                        span { class: "label-text text-sm", "Follow" }
                        input {
                            r#type: "checkbox",
                            class: "toggle toggle-sm toggle-primary",
                            checked: following(),
                            // The effect below does the scrolling: it reads
                            // `following`, so turning this on re-pins to the
                            // bottom on the next render.
                            oninput: move |e| following.set(e.value() == "true"),
                        }
                    }
                }

                if let Some(e) = error() {
                    div { class: "alert alert-error", span { "{e}" } }
                }

                pre {
                    id: "build-log",
                    // `flex-1 min-h-0` rather than a share of the viewport:
                    // the log takes whatever is left after the header and the
                    // footer, so the card fills the window exactly and this is
                    // the only thing that scrolls. `min-h-0` because a flex
                    // child will not shrink below its content without it, which
                    // is what pushed the page past the window before.
                    class: "bg-neutral text-neutral-content rounded-box p-4 text-xs \
                            flex-1 min-h-0 overflow-auto whitespace-pre-wrap font-mono",
                    if line_count() == 0 {
                        // A finished build with nothing to show has no log at
                        // all -- it never wrote one, or it has been removed --
                        // which is a different statement from a running build
                        // that has not written its first line yet.
                        if finished() {
                            span { class: "opacity-60", "no log for this build" }
                        } else {
                            span { class: "opacity-60", "waiting for output…" }
                        }
                    } else {
                        "{log}"
                    }
                }
                div { class: "text-sm opacity-60", "{line_count} lines" }
            }
        }
    }
}

/// Pin the log view to the newest output.
///
/// Done through the DOM rather than a Dioxus abstraction because the element
/// scrolls itself; there is no reactive value to bind to.
fn scroll_log_to_bottom() {
    if let Some(el) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id("build-log"))
    {
        el.set_scroll_top(el.scroll_height());
    }
}
