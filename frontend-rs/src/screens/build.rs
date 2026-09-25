//! A build's log output.

use crate::api::LoadError;
use crate::dates::AbsoluteDate;
use crate::format::{format_bytes, format_duration, now_secs};
use crate::listing::ViewParams;
use crate::log_tail::{
    append_capped, catch_up_offset, coarse_pointer, drop_leading_partial_line, log_tail_cap,
};
use crate::routes::Route;
use crate::shell::{CheckIcon, CopyIcon, DownloadIcon, WarnIcon};
use crate::status::BuildStatusBadge;
use aurcache_common::api::build_log::align;
use aurcache_common::api::builds::DiskUsage;
use aurcache_common::build_state::BuildState;
use dioxus::prelude::*;

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

/// Whether the build page is still waiting on something, so its header keeps
/// re-fetching briskly rather than falling back to the idle cadence.
///
/// Either side can be the one that is behind: the build's own state comes
/// from the log's poll loop, while the package badge up top only moves when
/// its lookup re-runs — without this a finished build read "successful" below
/// a header that still said "building". Nothing fetched yet counts as idle,
/// the same as elsewhere: the initial load is already in flight.
fn build_page_busy(build_status: Option<i32>, package_status: Option<i32>) -> bool {
    [build_status, package_status]
        .into_iter()
        .flatten()
        .any(|status| !settled(status))
}

/// What the log area says while it has no text, given whether the poll loop
/// has finished (its final page is in) and the build's state.
///
/// Three different statements. A finished build with nothing to show has no
/// log at all -- it never wrote one, or it has been removed. A settled build
/// whose output is still being fetched has a log that has not arrived yet,
/// which is the one to get right: saying "no log" there is false for as long
/// as a large log takes to download. And a build still going may not have
/// written its first line.
fn empty_log_placeholder(finished: bool, status: Option<i32>) -> &'static str {
    if finished {
        "no log for this build"
    } else if status.is_some_and(settled) {
        "loading log…"
    } else {
        "waiting for output…"
    }
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

    /// The header stays live while either side is still moving: the bug was a
    /// finished build reading "successful" under a package badge that still
    /// said "building", because only the log's loop kept polling.
    #[test]
    fn the_header_stays_busy_while_either_side_is_still_moving() {
        let active = Some(BuildState::Active.as_i32());
        let successful = Some(BuildState::Successful.as_i32());
        assert!(build_page_busy(active, active));
        assert!(build_page_busy(successful, active));
        assert!(build_page_busy(active, successful));
        assert!(!build_page_busy(successful, successful));
        // Nothing fetched yet: the initial load is already in flight, so
        // there is nothing extra to hurry along.
        assert!(!build_page_busy(None, None));
    }

    #[test]
    fn empty_log_placeholder_tells_loading_from_missing() {
        let failed = Some(BuildState::Failed.as_i32());
        // The regression: an old build whose log is still downloading is not
        // one without a log.
        assert_eq!(empty_log_placeholder(false, failed), "loading log…");
        assert_eq!(empty_log_placeholder(true, failed), "no log for this build");
        assert_eq!(
            empty_log_placeholder(false, Some(BuildState::Active.as_i32())),
            "waiting for output…"
        );
        // Before the first status poll answers, nothing is known yet.
        assert_eq!(empty_log_placeholder(false, None), "waiting for output…");
    }

    /// Signals only have a home inside a running component, so the button is
    /// rendered through a harness rather than handed `Signal` props from a
    /// test function.
    #[component]
    fn CopyButtonHarness(log: String, copied: bool) -> Element {
        let log_signal = use_signal(move || log);
        let copied_signal = use_signal(move || copied);
        let tail = use_signal(|| false);
        let error = use_signal(|| None::<String>);
        rsx! {
            LogCopyButton {
                pkgbase: "hello".to_string(),
                number: 3,
                log: log_signal,
                tail,
                cap: 4 << 20,
                copied: copied_signal,
                error,
            }
        }
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
        let tail = use_signal(|| false);
        let error = use_signal(|| None::<String>);
        rsx! {
            LogCopyButton {
                pkgbase: "hello".to_string(),
                number: 3,
                log,
                tail,
                cap: 4 << 20,
                copied,
                error,
            }
        }
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

    /// Once the window is a true tail, Copy stops pretending —
    /// it labels itself, warns, and the Download link is offered beside it.
    #[component]
    fn TailButtonHarness() -> Element {
        let log = use_signal(|| "line one\nline two\nline three\n".to_string());
        let tail = use_signal(|| true);
        let copied = use_signal(|| false);
        let error = use_signal(|| None::<String>);
        rsx! {
            LogCopyButton {
                pkgbase: "hello".to_string(),
                number: 3,
                log,
                tail,
                cap: 4 << 20,
                copied,
                error,
            }
        }
    }

    #[test]
    fn the_copy_button_says_when_it_is_a_tail_and_offers_download() {
        let mut dom = VirtualDom::new(TailButtonHarness);
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains("Copy tail"), "{html}");
        assert!(
            html.contains("Copying the shown ~4.0 MiB tail, not the full log"),
            "the tooltip should send whole-log users to Download: {html}"
        );
        // The Download anchor reaches the streaming route and names the file
        // it will save.
        assert!(
            html.contains("/package/hello/build/3/output/download"),
            "the anchor should point at the download route: {html}"
        );
        assert!(html.contains("download=\"hello-3.log\""), "{html}");
        // Warning marker on the tail's copy button.
        assert!(
            html.contains(
                "m21.73 18-8-14a2 2 0 0 0-3.48 0l-8 14A2 2 0 0 0 4 21h16a2 2 0 0 0 1.73-3"
            ),
            "a tail should carry the `!` warning: {html}"
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
    let mut build = use_resource(use_reactive(
        &(pkgbase.clone(), number),
        |(pkgbase, number)| async move {
            Ok::<_, LoadError>(crate::api::client()?.get_build(&pkgbase, number).await?)
        },
    ));

    // Fetched from the build's package so this page carries the same header
    // as every other package-scoped page, with the trail in the same place.
    let mut package = use_resource(use_reactive(&pkgbase, |pkgbase| async move {
        Ok::<_, LoadError>(Some(crate::api::client()?.get_package(&pkgbase).await?))
    }));

    // A build that is not here leaves for the closest thing that is: its
    // package's builds, or -- when the package is gone too -- the add page with
    // the name searched. Waits for both answers, since which it is depends on
    // the package.
    let notice = crate::notice::use_notice();
    use_effect(use_reactive(
        &(pkgbase.clone(), number),
        move |(pkgbase, number)| {
            if !matches!(&*build.read(), Some(Err(LoadError::NotFound))) {
                return;
            }
            match &*package.read() {
                Some(Err(LoadError::NotFound)) => crate::notice::redirect(
                    notice,
                    Route::PackageAdd { q: pkgbase.clone() },
                    crate::notice::Level::Info,
                    format!(
                        "No package called {pkgbase} is tracked here, so there is no build \
                     #{number} of it. Search the AUR to add it."
                    ),
                ),
                Some(Ok(_)) => crate::notice::redirect(
                    notice,
                    Route::PackageBuilds {
                        pkgbase: pkgbase.clone(),
                    },
                    crate::notice::Level::Error,
                    format!("{pkgbase} has no build #{number}."),
                ),
                // Still asking, or the package lookup itself failed: nowhere
                // better to go.
                _ => {}
            }
        },
    ));

    // The log below polls the build on its own loop, so without this the
    // package badge up top froze at whatever the first lookup returned while
    // the build's own badge moved on to "successful".
    let build_status = match &*build.read_unchecked() {
        Some(Ok(b)) => Some(b.status),
        _ => None,
    };
    let package_status = match &*package.read_unchecked() {
        Some(Ok(Some(p))) => Some(p.status),
        _ => None,
    };
    let busy = build_page_busy(build_status, package_status);
    crate::poll::use_poll(build, busy);
    crate::poll::use_poll(package, busy);

    rsx! {
        // `h-full` so the log below can flex into what the header leaves,
        // rather than guessing a fraction of the viewport and overshooting it.
        div { class: "space-y-4 h-full flex flex-col min-h-0",
            match (&*package.read_unchecked(), &*build.read_unchecked()) {
                // `done`, not `build`: the resource of that name is what the
                // rebuild handler below restarts, and the pattern would
                // otherwise shadow it with this one finished build.
                (Some(Ok(Some(pkg))), Some(Ok(done))) => rsx! {
                    crate::screens::PackageHeader {
                        pkg: pkg.clone(),
                        trail: vec![
                            (
                                "Builds".to_string(),
                                Some(crate::routes::Route::PackageBuilds {
                                    pkgbase: done.pkg_name.clone(),
                                }),
                            ),
                            (done.number.to_string(), None),
                        ],
                        on_rebuilt: move |()| {
                            build.restart();
                            package.restart();
                        },
                    }
                },
                // The log is what this page is for, so a failed lookup costs
                // the header rather than the page.
                _ => rsx! {},
            }
            // Not asked for a build that is not there; see the redirect above.
            if !matches!(&*build.read_unchecked(), Some(Err(LoadError::NotFound))) {
                BuildLog { pkgbase, number }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Build log
//
// The interesting screen: output arrives while the build runs. The API is
// incremental rather than streaming — `?offset=N&limit=M` returns a bounded
// page of raw bytes from byte N on — so this polls aligned pages, appends
// them to a cap-sized window, and stops once the build reaches a terminal
// state, with the raw bytes decoded (and the cap kept) client-side.
// ---------------------------------------------------------------------------

/// How often to ask for more output while a build is running.
const POLL_INTERVAL_MS: u32 = 3_000;

#[component]
pub fn BuildLog(pkgbase: String, number: i32) -> Element {
    // One string, one text node. Per-line elements would only earn their keep
    // for per-line features — ANSI colour, line numbers, deep links — and a
    // build emits thousands of lines, so the browser would lay out thousands
    // of elements and the VirtualDom would walk them on every poll.
    let log = use_signal(String::new);
    // How large the log is, from the detail route. As with any "not known" the
    // answer is `None`, not zero: no log file and a zero-byte log are different.
    let log_size = use_signal(|| None::<i64>);
    // Whether the window is a true tail — the initial frame was placed near the
    // end or a leading line was drained to stay in budget. A tail restates what
    // Copy does and how the footer reads; an untrimmed head view is the whole
    // log.
    let tail = use_signal(|| false);
    let finished = use_signal(|| false);
    // The build's real state, not just "is it over". "Not building" also covers
    // *enqueued* and *waiting for deps*, and collapsing those into a boolean is
    // what told someone their freshly queued build had already finished.
    let status = use_signal(|| None::<i32>);
    // Filled from the same poll that decides when the log stops, so a build
    // claimed while this page is open names its worker without a reload.
    let worker_name = use_signal(|| None::<String>);
    // When it started and, once it has, when it stopped. A build that is not
    // even queued has no start, and an ended build always has an end.
    let start_time = use_signal(|| None::<i64>);
    let end_time = use_signal(|| None::<i64>);
    // What the build used on its worker's disk, once it has ended and the
    // worker reported it.
    let disk_usage = use_signal(|| None::<DiskUsage>);
    let error = use_signal(|| Option::<String>::None);
    // True for a couple of seconds after a successful copy, so the button
    // swaps its icon and label to say the log is now on the clipboard.
    let copied = use_signal(|| false);
    // "Follow" pins the view to the bottom as output arrives; switching it off
    // is what lets someone read back through a long log while it is still
    // being written.
    let mut following = use_signal(|| true);
    // A Stop in flight, so the button shows "Stop…" and cannot double-fire.
    let canceling = use_signal(|| false);
    // The Stop dialog is open. Stopping throws away a build that may be hours
    // in, and the button sits beside Copy and Follow, so it asks first.
    let mut confirming_stop = use_signal(|| false);
    // The tail budget, read once on mount: `window.inner_width()` and
    // `(pointer: coarse)` decide phone (4 MiB) versus desktop (16 MiB).
    let cap = use_memo(|| {
        log_tail_cap(
            web_sys::window()
                .and_then(|w| w.inner_width().ok())
                .and_then(|w| w.as_f64()),
            coarse_pointer(),
        )
    });
    // The stop handler moves these; taken before the poll loop below moves the
    // originals, so the button does not borrow what the loop owns.
    let (pkgbase_for_stop, number_for_stop) = (pkgbase.clone(), number);
    // The poll loop is the component's only other `pkgbase` consumer, and it
    // runs for the life of the screen, so it takes its own copy and the rsx
    // below keeps the original for the header and the copy/download buttons.
    let pkgbase_for_poll = pkgbase.clone();
    // Counted once per page that lands, not on every render: the window can be
    // 16 MiB of text, and the header re-renders on every status poll.
    let line_count = use_memo(move || log.read().lines().count());
    use_future(move || {
        let (mut log, mut log_size, mut tail) = (log, log_size, tail);
        let (mut worker_name, mut status, mut start_time, mut end_time) =
            (worker_name, status, start_time, end_time);
        let mut disk_usage = disk_usage;
        let (mut finished, mut error, following) = (finished, error, following);
        let cap = cap();
        let pkgbase = pkgbase_for_poll.clone();
        let number = number;
        async move {
            let client = match crate::api::client() {
                Ok(c) => c,
                Err(e) => {
                    error.set(Some(e));
                    return;
                }
            };

            // The fetch position is private to the poll loop: nothing outside
            // it reads or writes it, and the loop is its only owner. `align`'s
            // back_drop makes it byte-precise — it advances by raw bytes read,
            // never by the length of the decoded text.
            let mut next_offset: u64 = 0;

            loop {
                // No fetch while hidden: a log page sitting in a background
                // tab should not emit two requests per interval, same rule as
                // `use_poll`. The sleep at the loop's end keeps ticking so
                // the return is noticed, at most one interval late.
                if crate::poll::hidden() {
                    gloo_timers::future::TimeoutFuture::new(POLL_INTERVAL_MS).await;
                    continue;
                }

                // The build's state first, each cycle: status, worker, start
                // and end times, and — from the detail route only — the log's
                // size. The output fetch is gated on this, so a hiccup on the
                // header route never fetches without bounds.
                let build = match client.get_build(&pkgbase, number).await {
                    Ok(build) => {
                        worker_name.set(build.worker_name.clone());
                        status.set(Some(build.status));
                        start_time.set(build.start_time);
                        end_time.set(build.end_time);
                        disk_usage.set(build.disk_usage);
                        log_size.set(build.log_size);
                        // A successful poll clears a previous failure: the
                        // banner should describe now, not the worst moment so
                        // far.
                        if error.peek().is_some() {
                            error.set(None);
                        }
                        Some(build)
                    }
                    Err(e) => {
                        error.set(Some(e.to_string()));
                        None
                    }
                };

                // Terminal check after the output fetch below (this cycle's
                // page arrives before the loop returns), so the last lines are
                // never missed. Only a *settled* build stops the loop.
                // `is_in_progress` answers exactly this and keeps the queued
                // states on the right side of it; testing for `Active` alone
                // treated an enqueued build as over, so the page announced
                // "finished" and never updated again when the build started.
                // An unrecognised state from a newer server counts as in
                // progress: being wrong that way costs one poll per interval,
                // while being wrong the other way is this bug.
                let settled_now = build.as_ref().is_some_and(|b| settled(b.status));

                // Output, gated on the header above. Frozen while not following
                // so the view cannot jump under a scroll position — the header
                // still polls, keeping status and times live. A build that
                // settles this cycle gets its final page regardless, then the
                // loop returns, so the last lines are never missed.
                if let Some(build) = build.filter(|_| following() || settled_now) {
                    // A log more than a window ahead of where this got to opens
                    // at its *end*, not at the next page: on the first frame,
                    // so a finished log shows its last lines rather than the
                    // first 4 MiB of a multi-GiB file, and after Follow was off
                    // while the build kept writing, so a build that settles
                    // then still shows how it ended. The window starts over,
                    // and the leading partial line that `size − cap` can land
                    // on is stripped below, after `align`.
                    let size = build.log_size.and_then(|s| u64::try_from(s).ok());
                    if let Some(offset) = catch_up_offset(next_offset, size, cap as u64) {
                        next_offset = offset;
                        log.set(String::new());
                        tail.set(true);
                    }

                    // One bounded page per poll: the window never holds
                    // more than the cap, and the server never reads more
                    // than the cap's worth either.
                    let requested = next_offset;
                    match client
                        .build_output_page(&pkgbase, number, Some(requested), Some(cap as u64))
                        .await
                    {
                        Ok(page) => {
                            if error.peek().is_some() {
                                error.set(None);
                            }
                            if !page.is_empty() {
                                let (front_skip, back_drop) = align(&page);
                                // A page that aligns to nothing — the log
                                // ended mid-codepoint and it is all tail —
                                // cannot advance `next_offset`, so it is
                                // EOF; nothing is appended.
                                let slice = &page[front_skip..page.len() - back_drop];
                                if !slice.is_empty() {
                                    let chunk = String::from_utf8_lossy(slice);
                                    // First real frame at a tail offset:
                                    // the view opens mid-line, so the
                                    // partial leading line is dropped once
                                    // to start at a line boundary.
                                    let first_tail_frame = requested > 0 && log.read().is_empty();
                                    let trimmed =
                                        log.with_mut(|text| append_capped(text, &chunk, cap));
                                    if trimmed || first_tail_frame {
                                        tail.set(true);
                                    }
                                    if first_tail_frame {
                                        log.with_mut(drop_leading_partial_line);
                                    }
                                }
                                // Advance by what was actually read — raw
                                // bytes minus the incomplete trailing
                                // codepoint — never by the length of the
                                // decoded text.
                                next_offset = requested + (page.len() - back_drop) as u64;
                            }
                        }
                        Err(e) => {
                            // Report and keep polling. Returning here
                            // abandoned the log for the life of the page,
                            // so one timeout on a slow connection meant a
                            // running build stopped updating until it was
                            // reloaded by hand.
                            error.set(Some(e.to_string()));
                        }
                    }
                }

                // Only now, with the final page in: setting this as soon as the
                // header said settled announced "no log for this build" for as
                // long as a large log took to arrive.
                if settled_now {
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
    // is finally correct. It re-runs when `log` grows (a new page landed) or
    // when `following` flips back on (re-pin immediately).
    use_effect(move || {
        // Read only to subscribe to it.
        let _ = log.read();
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
                    if let Some(disk) = disk_usage().as_ref().and_then(disk_usage_summary) {
                        span {
                            class: "text-sm opacity-70 whitespace-nowrap",
                            title: "Disk the build used on its worker, as stored: what it installed into its chroot, its working space (source and packages), its package's source cache, and its kept build tree.",
                            "Disk: {disk}"
                        }
                    }
                    div { class: "flex-1" }
                    // Stop a build that is not over yet: running, enqueued, or
                    // waiting for deps. Same `settled` gate as the poll loop,
                    // so a build that just started is stoppable the moment this
                    // page opens and a state from a newer server is too. Not
                    // one being published: it has already been built, and the
                    // server would refuse.
                    if status().is_some_and(|s| {
                        !settled(s) && BuildState::from_i32(s) != Some(BuildState::Publishing)
                    }) {
                        button {
                            disabled: canceling(),
                            class: "btn btn-xs btn-error btn-outline",
                            onclick: move |_| confirming_stop.set(true),
                            if canceling() { "Stopping…" } else { "Stop" }
                        }
                        // Inside the same condition as the button, so a build
                        // that ends while the dialog is open takes the dialog
                        // with it rather than offering to stop a finished build.
                        div {
                            class: if confirming_stop() { "modal modal-open" } else { "modal" },
                            role: "dialog",
                            aria_modal: "true",
                            aria_label: "Confirm stopping the build",
                            div { class: "modal-box",
                                h3 { class: "font-bold text-lg",
                                    "Stop {pkgbase_for_stop} build #{number_for_stop}?"
                                }
                                p { class: "text-sm opacity-70 pt-2",
                                    if status() == Some(BuildState::Active as i32) {
                                        "It is killed where it is and ends as canceled. Nothing it has built so \
                                         far is published; retrying it starts the build again."
                                    } else {
                                        "It leaves the queue without starting and ends as canceled. Retrying it \
                                         queues it again."
                                    }
                                }
                                div { class: "modal-action",
                                    button {
                                        class: "btn btn-sm",
                                        onclick: move |_| confirming_stop.set(false),
                                        "Keep building"
                                    }
                                    button {
                                        class: "btn btn-error btn-sm",
                                        onclick: move |_| {
                                            confirming_stop.set(false);
                                            // Signals are Copy; both are rebound `mut`
                                            // for the async body below.
                                            let (mut canceling, mut error) = (canceling, error);
                                            let (pkgbase, number) =
                                                (pkgbase_for_stop.clone(), number_for_stop);
                                            spawn(async move {
                                                canceling.set(true);
                                                let result = match crate::api::client() {
                                                    Ok(client) => {
                                                        match client.cancel_build(&pkgbase, number).await {
                                                            Ok(()) => Ok(()),
                                                            Err(e) => Err(e.to_string()),
                                                        }
                                                    }
                                                    Err(e) => Err(e),
                                                };
                                                if let Err(e) = result {
                                                    error.set(Some(e));
                                                    canceling.set(false);
                                                }
                                            });
                                        },
                                        "Stop build"
                                    }
                                }
                            }
                            // Clicking away is "no", as it is for every dialog here.
                            button {
                                class: "modal-backdrop",
                                onclick: move |_| confirming_stop.set(false),
                                aria_label: "Keep building",
                                "Close"
                            }
                        }
                    }
                    LogCopyButton {
                        pkgbase,
                        number,
                        log,
                        tail,
                        cap: cap(),
                        copied,
                        error,
                    }
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
                    if log.read().is_empty() {
                        span { class: "opacity-60", {empty_log_placeholder(finished(), status())} }
                    } else {
                        "{log}"
                    }
                }
                // The footer answers "how much of it is here". An untrimmed
                // window is the whole log, so it counts lines in the window.
                // A trimmed one is a tail, so it says how much of the total it
                // covers -- the size is what is actually missing.
                if tail() {
                    div {
                        class: "text-sm opacity-60",
                        {
                            // The total comes from the header route's
                            // `log_size`: a build may have created its log
                            // after the last poll, in which case "not known"
                            // reads as a dash, one of the several answers
                            // `None` has here.
                            let total = log_size()
                                .map(|s| format_bytes(s as u64))
                                .unwrap_or_else(|| "—".to_string());
                            let shown = format_bytes(cap() as u64);
                            format!("showing the last ~{shown} of {total} (~{line_count} lines in view)")
                        }
                    }
                } else {
                    div { class: "text-sm opacity-60", "{line_count} lines" }
                }
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
///
/// Once the window is a true tail — placed at `offset > 0` on first frame, or
/// a leading line drained since — the button stops pretending it copies the
/// whole log: a `!` marker goes on, the label flips to "Copy tail", and the
/// title says to use the Download link for everything. Both stay ≤ cap + one
/// line, so the clipboard path is never unsafe.
#[component]
fn LogCopyButton(
    pkgbase: String,
    number: i32,
    log: Signal<String>,
    tail: Signal<bool>,
    cap: usize,
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
    // A relative URL: same origin as the page, so the browser attaches the
    // session cookie itself (same rule as `api_base`), and nothing is touched
    // at render time for the host-side render tests to trip over.
    let download_url = format!("/api/package/{pkgbase}/build/{number}/output/download");
    let download_name = format!("{pkgbase}-{number}.log");
    let copy_title = if tail() {
        format!(
            "Copying the shown ~{} tail, not the full log — use Download for the whole log",
            format_bytes(cap as u64)
        )
    } else {
        "Copy the whole build log to the clipboard".to_string()
    };
    rsx! {
        div { class: "flex items-center gap-1.5",
            a {
                class: "btn btn-ghost btn-xs gap-1.5",
                // The page is session-cookie authenticated, so a bare anchor
                // carries the session and the browser streams the response to
                // its download manager with nothing landing in the JS heap —
                // important, since a multi-GiB log must never materialise
                // client-side (`fetch` + `response.blob()` would).
                href: download_url,
                download: download_name,
                title: "Download the whole build log",
                DownloadIcon {}
                span { "Download" }
            }
            button {
                class: "btn btn-ghost btn-xs gap-1.5",
                title: copy_title,
                disabled: copied(),
                onclick: move |_| async move {
                    // The window is one string in memory, so nothing extra is
                    // fetched — the button copies everything written so far.
                    // For a tail the marker says "and no more": the rest of
                    // the file is only reachable through the Download link.
                    match crate::clipboard::copy_text(&log()).await {
                        Ok(()) => {
                            copied.set(true);
                            // Let the checkmark say its piece, then give the button
                            // back its job.
                            gloo_timers::future::TimeoutFuture::new(2000).await;
                            copied.set(false);
                        }
                        Err(e) => error.set(Some(e.message("the build log"))),
                    }
                },
                if copied() {
                    CheckIcon {}
                    span { "Copied" }
                } else {
                    if tail() { WarnIcon {} }
                    CopyIcon {}
                    span { if tail() { "Copy tail" } else { "Copy" } }
                }
            }
        }
    }
}

/// The measured parts of a build's disk usage, as one line: `chroot 1.2 GiB ·
/// workdir 40 MiB`. `None` when nothing was measured. A part left out was not
/// measured -- which is not the same as zero, so it is not shown as one.
pub(crate) fn disk_usage_summary(usage: &DiskUsage) -> Option<String> {
    let parts: Vec<String> = [
        ("chroot", usage.chroot),
        ("workdir", usage.workdir),
        ("sources", usage.sources),
        ("build tree", usage.build_tree),
    ]
    .into_iter()
    .filter_map(|(name, bytes)| {
        let bytes = u64::try_from(bytes?).ok()?;
        Some(format!("{name} {}", format_bytes(bytes)))
    })
    .collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

#[cfg(test)]
mod disk_usage_tests {
    use super::*;

    #[test]
    fn only_measured_parts_are_shown() {
        let usage = DiskUsage {
            chroot: Some(3 << 30),
            workdir: Some(40 << 20),
            sources: None,
            build_tree: Some(0),
        };
        assert_eq!(
            disk_usage_summary(&usage).as_deref(),
            Some("chroot 3.0 GiB · workdir 40 MiB · build tree 0 B")
        );
        assert_eq!(disk_usage_summary(&DiskUsage::default()), None);
    }
}
