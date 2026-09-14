//! The two-pane source editor: a file list, and the file being edited.
//!
//! Used twice, for two flows that look the same and behave differently. A
//! package that already exists edits against the server, which stores the
//! change as a diff and answers with the patched content. A package that does
//! not exist yet edits against a preview of its upstream source, keeping the
//! result in the browser until it is sent along with the add — which is the
//! only way to add a package whose PKGBUILD fails to parse, since there is no
//! package to fix afterwards.
//!
//! What is shared is the shape and the rules for showing it: which file is
//! open, whether it differs from upstream, and where the buttons go. Loading
//! and saving are the caller's, because that is the part that genuinely
//! differs — a component that did both behind a flag would be two components
//! wearing one name.
//!
//! No syntax highlighting: the Dart original used a plain text field, so a
//! textarea is like-for-like and needs no JS editor component.

use dioxus::prelude::*;

/// A file list beside an editor for the file selected in it.
#[component]
pub fn SourcePane(
    /// Heading over the file list — the package, however the caller names it.
    title: String,
    /// The file list, or how it failed. `None` while it is still loading.
    files: Option<Result<Vec<String>, String>>,
    selected: Option<String>,
    onselect: EventHandler<String>,
    /// Paths that differ from upstream, marked in the list so a change made
    /// and navigated away from is still visible.
    #[props(default)]
    modified: Vec<String>,
    /// The text being edited. A signal rather than a value and a callback,
    /// because the caller has to read it to save it.
    draft: Signal<String>,
    /// Whether the open file differs from what was loaded into it.
    dirty: bool,
    /// Whether the open file already carried a change before this edit.
    #[props(default = false)]
    patched: bool,
    /// Whether the open file's stored patch no longer applies to the current
    /// upstream, leaving only the pristine content in the editor.
    #[props(default = false)]
    patch_failed: bool,
    /// Buttons belonging to the surrounding flow: Save here, Done there.
    actions: Element,
    /// Warnings and results the caller wants above the text.
    notices: Element,
    /// How tall the text area is. A modal has less room than a page.
    #[props(default = String::from("h-[60vh]"))]
    height: String,
) -> Element {
    let mut draft = draft;

    rsx! {
        div { class: "flex gap-4",
            div { class: "card bg-base-100 shadow-xl w-72 shrink-0",
                div { class: "card-body p-4",
                    h2 { class: "card-title text-base break-all", "{title}" }
                    match files {
                        None => rsx! { span { class: "loading loading-spinner loading-sm" } },
                        Some(Err(e)) => rsx! { div { class: "alert alert-error text-xs", "{e}" } },
                        Some(Ok(list)) => rsx! {
                            ul { class: "menu menu-sm p-0 max-h-[60vh] overflow-y-auto flex-nowrap",
                                for path in list.iter() {
                                    li { key: "{path}",
                                        a {
                                            class: if selected.as_deref() == Some(path.as_str()) { "active font-mono" } else { "font-mono" },
                                            onclick: {
                                                let path = path.clone();
                                                move |_| onselect.call(path.clone())
                                            },
                                            span { class: "truncate", "{path}" }
                                            // A file changed and then left is
                                            // otherwise indistinguishable from
                                            // an untouched one.
                                            if modified.iter().any(|m| m == path) {
                                                span { class: "badge badge-warning badge-xs ml-auto", "edited" }
                                            }
                                        }
                                    }
                                }
                            }
                        },
                    }
                }
            }

            div { class: "card bg-base-100 shadow-xl flex-1 min-w-0",
                div { class: "card-body",
                    match selected {
                        None => rsx! { p { class: "opacity-60", "Select a file to edit." } },
                        Some(path) => rsx! {
                            div { class: "flex items-center gap-2 flex-wrap",
                                h3 { class: "font-mono font-medium break-all", "{path}" }
                                if patched { span { class: "badge badge-warning badge-sm", "patched" } }
                                if patch_failed { span { class: "badge badge-error badge-sm", "patch failed" } }
                                if dirty { span { class: "badge badge-info badge-sm", "unsaved" } }
                                div { class: "flex-1" }
                                {actions}
                            }
                            {notices}
                            textarea {
                                class: "textarea textarea-bordered font-mono text-xs w-full leading-snug {height}",
                                spellcheck: "false",
                                value: "{draft}",
                                oninput: move |e| draft.set(e.value()),
                            }
                        },
                    }
                }
            }
        }
    }
}
