//! The activity log: what happened, when, and who asked for it.

use crate::dates::AbsoluteDate;
use crate::listing::ListHeader;
use aurcache_client::Activity;
use dioxus::prelude::*;

/// How much of the log to fetch.
///
/// The endpoint takes a limit and defaults to a small one; a page worth
/// scrolling is more useful than a handful, and the log is short rows of text.
const PAGE: u64 = 200;

#[component]
pub fn Activities() -> Element {
    let entries = use_resource(|| async move {
        crate::api::client()?
            .activities(Some(PAGE))
            .await
            .map_err(|e| e.to_string())
    });

    rsx! {
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
