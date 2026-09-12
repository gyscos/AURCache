//! A build's log output.

use crate::api::api_base;
use crate::dates::AbsoluteDate;
use crate::format::{format_duration, now_secs};
use crate::listing::ViewParams;
use crate::routes::Route;
use crate::shell::{CheckIcon, CopyIcon};
use crate::status::BuildStatusBadge;
use aurcache_client::AurCacheClient;
use aurcache_common::build_state::BuildState;
use dioxus::prelude::*;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;

/// Whether the build has stopped changing, so the poll loop can stop with it.
///
/// Only `Successful` and `Failed` settle. Testing for "not `Active`" instead
/// counted *enqueued* and *waiting for deps* as over, which showed a freshly
/// queued build as finished and stopped the page updating when it later
/// started.
///
/// An unrecognised state from a newer server does not settle: being wrong that
/// way costs one poll per interval, while being wrong the other way is the bug
/// above.
fn settled(status: i32) -> bool {
    BuildState::from_i32(status).is_some_and(|s| !s.is_in_progress())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_terminal_states_settle() {
        assert!(settled(BuildState::Successful.as_i32()));
        assert!(settled(BuildState::Failed.as_i32()));
        assert!(!settled(BuildState::Active.as_i32()));
        // The regression: queued is not finished.
        assert!(!settled(BuildState::Enqueued.as_i32()));
        assert!(!settled(BuildState::WaitingForDeps.as_i32()));
        // A state this build of the UI has never heard of keeps it polling.
        assert!(!settled(99));
    }

    /// Signals only have a home inside a running component, so the button is
    /// rendered through a harness rather than handed `Signal` props from a
    /// test function.
    #[component]
    fn CopyButtonHarness(log: String, copied: bool) -> Element {
        let log_signal = use_signal(move || log);
        let copied_signal = use_signal(move || copied);
        let error = use_signal(|| None::<String>);
        rsx! { LogCopyButton { log: log_signal, copied: copied_signal, error } }
    }

    fn render_copy_button(log: &str, copied: bool) -> String {
        let mut dom = VirtualDom::new_with_props(
            CopyButtonHarness,
            CopyButtonHarnessProps {
                log: log.to_string(),
                copied,
            },
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    thread_local! {
        /// Lets a test reach the signal a rendered harness owns. A signal has
        /// no life outside a component, so this is the only way to change one
        /// between renders.
        static LATE_LOG: std::cell::RefCell<Option<Signal<String>>> =
            const { std::cell::RefCell::new(None) };
    }

    #[component]
    fn LateLogHarness() -> Element {
        let log = use_signal(String::new);
        LATE_LOG.with(|cell| *cell.borrow_mut() = Some(log));
        let copied = use_signal(|| false);
        let error = use_signal(|| None::<String>);
        rsx! { LogCopyButton { log, copied, error } }
    }

    /// A log arrives after the first render — always, since the page fetches
    /// it — and the button has to arrive with it. Component props are memoized
    /// on signals whose identity never changes, so a button that only `peek`s
    /// at the log is never re-run: it stayed hidden for the whole life of the
    /// page, on every build.
    #[test]
    fn the_copy_button_appears_when_the_log_arrives() {
        let mut dom = VirtualDom::new(LateLogHarness);
        dom.rebuild_in_place();
        assert!(
            !dioxus_ssr::render(&dom).contains("Copy"),
            "nothing to copy before the log arrives"
        );

        let mut log = LATE_LOG.with(|cell| *cell.borrow().as_ref().unwrap());
        dom.in_runtime(|| log.set("make: *** [all] Error 1".to_string()));
        dom.render_immediate(&mut dioxus_core::NoOpMutations);

        let html = dioxus_ssr::render(&dom);
        assert!(
            html.contains("Copy"),
            "the button must arrive with the log: {html:?}"
        );
    }

    /// Nothing to copy: a build that has not written its first line yet, or a
    /// finished one whose log has been removed, is not offered a copy button.
    #[test]
    fn the_copy_button_appears_only_once_there_is_a_log() {
        assert!(
            !render_copy_button("", false).contains("Copy"),
            "an empty log should show no copy button"
        );
    }

    /// The checkmark is the promise that the paste will land, so it must not
    /// hang around after the button has gone back to work.
    #[test]
    fn the_copy_button_shows_a_checkmark_after_copying() {
        let html = render_copy_button("make: nothing to be done for `all'.", true);
        assert!(html.contains("Copied"), "{html}");
        assert!(
            !html.contains(">Copy<"),
            "the label should flip to Copied: {html}"
        );
    }
}

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
    // The build's real state, not just "is it over". "Not building" also covers
    // *enqueued* and *waiting for deps*, and collapsing those into a boolean is
    // what told someone their freshly queued build had already finished.
    let mut status = use_signal(|| None::<i32>);
    // Filled from the same poll that decides when the log stops, so a build
    // claimed while this page is open names its worker without a reload.
    let mut worker_name = use_signal(|| None::<String>);
    // When it started and, once it has, when it stopped. A build that is not
    // even queued has no start, and an ended build always has an end.
    let mut start_time = use_signal(|| None::<i64>);
    let mut end_time = use_signal(|| None::<i64>);
    let mut error = use_signal(|| Option::<String>::None);
    // True for a couple of seconds after a successful copy, so the button
    // swaps its icon and label to say the log is now on the clipboard.
    let copied = use_signal(|| false);
    // "Follow" pins the view to the bottom as output arrives; switching it off
    // is what lets someone read back through a long log while it is still
    // being written.
    let mut following = use_signal(|| true);
    use_future(move || {
        let pkgbase = pkgbase.clone();
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
                        if error.peek().is_some() {
                            error.set(None);
                        }
                    }
                    Ok(_) => {
                        // A successful poll clears a previous failure: the
                        // banner should describe now, not the worst moment so
                        // far.
                        if error.peek().is_some() {
                            error.set(None);
                        }
                    }
                    Err(e) => {
                        // Report and keep polling. Returning here abandoned the
                        // log for the life of the page, so one timeout on a
                        // slow connection meant a running build stopped
                        // updating until it was reloaded by hand -- and the
                        // first fetch is the most likely to time out, being the
                        // whole log at once.
                        error.set(Some(e.to_string()));
                    }
                }

                // Stop polling once the build reaches a terminal state, but only
                // after the fetch above, so the last lines are never missed.
                if let Ok(build) = client.get_build(&pkgbase, number).await {
                    worker_name.set(build.worker_name.clone());
                    status.set(Some(build.status));
                    start_time.set(build.start_time);
                    end_time.set(build.end_time);
                    // Only a *settled* build stops the loop. `is_in_progress`
                    // answers exactly this and keeps the queued states on the
                    // right side of it; testing for `Active` alone treated an
                    // enqueued build as over, so the page announced "finished"
                    // and -- because this returns -- never updated again when
                    // the build actually started.
                    //
                    // An unrecognised state from a newer server counts as in
                    // progress: being wrong that way costs one poll per
                    // interval, while being wrong the other way is this bug.
                    if settled(build.status) {
                        finished.set(true);
                        return;
                    }
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
                    // The same badge the lists use, so a state means the same
                    // thing everywhere and a new one added server-side breaks
                    // the exhaustive match instead of rendering as a guess.
                    if let Some(state) = status() {
                        span { class: "flex items-center gap-2",
                            BuildStatusBadge { status: state }
                            if !finished() {
                                span { class: "loading loading-spinner loading-xs opacity-60" }
                            }
                        }
                    } else {
                        span { class: "badge badge-ghost badge-sm", "…" }
                    }
                    if let Some(worker) = worker_name() {
                        // Beside the state, because "what is it doing" and
                        // "where" are one question when a build misbehaves.
                        Link {
                            class: "font-mono text-sm opacity-70 hover:underline",
                            to: Route::Builds { view: ViewParams::default(), q: worker.clone() },
                            title: "Show this worker's builds",
                            "{worker}"
                        }
                    }
                    if let Some(start) = start_time() {
                        // When it started and what it has used since. A running
                        // build's duration grows as this page watches, so it is
                        // measured to now; an ended one reports its total.
                        span { class: "flex items-center gap-2 text-sm opacity-70",
                            span { class: "whitespace-nowrap", "Started " }
                            AbsoluteDate { ts: Some(start) }
                            if end_time().is_none() {
                                {format!("· {} so far", format_duration(Some(start), Some(now_secs())))}
                            } else {
                                {format!("· took {}", format_duration(Some(start), end_time()))}
                            }
                        }
                    }
                    div { class: "flex-1" }
                    LogCopyButton { log, copied, error }
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

/// Copy the whole log to the clipboard.
///
/// Split from `BuildLog` so the button's presence and its "copied" state are
/// render-testable: the interesting browser part — asking the browser for its
/// clipboard — cannot be, and lives in the onclick. The button only appears
/// once there is something to copy, and flips to a checkmark for a couple of
/// seconds after a successful copy so someone pasting knows it landed.
#[component]
fn LogCopyButton(
    log: Signal<String>,
    mut copied: Signal<bool>,
    mut error: Signal<Option<String>>,
) -> Element {
    // `read`, not `peek`: props are memoized on signals that never change
    // identity, so a component that only peeks is never re-run. Peeking here
    // left the button hidden for the whole life of a page whose log arrived
    // after the first render — which is every page.
    if log.read().is_empty() {
        return rsx! {};
    }
    rsx! {
        button {
            class: "btn btn-ghost btn-xs gap-1.5",
            title: "Copy the whole build log to the clipboard",
            disabled: copied(),
            onclick: move |_| async move {
                // The whole log is one string in memory, so nothing extra is
                // fetched — the button copies everything written so far.
                let text = log();
                let Some(clipboard) = web_sys::window()
                    .map(|w| w.navigator().clipboard())
                    // Outside a secure context `navigator.clipboard` is
                    // undefined, and web-sys hands that straight back as a
                    // `Clipboard` rather than `None`. Calling `write_text` on
                    // it throws through the wasm boundary, which takes the page
                    // down instead of showing the message below — and http on a
                    // LAN address is an ordinary way to reach this UI.
                    .filter(|c| !AsRef::<JsValue>::as_ref(c).is_undefined())
                else {
                    error.set(Some(
                        "Clipboard is unavailable on this connection \
                         (it needs a secure context, like https or localhost)."
                            .to_string(),
                    ));
                    return;
                };
                match JsFuture::from(clipboard.write_text(&text)).await {
                    Ok(_) => {
                        copied.set(true);
                        // Let the checkmark say its piece, then give the button
                        // back its job.
                        gloo_timers::future::TimeoutFuture::new(2000).await;
                        copied.set(false);
                    }
                    Err(_) => error.set(Some(
                        "Could not copy the build log to the clipboard.".to_string(),
                    )),
                }
            },
            if copied() {
                CheckIcon {}
                span { "Copied" }
            } else {
                CopyIcon {}
                span { "Copy" }
            }
        }
    }
}
