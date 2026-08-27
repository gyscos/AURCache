//! Settings that belong to one package rather than to the server.
//!
//! Everything here is an override: the package either holds its own value or
//! inherits the server-wide one, which is why each control says which of the
//! two is in force. The page is laid out like the source editor — the same
//! header card, the same trail — so moving between a package's sub-pages does
//! not move the furniture.
//!
//! What is *not* here is as deliberate as what is. The CPU limit, the memory
//! limit and the job timeout can all be stored against a package, and the Dart
//! UI offered them, but nothing reads them back per package: `JobDescriptor`
//! (`aurcache-types/src/worker.rs`) carries no limits, and a worker takes them
//! from its own environment (`aurcache-worker-docker/src/config.rs`). Offering
//! them would store a number that changes nothing.

use crate::api::client;
use crate::routes::Route;
use crate::screens::PackageHeader;
use crate::screens::config_files::ConfigFileTabs;
use crate::screens::settings::Section;
use aurcache_client::PatchPackageRequest;
use dioxus::prelude::*;

/// The screen behind `/package/:pkgbase/settings`.
#[component]
pub fn PackageSettings(pkgbase: String) -> Element {
    let mut package = use_resource({
        let pkgbase = pkgbase.clone();
        move || {
            let pkgbase = pkgbase.clone();
            async move {
                client()?
                    .get_package(&pkgbase)
                    .await
                    .map_err(|e| e.to_string())
            }
        }
    });

    rsx! {
        div { class: "space-y-4",
            match &*package.read_unchecked() {
                Some(Ok(pkg)) => rsx! {
                    PackageHeader {
                        pkg: pkg.clone(),
                        trail: vec![("Settings".to_string(), None)],
                    }
                    Section { title: "Build configuration",
                        crate::screens::package::PlatformField {
                            pkgbase: pkg.name.clone(),
                            selected: pkg.selected_platforms.clone(),
                            on_changed: move |()| package.restart(),
                        }
                        BuildFlagsField {
                            pkgbase: pkg.name.clone(),
                            // A package with no flags stores an empty string,
                            // which the API splits on `;` into one empty flag
                            // rather than none. Left in, it renders as a chip
                            // with no label that cannot be told apart from a
                            // real one.
                            flags: pkg
                                .selected_build_flags
                                .clone()
                                .unwrap_or_default()
                                .into_iter()
                                .filter(|flag| !flag.trim().is_empty())
                                .collect(),
                            on_changed: move |()| package.restart(),
                        }
                    }
                },
                Some(Err(e)) => rsx! {
                    div { class: "alert alert-error", span { "Could not load {pkgbase}: {e}" } }
                },
                None => rsx! {
                    div { class: "flex justify-center p-8",
                        span { class: "loading loading-spinner loading-lg" }
                    }
                },
            }

            // Below the fetch rather than inside it: these two need only the
            // name, which came from the URL, so a package that fails to load
            // still leaves its config files editable and itself removable.
            Section { title: "Build environment",
                p { class: "text-xs opacity-60 max-w-prose",
                    "Files this package builds against. Left unset, each inherits the "
                    Link { class: "link link-primary", to: Route::ConfigFiles {},
                        "server-wide file"
                    }
                    "."
                }
                ConfigFileTabs { pkgbase: Some(pkgbase.clone()) }
            }

            RemoveSection { pkgbase }
        }
    }
}

/// The makepkg flags this package builds with, as chips.
///
/// Free-form rather than a fixed set: they are passed to makepkg, which has far
/// more of them than a checklist would be honest about. Every edit saves the
/// whole list, because that is what the endpoint takes — there is no
/// add-one/remove-one operation to mirror.
#[component]
fn BuildFlagsField(pkgbase: String, flags: Vec<String>, on_changed: EventHandler<()>) -> Element {
    let mut draft = use_signal(String::new);
    let mut busy = use_signal(|| false);
    let mut error = use_signal(|| Option::<String>::None);

    let pkgbase = use_signal(|| pkgbase);
    let current = use_signal(|| flags.clone());
    // Follow the server after a save, or the chips show the list as it was
    // before the change that was just made.
    use_effect(use_reactive(&flags, move |flags| {
        let mut current = current;
        current.set(flags);
    }));

    let save = move |next: Vec<String>| async move {
        busy.set(true);
        error.set(None);
        let outcome = match client() {
            Ok(client) => client
                .patch_package(
                    &pkgbase(),
                    &PatchPackageRequest {
                        build_flags: Some(next),
                        ..Default::default()
                    },
                )
                .await
                .map_err(|e| e.to_string()),
            Err(e) => Err(e),
        };
        busy.set(false);
        match outcome {
            Ok(()) => {
                draft.set(String::new());
                on_changed.call(());
            }
            Err(e) => error.set(Some(e)),
        }
    };

    // Adding a flag already present would save a list with a duplicate in it,
    // which makepkg would then see twice.
    let entered = draft().trim().to_string();
    let can_add = !entered.is_empty() && !current().contains(&entered) && !busy();

    let add = move |()| async move {
        let entered = draft().trim().to_string();
        if entered.is_empty() || current().contains(&entered) {
            return;
        }
        let mut next = current();
        next.push(entered);
        save(next).await;
    };

    rsx! {
        div { class: "flex gap-2 py-1 text-sm",
            span { class: "opacity-60 w-24 shrink-0", "Flags" }
            div { class: "min-w-0 flex-1 flex flex-col gap-2",
                if current().is_empty() {
                    span { class: "opacity-60 text-xs italic",
                        "No build flags. makepkg runs with its own defaults."
                    }
                } else {
                    div { class: "flex flex-wrap gap-1",
                        for flag in current() {
                            span {
                                key: "{flag}",
                                class: "badge badge-outline gap-1 font-mono text-xs",
                                "{flag}"
                                button {
                                    class: "opacity-60 hover:opacity-100",
                                    disabled: busy(),
                                    aria_label: "Remove {flag}",
                                    onclick: {
                                        let flag = flag.clone();
                                        move |_| {
                                            let flag = flag.clone();
                                            async move {
                                                let next = current()
                                                    .into_iter()
                                                    .filter(|f| *f != flag)
                                                    .collect();
                                                save(next).await;
                                            }
                                        }
                                    },
                                    "✕"
                                }
                            }
                        }
                    }
                }

                div { class: "flex gap-2",
                    input {
                        r#type: "text",
                        class: "input input-bordered input-xs font-mono w-48",
                        placeholder: "--nocheck",
                        value: "{draft}",
                        disabled: busy(),
                        oninput: move |e| draft.set(e.value()),
                        onkeydown: move |e: KeyboardEvent| async move {
                            if e.key() == Key::Enter {
                                add(()).await;
                            }
                        },
                    }
                    button {
                        class: "btn btn-xs",
                        disabled: !can_add,
                        onclick: move |_| add(()),
                        "Add"
                    }
                }

                if let Some(message) = error() {
                    span { class: "text-xs text-error", "{message}" }
                }
            }
        }
    }
}

/// Removing the package from the repository.
///
/// "Remove" rather than "delete" because that is what the server does: it
/// clears the direct-request flag and then live-checks. A package nothing
/// depends on is deleted along with any dependency that was only there for it;
/// one that something still needs stays, demoted to a dependency. Saying
/// "delete" would promise the first case in a UI that cannot tell which applies
/// until it has happened.
#[component]
fn RemoveSection(pkgbase: String) -> Element {
    let mut confirming = use_signal(|| false);
    let mut busy = use_signal(|| false);
    let mut error = use_signal(|| Option::<String>::None);
    let pkgbase = use_signal(|| pkgbase);

    let remove = move |_| async move {
        busy.set(true);
        error.set(None);
        let outcome = match client() {
            Ok(client) => client
                .delete_package(&pkgbase())
                .await
                .map_err(|e| e.to_string()),
            Err(e) => Err(e),
        };
        busy.set(false);
        match outcome {
            Ok(()) => {
                confirming.set(false);
                // The package may no longer exist, so going back to it would
                // land on an error page.
                navigator().push(Route::Packages { q: String::new() });
            }
            Err(e) => error.set(Some(e)),
        }
    };

    rsx! {
        div { class: "card bg-base-100 shadow-xl border border-error/30",
            div { class: "card-body",
                h2 { class: "card-title text-base text-error", "Remove" }
                p { class: "text-xs opacity-60 max-w-prose",
                    "Marks this package as no longer requested. If nothing depends on it, "
                    "it and its builds are deleted, along with any dependency that was only "
                    "installed for it. If something still depends on it, it stays as a "
                    "dependency."
                }
                if let Some(message) = error() {
                    div { class: "alert alert-error text-sm", span { "{message}" } }
                }
                div {
                    button {
                        class: "btn btn-error btn-sm btn-outline",
                        onclick: move |_| confirming.set(true),
                        "Remove package"
                    }
                }
            }
        }

        div {
            class: if confirming() { "modal modal-open" } else { "modal" },
            role: "dialog",
            aria_modal: "true",
            aria_label: "Confirm removal",
            div { class: "modal-box",
                h3 { class: "font-bold text-lg", "Remove {pkgbase}?" }
                p { class: "text-sm opacity-70 pt-2",
                    "It stops being a requested package. Unless something depends on it, "
                    "it and its build history are deleted, and so is anything that was only "
                    "here as its dependency. This cannot be undone."
                }
                div { class: "modal-action",
                    button {
                        class: "btn btn-sm",
                        disabled: busy(),
                        onclick: move |_| confirming.set(false),
                        "Cancel"
                    }
                    button {
                        class: "btn btn-error btn-sm",
                        disabled: busy(),
                        onclick: remove,
                        if busy() {
                            span { class: "loading loading-spinner loading-xs" }
                        }
                        "Remove"
                    }
                }
            }
            button {
                class: "modal-backdrop",
                disabled: busy(),
                onclick: move |_| confirming.set(false),
                aria_label: "Cancel removal",
                "Close"
            }
        }
    }
}
