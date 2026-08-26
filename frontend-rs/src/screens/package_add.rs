//! Adding a package.
//!
//! A dialog over the package list rather than a page of its own: adding a
//! package is a small task, and coming back to a list that never went away is
//! less disorienting than coming back to one that reloaded. It still has a URL,
//! so it can be linked to and Back closes it.
//!
//! One step, not the three the Dart version used. Choosing a source, naming it
//! and picking architectures are not stages of anything — they are three fields,
//! and two of them usually keep their defaults.

use crate::platforms::{self, PlatformChecklist};
use crate::routes::Route;
use aurcache_client::{AddPackageRequest, GitSourceSpec, SearchResult, SourceData};
use dioxus::prelude::*;
use std::time::Duration;

/// How long to wait after the last keystroke before searching.
///
/// The search proxies to the AUR, so every keystroke sent straight through is a
/// request someone else pays for.
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(300);

/// Shorter than this and the result set is the whole AUR.
const MIN_QUERY: usize = 3;

/// Which kind of source is being added.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SourceKind {
    Aur,
    Git,
}

/// The git source these fields describe, or `None` if there is not one yet.
///
/// Separate from the form so the rules are testable: a URL is the only required
/// field, and everything is trimmed, because a trailing space pasted along with
/// a URL should not become part of it.
fn git_source(url: &str, git_ref: &str, subfolder: &str) -> Option<SourceData> {
    let url = url.trim();
    if url.is_empty() {
        return None;
    }
    Some(SourceData::Git {
        spec: GitSourceSpec {
            url: url.to_string(),
            // Blank means "the usual one" to a person, but the server wants an
            // actual ref. `master` is the same fallback the Dart form used.
            r#ref: match git_ref.trim() {
                "" => "master".to_string(),
                given => given.to_string(),
            },
            subfolder: subfolder.trim().to_string(),
        },
    })
}

#[component]
pub fn PackageAdd() -> Element {
    // The list stays mounted behind the dialog, so dismissing it reveals the
    // page as it was rather than reloading it.
    rsx! {
        super::Packages {}
        AddPackageDialog {}
    }
}

#[component]
fn AddPackageDialog() -> Element {
    let mut kind = use_signal(|| SourceKind::Aur);

    // AUR
    let mut query = use_signal(String::new);
    let mut debounced = use_signal(String::new);
    let mut chosen = use_signal(|| Option::<String>::None);

    // Git
    let mut git_url = use_signal(String::new);
    let mut git_ref = use_signal(|| "master".to_string());
    let mut git_subfolder = use_signal(String::new);

    let mut selected = use_signal(|| vec![platforms::DEFAULT.to_string()]);
    let mut busy = use_signal(|| false);
    let mut error = use_signal(|| Option::<String>::None);

    let results = use_resource(move || async move {
        let q = debounced();
        if q.chars().count() < MIN_QUERY {
            return Ok(Vec::new());
        }
        crate::api::client()?
            .search(&q)
            .await
            .map_err(|e| e.to_string())
    });

    let close = move |_| {
        navigator().push(Route::Packages {});
    };

    // What the fields currently describe, or `None` while they are incomplete.
    // Deriving this rather than tracking a separate "can add" flag keeps the
    // button's enabled state and the request from ever disagreeing.
    let source = move || -> Option<SourceData> {
        match kind() {
            SourceKind::Aur => chosen().map(|name| SourceData::Aur { name }),
            SourceKind::Git => git_source(&git_url(), &git_ref(), &git_subfolder()),
        }
    };

    let ready = source().is_some() && !selected().is_empty() && !busy();

    let submit = move |_| async move {
        let Some(source) = source() else { return };
        busy.set(true);
        error.set(None);

        let outcome = match crate::api::client() {
            Err(e) => Err(e),
            Ok(client) => client
                .add_package(&AddPackageRequest {
                    platforms: Some(selected()),
                    build_flags: None,
                    source,
                    patched_files: None,
                })
                .await
                .map_err(|e| e.to_string()),
        };
        busy.set(false);

        match outcome {
            // The list behind the dialog is where the new package appears, and
            // closing is what refetches it.
            Ok(()) => {
                navigator().push(Route::Packages {});
            }
            // Staying open keeps the typed source in reach: a name the AUR does
            // not have, or a git URL that cannot be cloned, is usually a typo
            // rather than a reason to start over.
            Err(e) => error.set(Some(e)),
        }
    };

    rsx! {
        div { class: "modal modal-open",
            div { class: "modal-box max-w-2xl",
                h3 { class: "font-bold text-lg", "Add package" }

                div { role: "tablist", class: "tabs tabs-bordered mt-3",
                    button {
                        role: "tab",
                        class: if kind() == SourceKind::Aur { "tab tab-active" } else { "tab" },
                        onclick: move |_| kind.set(SourceKind::Aur),
                        "AUR"
                    }
                    button {
                        role: "tab",
                        class: if kind() == SourceKind::Git { "tab tab-active" } else { "tab" },
                        onclick: move |_| kind.set(SourceKind::Git),
                        "Git"
                    }
                }

                div { class: "py-4 space-y-3",
                    match kind() {
                        SourceKind::Aur => rsx! {
                            input {
                                r#type: "search",
                                class: "input input-bordered w-full",
                                placeholder: "Search the AUR…",
                                autofocus: true,
                                value: "{query}",
                                oninput: move |e| {
                                    let typed = e.value();
                                    query.set(typed.clone());
                                    // Each keystroke starts its own timer and
                                    // then checks whether it is still the
                                    // latest; the stale ones fall through
                                    // without touching anything.
                                    spawn(async move {
                                        gloo_timers::future::sleep(SEARCH_DEBOUNCE).await;
                                        if query() == typed {
                                            debounced.set(typed);
                                        }
                                    });
                                },
                            }
                            SearchResults {
                                query: query(),
                                // Cloned out of the resource so the borrow ends here rather
                                // than living as long as the child's props.
                                results: results.read_unchecked().as_ref().cloned(),
                                chosen: chosen(),
                                onpick: move |name| chosen.set(Some(name)),
                            }
                        },
                        SourceKind::Git => rsx! {
                            label { class: "form-control w-full",
                                span { class: "label-text text-sm", "Repository URL" }
                                input {
                                    r#type: "url",
                                    class: "input input-bordered w-full font-mono text-sm",
                                    placeholder: "https://github.com/user/repo.git",
                                    value: "{git_url}",
                                    oninput: move |e| git_url.set(e.value()),
                                }
                            }
                            div { class: "flex gap-3 flex-wrap",
                                label { class: "form-control flex-1 min-w-40",
                                    span { class: "label-text text-sm", "Ref" }
                                    input {
                                        r#type: "text",
                                        class: "input input-bordered w-full font-mono text-sm",
                                        placeholder: "master",
                                        value: "{git_ref}",
                                        oninput: move |e| git_ref.set(e.value()),
                                    }
                                }
                                label { class: "form-control flex-1 min-w-40",
                                    span { class: "label-text text-sm", "Subfolder" }
                                    input {
                                        r#type: "text",
                                        class: "input input-bordered w-full font-mono text-sm",
                                        placeholder: "repository root",
                                        value: "{git_subfolder}",
                                        oninput: move |e| git_subfolder.set(e.value()),
                                    }
                                }
                            }
                        },
                    }

                    div {
                        span { class: "label-text text-sm", "Platforms" }
                        PlatformChecklist {
                            selected: selected(),
                            onchange: move |next| selected.set(next),
                            size: "checkbox-sm",
                        }
                        // Whether a package builds for an architecture is the
                        // PKGBUILD's business, not ours, so this offers rather
                        // than promises.
                        p { class: "text-xs opacity-50 pt-1",
                            "Which of these actually work depends on the package's PKGBUILD."
                        }
                    }

                    if let Some(message) = error() {
                        div { class: "alert alert-error text-sm", span { "{message}" } }
                    }
                }

                div { class: "modal-action",
                    button { class: "btn btn-ghost", onclick: close, "Cancel" }
                    button {
                        class: "btn btn-primary",
                        disabled: !ready,
                        onclick: submit,
                        if busy() {
                            span { class: "loading loading-spinner loading-xs" }
                        }
                        "Add"
                    }
                }
            }
            // Clicking away closes, which is what a dimmed backdrop implies.
            // No colour of its own: `.modal` already dims the page behind it,
            // and a second translucent layer on top darkened it twice.
            div { class: "modal-backdrop", onclick: close }
        }
    }
}

/// The AUR search results, and the reasons there might not be any.
///
/// "No matches" and "keep typing" look the same — an empty list — and telling
/// someone their package does not exist when they have typed two letters is
/// worse than saying nothing.
#[component]
fn SearchResults(
    query: String,
    results: Option<Result<Vec<SearchResult>, String>>,
    chosen: Option<String>,
    onpick: EventHandler<String>,
) -> Element {
    if query.chars().count() < MIN_QUERY {
        return rsx! {
            p { class: "text-sm opacity-60 py-2", "Type at least {MIN_QUERY} characters to search." }
        };
    }

    match results {
        None => rsx! {
            div { class: "py-2", span { class: "loading loading-spinner loading-sm" } }
        },
        Some(Err(e)) => rsx! {
            div { class: "alert alert-error text-sm", span { "Search failed: {e}" } }
        },
        Some(Ok(found)) if found.is_empty() => rsx! {
            p { class: "text-sm opacity-60 py-2", "Nothing in the AUR matches “{query}”." }
        },
        Some(Ok(found)) => rsx! {
            ul { class: "menu menu-sm p-0 max-h-64 overflow-y-auto border border-base-300 rounded-box",
                for result in found {
                    li { key: "{result.name}",
                        button {
                            class: if chosen.as_deref() == Some(result.name.as_str()) { "active" } else { "" },
                            onclick: {
                                let name = result.name.clone();
                                move |_| onpick.call(name.clone())
                            },
                            span { class: "font-mono", "{result.name}" }
                            span { class: "opacity-50 text-xs", "{result.version}" }
                        }
                    }
                }
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{SearchResults, git_source};
    use aurcache_client::{SearchResult, SourceData};
    use dioxus::prelude::*;

    #[component]
    fn Harness(query: String, results: Option<Result<Vec<SearchResult>, String>>) -> Element {
        rsx! {
            SearchResults { query, results, chosen: None, onpick: move |_| {} }
        }
    }

    fn render(query: &str, results: Option<Result<Vec<SearchResult>, String>>) -> String {
        let mut dom = VirtualDom::new_with_props(
            Harness,
            HarnessProps {
                query: query.to_string(),
                results,
            },
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    /// Two letters is not a failed search, it is an unfinished one. Reporting
    /// "nothing matches" there tells someone their package does not exist when
    /// nothing has been looked up yet.
    #[test]
    fn a_short_query_is_not_a_failed_search() {
        let html = render("he", Some(Ok(Vec::new())));
        assert!(html.contains("Type at least"), "{html}");
        assert!(!html.contains("Nothing in the AUR"), "{html}");
    }

    /// Once the query is long enough, an empty result really is an answer.
    #[test]
    fn a_long_query_with_no_results_says_so() {
        let html = render("nonesuch", Some(Ok(Vec::new())));
        assert!(html.contains("Nothing in the AUR"), "{html}");
    }

    /// A search that is still running must not look like one that came back
    /// empty; the two are a spinner and a verdict.
    #[test]
    fn a_pending_search_shows_no_verdict() {
        let html = render("hello", None);
        assert!(html.contains("loading"), "{html}");
        assert!(!html.contains("Nothing in the AUR"), "{html}");
    }

    #[test]
    fn results_are_listed_with_their_versions() {
        let html = render(
            "hello",
            Some(Ok(vec![SearchResult {
                name: "hello-world".to_string(),
                version: "1.0-3".to_string(),
            }])),
        );
        assert!(html.contains("hello-world"), "{html}");
        assert!(html.contains("1.0-3"), "{html}");
    }

    #[test]
    fn a_git_source_needs_only_a_url() {
        assert!(git_source("", "", "").is_none());
        assert!(git_source("   ", "", "").is_none());
        assert!(git_source("https://example.invalid/r.git", "", "").is_some());
    }

    /// The server wants a ref, and a blank field means "the usual one" rather
    /// than "no ref".
    #[test]
    fn a_blank_ref_falls_back_to_master() {
        let SourceData::Git { spec } = git_source("https://e.invalid/r.git", "  ", "").unwrap()
        else {
            panic!("expected a git source");
        };
        assert_eq!(spec.r#ref, "master");
    }

    /// A URL is usually pasted, and a pasted URL usually brings whitespace.
    #[test]
    fn fields_are_trimmed() {
        let SourceData::Git { spec } =
            git_source(" https://e.invalid/r.git ", " v1.2 ", " pkg ").unwrap()
        else {
            panic!("expected a git source");
        };
        assert_eq!(spec.url, "https://e.invalid/r.git");
        assert_eq!(spec.r#ref, "v1.2");
        assert_eq!(spec.subfolder, "pkg");
    }

    /// The shape the server parses. `GitSourceSpec` is flattened into the
    /// tagged enum, so its fields sit beside `type` rather than under a `spec`
    /// key — a nesting mistake here is a 422 with no other symptom.
    #[test]
    fn a_git_source_serialises_the_way_the_server_reads_it() {
        let source = git_source("https://e.invalid/r.git", "main", "sub").unwrap();
        let json = serde_json::to_value(&source).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "type": "git",
                "url": "https://e.invalid/r.git",
                "ref": "main",
                "subfolder": "sub",
            })
        );
    }

    #[test]
    fn an_aur_source_serialises_as_a_name() {
        let json = serde_json::to_value(SourceData::Aur {
            name: "hello".to_string(),
        })
        .unwrap();
        assert_eq!(json, serde_json::json!({ "type": "aur", "name": "hello" }));
    }

    /// A failed lookup is not an empty one — the AUR being unreachable must not
    /// read as "your package does not exist".
    #[test]
    fn a_failed_search_reports_the_failure() {
        let html = render("hello", Some(Err("connection refused".to_string())));
        assert!(html.contains("connection refused"), "{html}");
        assert!(!html.contains("Nothing in the AUR"), "{html}");
    }
}
