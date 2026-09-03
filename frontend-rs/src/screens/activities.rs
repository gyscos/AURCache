//! The activity log: what happened, when, and who asked for it.

use crate::dates::AbsoluteDate;
use crate::listing::ListHeader;
use aurcache_client::{ActiveOperation, Activity, operation_kind};
use dioxus::prelude::*;
use std::time::Duration;

/// How much of the log to fetch.
///
/// The endpoint takes a limit and defaults to a small one; a page worth
/// scrolling is more useful than a handful, and the log is short rows of text.
const PAGE: u64 = 200;

/// How often to re-ask what is still running.
///
/// Slower than a progress card's own polling: this list only reports *that*
/// something is running, and a job showing up a second late costs nothing.
const RUNNING_POLL: Duration = Duration::from_secs(3);

#[component]
pub fn Activities() -> Element {
    let entries = use_resource(|| async move {
        crate::api::client()?
            .activities(Some(PAGE))
            .await
            .map_err(|e| e.to_string())
    });

    rsx! {
        div { class: "space-y-4",

        RunningOperations {}

        div { class: "card bg-base-100 shadow-xl",
            div { class: "card-body",
                ListHeader { title: "Activities" }

                match &*entries.read_unchecked() {
                    None => rsx! {
                        div { class: "flex justify-center p-8",
                            span { class: "loading loading-spinner loading-lg" }
                        }
                    },
                    Some(Err(e)) => rsx! {
                        div { class: "alert alert-error", span { "Could not load the log: {e}" } }
                    },
                    Some(Ok(list)) if list.is_empty() => rsx! {
                        div { class: "alert", span { "Nothing has happened yet." } }
                    },
                    Some(Ok(list)) => rsx! {
                        ActivityTable { entries: list.clone() }
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

/// Columns that only appear once there is room for them.
const WIDE_ONLY: &str = "hidden md:table-cell";

#[component]
fn ActivityTable(entries: Vec<Activity>) -> Element {
    rsx! {
        div { class: "overflow-x-auto",
            table { class: "table table-zebra",
                thead {
                    tr {
                        th { "When" }
                        th { class: "{WIDE_ONLY}", "Who" }
                        th { "What" }
                    }
                }
                tbody {
                    for (index, entry) in entries.iter().enumerate() {
                        // The log has no ids and entries are not unique — the
                        // same package updated twice a second apart is two
                        // identical rows — so position is the only key.
                        tr { key: "{index}",
                            td { class: "whitespace-nowrap text-sm opacity-70",
                                AbsoluteDate { ts: Some(entry.timestamp) }
                            }
                            td { class: "{WIDE_ONLY} text-sm",
                                {actor(entry.user.as_deref())}
                            }
                            td { class: "text-sm", "{entry.text}" }
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
    use super::{ActivityTable, ActivityTableProps, actor};
    use aurcache_client::Activity;
    use dioxus::prelude::*;

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
                    },
                    Activity {
                        timestamp: 1_756_000_100,
                        text: "rebuilt yay".to_string(),
                        user: None,
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
    }
}
