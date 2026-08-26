//! Stand-in for a route the Rust frontend does not implement yet.

use dioxus::prelude::*;

/// Says plainly that a screen is missing, and why where there is a reason.
///
/// The alternative — leaving unported routes out of the menu — hides how much
/// of the app is still Dart, which is the thing worth being able to see at a
/// glance while the port is in progress.
#[component]
pub fn NotPorted(title: String, note: String) -> Element {
    rsx! {
        div { class: "card bg-base-100 shadow-xl",
            div { class: "card-body items-center text-center py-16",
                h2 { class: "card-title", "{title}" }
                p { class: "opacity-70", "Not ported to the Rust frontend yet." }
                if !note.is_empty() {
                    p { class: "opacity-50 text-sm max-w-md", "{note}" }
                }
            }
        }
    }
}
