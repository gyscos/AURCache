//! The log: what happened, when, who asked for it, and what went wrong.
//!
//! Not only what people did. The server writes an entry wherever it already
//! knows something failed -- a build that published nowhere, a worker that
//! stopped answering -- so the page you read to see what has been going on is
//! also the page that tells you something is off.
//!
//! The one list in this app filtered *server-side*. Everything else fetches
//! its rows and filters them in the browser (see `crate::listing`); the log
//! cannot be fetched whole, because it only grows, and a filter applied to one
//! page would only ever search the page you were already looking at.

use crate::dates::AbsoluteDate;
use crate::listing::{ListHeader, PAGE_SIZE, Pager, ViewParams, use_url_view};
use crate::routes::Route;
use aurcache_client::{ActiveOperation, Activity, ActivitySubject, Severity, operation_kind};
use dioxus::prelude::*;
use std::time::Duration;

/// How much of the log to fetch at a time.
///
/// The same page size the other lists use, but fetched a page at a time rather
/// than sliced from everything: the log only grows, so there is no point at
/// which asking for all of it is the cheap option.
const PAGE: u64 = PAGE_SIZE as u64;

/// How often to re-ask what is still running.
///
/// Slower than a progress card's own polling: this list only reports *that*
/// something is running, and a job showing up a second late costs nothing.
const RUNNING_POLL: Duration = Duration::from_secs(3);

#[component]
pub fn Logs(view: ViewParams) -> Element {
    let mut page = use_signal(|| 0usize);
    let severity = use_signal(|| view.severity);
    let since_boot = use_signal(|| view.since_boot);

    // The whole route from live control state, which is what keeps a filtered
    // log linkable and survives a reload.
    let to_route = move |_: String| Route::Logs {
        view: ViewParams::for_logs(severity(), since_boot()),
    };
    use_url_view(use_signal(String::new), true, to_route);

    // Narrowing the log changes what page 1 even is, so start again from it --
    // otherwise a filter applied on page 4 lands past the end of a shorter log.
    use_effect(use_reactive(&(severity(), since_boot()), move |_| {
        page.set(0);
    }));

    let entries = use_resource(move || async move {
        let offset = page() as u64 * PAGE;
        crate::api::client()?
            .activities(Some(PAGE), Some(offset), severity(), since_boot())
            .await
            .map_err(|e| e.to_string())
    });

    rsx! {
        div { class: "space-y-4",

        RunningOperations {}

        div { class: "card bg-base-100 shadow-xl",
            div { class: "card-body",
                ListHeader { title: "Logs" }
                LogControls { severity, since_boot }

                match &*entries.read_unchecked() {
                    None => rsx! {
                        div { class: "flex justify-center p-8",
                            span { class: "loading loading-spinner loading-lg" }
                        }
                    },
                    Some(Err(e)) => rsx! {
                        div { class: "alert alert-error", span { "Could not load the log: {e}" } }
                    },
                    Some(Ok(view)) if view.total == 0 => rsx! {
                        div { class: "alert",
                            // A filter that matched nothing is a different
                            // answer from an empty log, and the remedy is
                            // different too.
                            if severity().is_some() || since_boot() {
                                span { "Nothing matches these filters." }
                            } else {
                                span { "Nothing has happened yet." }
                            }
                        }
                    },
                    Some(Ok(view)) => rsx! {
                        ActivityTable { entries: view.entries.clone() }
                        Pager {
                            page,
                            index: page().min(pages_of(view.total).saturating_sub(1)),
                            pages: pages_of(view.total),
                            first: page() * PAGE_SIZE,
                            count: view.entries.len(),
                            total: view.total as usize,
                        }
                    },
                }
            }
        }
        }
    }
}

/// Long-running work still in flight, with a way back to its progress card.
///
/// A bulk add outlives the dialog that started it and the browser that was
/// watching, so without this a job whose card was dismissed -- or which was
/// started from another tab, or before a reload -- is running with nothing on
/// screen to say so. The server knows; this is what asks it.
///
/// Absent entirely when nothing is running: a permanent empty panel on a page
/// about history would be noise.
#[component]
fn RunningOperations() -> Element {
    let jobs = crate::progress::use_jobs();

    let mut running = use_resource(|| async move {
        // Polled rather than fetched once: this page is somewhere to leave open
        // while something runs, and a list that went stale the moment it loaded
        // would be worse than not having it.
        loop {
            if let Ok(client) = crate::api::client()
                && let Ok(list) = client.active_operations().await
            {
                return list;
            }
            gloo_timers::future::sleep(RUNNING_POLL).await;
        }
    });

    // Re-ask on a timer, so a job that starts or ends while this page is open
    // appears or goes without a reload.
    use_future(move || async move {
        loop {
            gloo_timers::future::sleep(RUNNING_POLL).await;
            running.restart();
        }
    });

    let list = running.read_unchecked().clone().unwrap_or_default();
    if list.is_empty() {
        return rsx! {};
    }

    rsx! {
        div { class: "card bg-base-100 shadow-xl",
            div { class: "card-body",
                ListHeader { title: "Running now" }
                div { class: "space-y-2",
                    for operation in list {
                        RunningRow { key: "{operation.id}", operation, jobs }
                    }
                }
            }
        }
    }
}

/// One running operation.
#[component]
fn RunningRow(operation: ActiveOperation, jobs: Signal<Vec<crate::progress::Job>>) -> Element {
    let resolved = operation.resolved_count();
    let watching = jobs
        .read()
        .iter()
        .any(|job| job.operation_id() == Some(operation.id));

    // Both kinds have a card; they differ in what they poll and in the words
    // they report outcomes with. A kind this build does not know has neither,
    // so it gets no button rather than one that would open an empty card.
    let known = matches!(
        operation.kind.as_str(),
        operation_kind::BULK_ADD | operation_kind::RESTORE
    );
    let label = match operation.kind.as_str() {
        operation_kind::BULK_ADD => format!("Adding {} packages", operation.total),
        operation_kind::RESTORE => format!("Restoring {} packages", operation.total),
        // A kind this build does not know about, from a newer server. Saying so
        // beats hiding it.
        other => format!("{other} ({} items)", operation.total),
    };

    rsx! {
        div { class: "flex items-center gap-3",
            span { class: "loading loading-spinner loading-xs" }
            div { class: "flex-1 min-w-0",
                div { class: "text-sm truncate", "{label}" }
                div { class: "text-xs opacity-60",
                    "{resolved} of {operation.total}"
                    if operation.failed > 0 {
                        ", {operation.failed} failed"
                    }
                }
            }
            if known {
                if watching {
                    span { class: "text-xs opacity-60", "shown" }
                } else {
                    button {
                        class: "btn btn-ghost btn-xs",
                        onclick: move |_| {
                            let (id, total, label) = (operation.id, operation.total, label.clone());
                            if operation.kind == operation_kind::RESTORE {
                                crate::progress::watch_restore(jobs, id, total, label);
                            } else {
                                crate::progress::watch_add(jobs, id, total, label);
                            }
                        },
                        "Show progress"
                    }
                }
            }
        }
    }
}

/// An entry's text, with the thing it is about turned into a link.
///
/// The server sends prose and, separately, what the entry is about. Splitting
/// here rather than sending markup keeps the server out of the business of
/// laying out a page, and keeps an entry readable if this ever renders
/// somewhere without links.
///
/// Only the first occurrence is linked: the name usually appears once, and
/// three links to the same page in one sentence is noise.
#[component]
fn EntryText(entry: Activity) -> Element {
    let Some((before, name, after)) = entry
        .subject
        .as_ref()
        .and_then(|subject| split_on(&entry.text, subject.name()))
    else {
        return rsx! { "{entry.text}" };
    };

    // Unreachable otherwise: `split_on` only matched because there was a
    // subject to match on.
    let Some(to) = entry.subject.as_ref().map(subject_route) else {
        return rsx! { "{entry.text}" };
    };

    rsx! {
        "{before}"
        Link { class: "link-hover font-medium", to, "{name}" }
        "{after}"
    }
}

/// Where an entry's subject opens.
///
/// A worker by name, which is how its page is addressed -- and which lands on
/// the chooser if two machines have answered to that name, rather than guessing
/// at one of them.
fn subject_route(subject: &ActivitySubject) -> Route {
    match subject {
        ActivitySubject::Package(name) => Route::Package {
            pkgbase: name.clone(),
        },
        ActivitySubject::Worker(name) => Route::Worker {
            name: crate::screens::worker::name_segments(name),
        },
    }
}

/// The text either side of `name`'s first occurrence, or `None` if it does not
/// appear.
///
/// Whole-word: a package called `hello` must not light up the `hello` inside
/// `hello-world`, which would link to a page that is not what the entry is
/// about.
fn split_on<'a>(text: &'a str, name: &str) -> Option<(&'a str, &'a str, &'a str)> {
    if name.is_empty() {
        return None;
    }
    let mut from = 0;
    while let Some(offset) = text[from..].find(name) {
        let start = from + offset;
        let end = start + name.len();
        let bounded = |c: Option<char>| c.is_none_or(|c| !c.is_alphanumeric() && c != '-');
        if bounded(text[..start].chars().next_back()) && bounded(text[end..].chars().next()) {
            return Some((&text[..start], &text[start..end], &text[end..]));
        }
        // Advance past this occurrence rather than past the whole prefix, so a
        // later, properly bounded one is still found.
        from = end;
    }
    None
}

/// What to narrow the log to.
///
/// Its own controls rather than the shared [`crate::listing::ListControls`]:
/// that pair is a name search and a build status, and the log has neither. A
/// search box that did nothing would be worse than no search box.
#[component]
fn LogControls(severity: Signal<Option<Severity>>, since_boot: Signal<bool>) -> Element {
    let mut severity = severity;
    let mut since_boot = since_boot;

    rsx! {
        div { class: "flex flex-wrap items-center gap-3 mb-2",
            select {
                class: "select select-bordered select-sm",
                aria_label: "Filter by severity",
                onchange: move |e| severity.set(Severity::from_slug(&e.value())),
                option { value: "", selected: severity().is_none(), "Everything" }
                option {
                    value: Severity::Warning.slug(),
                    selected: severity() == Some(Severity::Warning),
                    "Warnings and errors"
                }
                option {
                    value: Severity::Error.slug(),
                    selected: severity() == Some(Severity::Error),
                    "Errors only"
                }
            }
            label { class: "label cursor-pointer gap-2 py-0",
                input {
                    r#type: "checkbox",
                    class: "checkbox checkbox-sm",
                    aria_label: "Since the last restart",
                    checked: since_boot(),
                    onchange: move |e| since_boot.set(e.checked()),
                }
                span { class: "label-text text-sm", "Since the last restart" }
            }
        }
    }
}

/// How loud an entry is, for the eye rather than the filter.
///
/// Informational entries carry no badge at all: they are most of the log, and a
/// badge on every row would make the ones that matter harder to find, not
/// easier.
#[component]
fn SeverityBadge(severity: Severity) -> Element {
    let (label, class) = match severity {
        Severity::Info => return rsx! {},
        Severity::Warning => ("warning", "badge-warning"),
        Severity::Error => ("error", "badge-error"),
    };
    rsx! {
        span { class: "badge {class} badge-sm mr-2 align-middle", "{label}" }
    }
}

/// How many pages a log of this length has.
///
/// At least one, so an empty log reads as "page 1 of 1" rather than "of 0".
fn pages_of(total: u64) -> usize {
    (total as usize).div_ceil(PAGE_SIZE).max(1)
}

/// Columns that only appear once there is room for them.
const WIDE_ONLY: &str = "hidden md:table-cell";

#[component]
fn ActivityTable(entries: Vec<Activity>) -> Element {
    rsx! {
        div { class: "overflow-x-auto",
            table { class: "table table-zebra",
                thead {
                    tr {
                        // The two narrow columns take only what they need, so
                        // the text -- the column anyone is actually reading --
                        // gets the rest of the width.
                        th { class: "w-px whitespace-nowrap", "When" }
                        th { class: "{WIDE_ONLY} w-px whitespace-nowrap", "Who" }
                        th { "What" }
                    }
                }
                tbody {
                    for (index, entry) in entries.iter().enumerate() {
                        // The log has no ids and entries are not unique — the
                        // same package updated twice a second apart is two
                        // identical rows — so position is the only key.
                        tr { key: "{index}",
                            td { class: "w-px whitespace-nowrap text-sm opacity-70",
                                AbsoluteDate { ts: Some(entry.timestamp) }
                            }
                            td { class: "{WIDE_ONLY} w-px whitespace-nowrap text-sm",
                                {actor(entry.user.as_deref())}
                            }
                            td { class: "text-sm",
                                SeverityBadge { severity: entry.severity }
                                EntryText { entry: entry.clone() }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Who to credit an entry to.
///
/// A missing user is the server acting on its own — a schedule firing, a
/// version check finding something — not an anonymous person. The Dart
/// frontend rendered that as "You", which is wrong in both directions: it
/// claims work you did not do, and on a multi-user instance it would claim
/// someone else's.
fn actor(user: Option<&str>) -> String {
    user.map_or_else(|| "AURCache".to_string(), ToString::to_string)
}

#[cfg(test)]
mod tests {
    use super::{ActivityTable, ActivityTableProps, actor, split_on, subject_route};
    use crate::routes::Route;
    use aurcache_client::{Activity, ActivitySubject, Severity};
    use dioxus::prelude::*;

    /// A package named in an entry has to be picked out exactly, or the link
    /// goes to a package the entry is not about.
    #[test]
    fn a_name_is_matched_whole() {
        assert_eq!(
            split_on("added package hello", "hello"),
            Some(("added package ", "hello", ""))
        );
        assert_eq!(
            split_on("added package hello-world", "hello-world"),
            Some(("added package ", "hello-world", ""))
        );
        // `hello` inside `hello-world` is a different package.
        assert_eq!(split_on("added package hello-world", "hello"), None);
        assert_eq!(split_on("added package neofetch", "hello"), None);
        assert_eq!(split_on("added package hello", ""), None);
    }

    /// The first *bounded* occurrence, not the first substring match: a name
    /// that appears inside a longer word before it appears on its own must not
    /// swallow the real one.
    #[test]
    fn a_later_whole_match_is_still_found() {
        assert_eq!(
            split_on("hello-world depends on hello", "hello"),
            Some(("hello-world depends on ", "hello", ""))
        );
    }

    /// Only the first occurrence is linked; three links to one page in a
    /// sentence is noise.
    #[test]
    fn only_the_first_occurrence_is_split_out() {
        let (before, name, after) = split_on("hello needs hello", "hello").unwrap();
        assert_eq!(before, "");
        assert_eq!(name, "hello");
        assert_eq!(after, " needs hello");
    }

    #[test]
    fn an_entry_with_no_user_is_credited_to_the_server() {
        assert_eq!(actor(Some("alice")), "alice");
        assert_eq!(actor(None), "AURCache");
    }

    /// The three columns are the whole screen, so a row that drops one is the
    /// only defect worth guarding here.
    #[test]
    fn a_row_shows_when_who_and_what() {
        let mut dom = VirtualDom::new_with_props(
            ActivityTable,
            ActivityTableProps {
                entries: vec![
                    Activity {
                        timestamp: 1_756_000_000,
                        text: "added package hello".to_string(),
                        user: Some("alice".to_string()),
                        severity: Severity::Info,
                        // No subject in this fixture: a `Link` needs a router
                        // context that a bare render test has none of, so what
                        // the link points at is asserted by `subject_route`
                        // below and that it works by the browser suite.
                        subject: None,
                    },
                    Activity {
                        timestamp: 1_756_000_100,
                        text: "rebuilt yay".to_string(),
                        user: None,
                        severity: Severity::Info,
                        subject: None,
                    },
                    Activity {
                        timestamp: 1_756_000_200,
                        text: "publishing build #3 of hello failed: disk full".to_string(),
                        user: None,
                        severity: Severity::Error,
                        subject: None,
                    },
                ],
            },
        );
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);

        assert!(html.contains("added package hello"), "{html}");
        assert!(html.contains("alice"), "{html}");
        assert!(html.contains("rebuilt yay"), "{html}");
        assert!(html.contains("AURCache"), "unattributed entry: {html}");

        // A failure is marked; ordinary news is not, or the badge would be on
        // every row and pick nothing out.
        assert!(html.contains("badge-error"), "{html}");
        assert!(!html.contains("badge-info"), "{html}");
    }

    /// Where a subject opens. A worker goes by name, which is how its page is
    /// addressed and which copes with two machines sharing one.
    #[test]
    fn a_subject_opens_its_own_page() {
        assert_eq!(
            subject_route(&ActivitySubject::Package("hello".to_string())),
            Route::Package {
                pkgbase: "hello".to_string()
            }
        );
        assert_eq!(
            subject_route(&ActivitySubject::Worker("builder-01".to_string())),
            Route::Worker {
                name: vec!["builder-01".to_string()]
            }
        );
    }
}
