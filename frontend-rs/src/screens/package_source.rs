//! Editing a package's source files.

use crate::api::api_base;
use aurcache_client::{AurCacheClient, SourceFileContent};
use dioxus::prelude::*;

/// The screen behind `/package/:pkgbase/source/:..path`.
///
/// The path arrives as segments because a source path contains slashes;
/// rejoining them is what makes a nested file linkable.
#[component]
pub fn PackageSource(pkgbase: String, path: Vec<String>) -> Element {
    let initial_path = (!path.is_empty()).then(|| path.join("/"));
    rsx! { SourceEditor { pkgbase, initial_path } }
}

// ---------------------------------------------------------------------------
// Source patch editor
//
// The server stores a package's local modifications as a diff against the
// pristine upstream source. The editor never shows that diff: it shows a file,
// you change it, and the server derives the patch. Writing the original
// content back is how a file is un-patched.
//
// The Dart original uses a plain text field — no syntax highlighting — so a
// textarea is a like-for-like replacement and needs no JS editor component.
// ---------------------------------------------------------------------------

#[component]
pub fn SourceEditor(pkgbase: String, initial_path: Option<String>) -> Element {
    let files = use_resource({
        let pkgbase = pkgbase.clone();
        move || {
            let pkgbase = pkgbase.clone();
            async move {
                let client = AurCacheClient::new(api_base(), None).map_err(|e| e.to_string())?;
                client
                    .list_source_files(&pkgbase)
                    .await
                    .map(|l| l.files)
                    .map_err(|e| e.to_string())
            }
        }
    });

    let mut selected = use_signal(|| Option::<String>::None);
    let mut loaded = use_signal(|| Option::<SourceFileContent>::None);
    // Edited text, kept separate from what was loaded so "dirty" and "revert"
    // are both expressible.
    let mut draft = use_signal(String::new);
    let mut status = use_signal(|| Option::<(String, bool)>::None);

    let open_file = move |pkgbase: String, path: String| async move {
        let Ok(client) = AurCacheClient::new(api_base(), None) else {
            return;
        };
        match client.get_source_file(&pkgbase, &path).await {
            Ok(content) => {
                // Show the patched content when there is some; otherwise the
                // pristine file. A patch that no longer applies falls back to
                // pristine and says so, rather than showing nothing.
                draft.set(
                    content
                        .patched_content
                        .clone()
                        .unwrap_or_else(|| content.original_content.clone()),
                );
                loaded.set(Some(content));
                selected.set(Some(path));
                status.set(None);
            }
            Err(e) => status.set(Some((e.to_string(), false))),
        }
    };

    // Open the file named in the URL, if any.
    use_effect({
        let pkgbase = pkgbase.clone();
        let initial_path = initial_path.clone();
        move || {
            if let Some(path) = initial_path.clone()
                && selected.peek().is_none()
            {
                let pkgbase = pkgbase.clone();
                spawn(async move { open_file(pkgbase, path).await });
            }
        }
    });

    let dirty = loaded.read().as_ref().is_some_and(|c| {
        let shown = c
            .patched_content
            .clone()
            .unwrap_or_else(|| c.original_content.clone());
        draft() != shown
    });
    let patched = loaded
        .read()
        .as_ref()
        .is_some_and(|c| c.patched_content.is_some());

    rsx! {
        div { class: "flex gap-4",
            // File list
            div { class: "card bg-base-100 shadow-xl w-72 shrink-0",
                div { class: "card-body p-4",
                    h2 { class: "card-title text-base", "{pkgbase}" }
                    match &*files.read_unchecked() {
                        None => rsx! { span { class: "loading loading-spinner loading-sm" } },
                        Some(Err(e)) => rsx! { div { class: "alert alert-error text-xs", "{e}" } },
                        Some(Ok(list)) => rsx! {
                            ul { class: "menu menu-sm p-0",
                                for path in list.iter() {
                                    li { key: "{path}",
                                        a {
                                            class: if selected().as_deref() == Some(path.as_str()) { "active font-mono" } else { "font-mono" },
                                            onclick: {
                                                let pkgbase = pkgbase.clone();
                                                let path = path.clone();
                                                move |_| open_file(pkgbase.clone(), path.clone())
                                            },
                                            "{path}"
                                        }
                                    }
                                }
                            }
                        },
                    }
                }
            }

            // Editor
            div { class: "card bg-base-100 shadow-xl flex-1",
                div { class: "card-body",
                    match selected() {
                        None => rsx! { p { class: "opacity-60", "Select a file to edit." } },
                        Some(path) => rsx! {
                            div { class: "flex items-center gap-2",
                                h3 { class: "font-mono font-medium", "{path}" }
                                if patched { span { class: "badge badge-warning badge-sm", "patched" } }
                                if dirty { span { class: "badge badge-info badge-sm", "unsaved" } }
                                div { class: "flex-1" }
                                button {
                                    class: "btn btn-ghost btn-sm",
                                    disabled: !dirty,
                                    onclick: move |_| {
                                        if let Some(c) = loaded.read().as_ref() {
                                            draft.set(c.original_content.clone());
                                        }
                                    },
                                    "Revert to upstream"
                                }
                                button {
                                    class: "btn btn-primary btn-sm",
                                    disabled: !dirty,
                                    onclick: {
                                        let pkgbase = pkgbase.clone();
                                        let path = path.clone();
                                        move |_| {
                                            let pkgbase = pkgbase.clone();
                                            let path = path.clone();
                                            async move {
                                                let Ok(client) = AurCacheClient::new(api_base(), None) else { return };
                                                match client.put_source_file(&pkgbase, &path, &draft()).await {
                                                    Ok(()) => status.set(Some(("Saved. The patch was updated.".into(), true))),
                                                    Err(e) => status.set(Some((e.to_string(), false))),
                                                }
                                            }
                                        }
                                    },
                                    "Save"
                                }
                            }

                            // A patch that no longer applies is shown, not hidden:
                            // the editor is falling back to pristine content and the
                            // user needs to know before they save over it.
                            if let Some(err) = loaded.read().as_ref().and_then(|c| c.patch_error.clone()) {
                                div { class: "alert alert-warning text-sm",
                                    span { "Stored patch no longer applies: {err}. Showing upstream content." }
                                }
                            }
                            if let Some((msg, ok)) = status() {
                                div {
                                    class: if ok { "alert alert-success text-sm" } else { "alert alert-error text-sm" },
                                    span { "{msg}" }
                                }
                            }

                            textarea {
                                class: "textarea textarea-bordered font-mono text-xs w-full h-[60vh] leading-snug",
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
