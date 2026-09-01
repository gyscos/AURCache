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
//!
//! And one field for the source, not a pair of tabs: `looks_like_git_url` tells
//! a remote from a package name, the same way `aurcache-cli packages add` does,
//! so nobody has to say which kind of thing they are about to paste.
//!
//! Several packages can be queued before committing, all onto the same
//! platforms. Adding a handful at once is the normal case when setting a server
//! up, and the platforms are almost always the same for all of them — asking
//! once beats reopening the dialog per package and re-picking them each time.

use crate::platforms::{self, PlatformChecklist};
use crate::routes::Route;
use aurcache_client::{
    AddPackageRequest, AddPackagesRequest, BulkAddOutcome, GitSourceSpec, SearchResult, SourceData,
    looks_like_git_url,
};
use dioxus::prelude::*;
use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

/// How long to wait after the last keystroke before searching.
///
/// The search proxies to the AUR, so every keystroke sent straight through is a
/// request someone else pays for.
/// How often to ask a running bulk add what it has done.
///
/// A package takes seconds to add -- a checkout each -- so polling faster than
/// this only produces identical answers.
const BULK_POLL_INTERVAL: Duration = Duration::from_millis(700);

const SEARCH_DEBOUNCE: Duration = Duration::from_millis(300);

/// How long to wait before looking up a one- or two-character entry.
///
/// Such a query is answered by an exact name lookup, which is only meaningful
/// if that *is* the whole name — passing through `h` and `he` on the way to
/// `hello` asks two questions nobody wanted the answer to. Longer than
/// [`SEARCH_DEBOUNCE`] because there is no incremental feedback to lose: the
/// answer is one package or none, and it still arrives promptly once typing
/// stops. Short names stay addable; they just settle a beat later.
const EXACT_LOOKUP_DEBOUNCE: Duration = Duration::from_millis(900);

/// Up to this many bytes, the server looks the name up exactly instead of
/// searching — see `aurcache_api::aur::search`, which switches on the same
/// number. A substring search for `a` would match most of the AUR; an exact
/// lookup for it finds the package genuinely called `a`, which exists, as does
/// `zz`. Short names are addable because of this, so nothing here may gate them
/// out.
///
/// Bytes rather than characters, to switch where the server switches: the two
/// agree for every package name the AUR allows, and disagreeing about a
/// mistyped multi-byte query would only mislabel the "no results" wording.
const EXACT_LOOKUP_MAX: usize = 2;

/// What pressing Add will actually add.
///
/// The queue wins when there is one: the field is a staging area for it, and
/// something half-typed there should not ride along with a list that was
/// assembled deliberately. With nothing queued, the field *is* the request —
/// typing a name and pressing Add, or Enter, adds it without a detour through
/// the queue.
fn to_add(queued: Vec<SourceData>, pending: Option<SourceData>) -> Vec<SourceData> {
    if queued.is_empty() {
        pending.into_iter().collect()
    } else {
        queued
    }
}

/// How many completed searches to keep for narrowing.
///
/// Bounded because someone exploring tries many unrelated queries in one
/// sitting, and a broad result set is not small -- `hel` alone is a few
/// thousand packages. Sixteen covers the back-and-forth of refining a search
/// while letting the ones before it go.
const SEARCH_CACHE_ENTRIES: usize = 16;

/// Completed AUR searches, newest first, used to answer later queries locally.
///
/// Only substring searches go in here. A query of one or two characters is
/// answered by an *exact name lookup* server-side, whose single result is not
/// the set of everything containing those characters -- narrowing from it would
/// claim almost nothing matches.
#[derive(Default)]
struct SearchCache(Vec<(String, Vec<SearchResult>)>);

impl SearchCache {
    /// The results for `query`, if any cached search can answer it without the
    /// network.
    ///
    /// The AUR searches `by=name-desc`, a substring match, so anything matching
    /// a longer query also matched a shorter prefix of it: a cached search for
    /// `hel` contains every result `hello` could have. The longest usable
    /// prefix is chosen because it is the smallest set to filter.
    ///
    /// This is why a successful response has to be a *complete* one. It is:
    /// aurweb answers an over-broad search with `Too many package results.`
    /// rather than a truncated list, and `aurcache-deps` turns that into an
    /// error, so a set we hold is never a partial one.
    fn narrow(&self, query: &str) -> Option<Vec<SearchResult>> {
        let query = query.trim().to_lowercase();
        let (_, results) = self
            .0
            .iter()
            .filter(|(cached, _)| query.starts_with(cached))
            .max_by_key(|(cached, _)| cached.len())?;
        Some(
            results
                .iter()
                .filter(|result| matches_query(result, &query))
                .cloned()
                .collect(),
        )
    }

    /// Record a completed substring search.
    ///
    /// Insertion never rewrites an existing entry, so narrowing is
    /// non-destructive: typing on past `hel` and deleting back to it filters
    /// the original `hel` results again rather than a narrowed remnant of them.
    fn insert(&mut self, query: &str, results: &[SearchResult]) {
        let query = query.trim().to_lowercase();
        if query.len() <= EXACT_LOOKUP_MAX {
            return;
        }
        self.0.retain(|(cached, _)| cached != &query);
        self.0.insert(0, (query, results.to_vec()));
        self.0.truncate(SEARCH_CACHE_ENTRIES);
    }
}

/// Whether a result matches `query` the way the AUR's `by=name-desc` does:
/// a case-insensitive substring of either the name or the description.
fn matches_query(result: &SearchResult, query: &str) -> bool {
    result.name.to_lowercase().contains(query)
        || result
            .description
            .as_deref()
            .is_some_and(|d| d.to_lowercase().contains(query))
}

/// Add a list of sources in one request, following the job until it ends.
///
/// One request rather than one per package: the server resolves every AUR name
/// to its pkgbase in a single batched RPC call, where adding them one at a time
/// spent an AUR request per package before fetching anything.
///
/// Returns the failures and the sources that produced them, so the queue can
/// keep exactly what still needs adding. Watching is optional as far as the
/// server is concerned — the job runs whether or not this keeps polling — but
/// the dialog stays open, so it does.
async fn submit_bulk(
    client: &aurcache_client::AurCacheClient,
    sources: Vec<SourceData>,
    platforms: Vec<String>,
    mut added: Signal<Vec<String>>,
    mut adding: Signal<Option<String>>,
) -> (Vec<(String, String)>, Vec<SourceData>) {
    // Kept so a failure can be matched back to the source that caused it: the
    // job reports the name the request carried, which is what `source_label`
    // produces here.
    let by_label: HashMap<String, SourceData> = sources
        .iter()
        .map(|source| (source_label(source), source.clone()))
        .collect();
    // The job works through the list in order, so once N outcomes are in, the
    // one in flight is the N-th. That is what the queue puts a spinner on.
    let labels: Vec<String> = sources.iter().map(source_label).collect();

    let accepted = match client
        .add_packages(&AddPackagesRequest {
            platforms: Some(platforms),
            build_flags: None,
            sources: sources.clone(),
        })
        .await
    {
        Ok(accepted) => accepted,
        // Nothing started, so everything is still to do.
        Err(e) => return (vec![(String::new(), e.to_string())], sources),
    };

    let mut seen = 0_usize;
    let mut failed = Vec::new();
    let mut remaining = Vec::new();
    loop {
        let progress = match client.bulk_add_progress(accepted.job_id, seen).await {
            Ok(progress) => progress,
            // The job is still running; we have merely lost sight of it. Say so
            // rather than reporting packages as failed, which they are not.
            Err(e) => {
                failed.push((String::new(), format!("lost track of the add: {e}")));
                return (failed, remaining);
            }
        };
        seen += progress.entries.len();
        for entry in progress.entries {
            match entry.outcome {
                BulkAddOutcome::Added | BulkAddOutcome::Existed => added.push(entry.name),
                BulkAddOutcome::Failed { error } => {
                    if let Some(source) = by_label.get(&entry.name) {
                        remaining.push(source.clone());
                    }
                    failed.push((entry.name, error));
                }
            }
        }
        if progress.finished {
            return (failed, remaining);
        }
        adding.set(labels.get(seen).cloned());
        gloo_timers::future::sleep(BULK_POLL_INTERVAL).await;
    }
}

/// Add a single source that carries file edits, through the endpoint that
/// accepts them.
async fn submit_patched(
    client: &aurcache_client::AurCacheClient,
    sources: Vec<SourceData>,
    platforms: Vec<String>,
    patched: BTreeMap<String, String>,
    mut added: Signal<Vec<String>>,
) -> (Vec<(String, String)>, Vec<SourceData>) {
    let mut failed = Vec::new();
    let mut remaining = Vec::new();
    for source in sources {
        let label = source_label(&source);
        match client
            .add_package(&AddPackageRequest {
                platforms: Some(platforms.clone()),
                build_flags: None,
                source: source.clone(),
                patched_files: Some(patched.clone()),
            })
            .await
        {
            Ok(()) => added.push(label),
            Err(e) => {
                failed.push((label, e.to_string()));
                remaining.push(source);
            }
        }
    }
    (failed, remaining)
}

/// Orders search results by how well they answer what was typed.
///
/// The AUR's own ordering is not by relevance: searching `hello` returns
/// `howdy-git`, `biopass-bin`, `wsl-hello-sudo-bin` and four more before
/// `hello` itself appears. The package someone typed the exact name of is the
/// one they meant, so it goes first, then the ones that start with what they
/// typed, then everything else.
///
/// Stable within each rank, so the AUR's own order — which does carry some
/// popularity signal — survives among equally good matches.
fn rank_results(query: &str, results: &mut [SearchResult]) {
    let query = query.trim().to_lowercase();
    results.sort_by_key(|result| {
        let name = result.name.to_lowercase();
        if name == query {
            0
        } else if name.starts_with(&query) {
            1
        } else if name.contains(&query) {
            2
        } else {
            // Matched on something other than the name — the AUR searches
            // descriptions too — so it is the least likely to be what was meant.
            3
        }
    });
}

/// The source the form currently describes, or `None` while it is empty.
///
/// One entry field decides its own kind: anything shaped like a git remote is
/// one, and everything else is an AUR package name. That is the same call
/// `aurcache-cli packages add` makes, from the same function, so the two cannot
/// disagree about what a given string means.
///
/// An AUR name is taken as typed rather than requiring a search result to be
/// clicked — the list is a convenience, and someone who already knows the name
/// should not have to wait for the AUR to confirm it.
fn source_for(entry: &str, git_ref: &str, subfolder: &str) -> Option<SourceData> {
    let entry = entry.trim();
    if looks_like_git_url(entry) {
        return git_source(entry, git_ref, subfolder);
    }
    if entry.is_empty() {
        return None;
    }
    Some(SourceData::Aur {
        name: entry.to_string(),
    })
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

/// A source waiting to be added, and how to write it on a chip.
///
/// The label is derived rather than stored: two chips with the same label are
/// the same source, which is what the duplicate check relies on.
fn source_label(source: &SourceData) -> String {
    match source {
        SourceData::Aur { name } => name.clone(),
        SourceData::Git { spec } => {
            let mut label = spec.url.clone();
            if !spec.r#ref.is_empty() {
                label.push('#');
                label.push_str(&spec.r#ref);
            }
            if !spec.subfolder.is_empty() {
                label.push('/');
                label.push_str(&spec.subfolder);
            }
            label
        }
        // This dialog never builds one — the upload it belongs to was never
        // implemented server-side — but the variant exists, so it gets a label
        // rather than a panic.
        SourceData::Upload { .. } => "uploaded archive".to_string(),
    }
}

/// The pkgbase a source will land under, for comparing against what is already
/// on the server.
///
/// Only an AUR source has one to predict. A git remote's pkgbase comes from the
/// PKGBUILD inside it, which nothing here has read, so a duplicate git URL is
/// left to the server to refuse.
fn source_at(source: &SourceData) -> Option<&str> {
    match source {
        SourceData::Aur { name } => Some(name),
        SourceData::Git { .. } | SourceData::Upload { .. } => None,
    }
}

#[component]
pub fn PackageAdd(q: String) -> Element {
    // The list stays mounted behind the dialog, so dismissing it reveals the
    // page as it was rather than reloading it. Unfiltered and not syncing the
    // URL: the fragment here is the dialog's search, not the list's filter.
    rsx! {
        super::Packages { q: String::new(), sync_url: false }
        AddPackageDialog { q }
    }
}

#[component]
fn AddPackageDialog(q: String) -> Element {
    // The one field: a package name or a git remote, told apart by their shape.
    // Seeded from `?q=` and written back as it changes, so a search can be
    // linked to — `/packages/add?q=hello` opens with the results already up.
    let mut entry = crate::listing::use_url_search(q.clone(), true, |q| Route::PackageAdd { q });
    // Starts equal to the entry so a URL-seeded search runs immediately, rather
    // than waiting for a keystroke that may never come.
    let mut debounced = use_signal(|| q);
    // Completed searches, so refining a query stops asking the AUR again.
    let mut cache = use_signal(SearchCache::default);

    // Only meaningful once the entry is a remote, which is when they appear.
    let mut git_ref = use_signal(|| "master".to_string());
    let mut git_subfolder = use_signal(String::new);

    // What will be added when the button is pressed.
    let mut queued = use_signal(Vec::<SourceData>::new);
    let mut platforms_selected = use_signal(|| vec![platforms::DEFAULT.to_string()]);
    let mut busy = use_signal(|| false);
    let mut failures = use_signal(Vec::<(String, String)>::new);
    // Which source is in flight, and which are already through. Resolving a
    // package's dependencies can take a while, and adds are sequential, so a
    // single spinner on the button leaves someone watching a list of five with
    // no idea which one is holding things up.
    let mut adding = use_signal(|| Option::<String>::None);
    let mut added = use_signal(Vec::<String>::new);

    // Edits made before the package exists, as whole files rather than a diff:
    // the server diffs each against the pristine source when the add arrives.
    // One package at a time, like `--patch` on the CLI — the paths name files
    // inside a source, and with two sources there is no way to say whose.
    let mut patched = use_signal(BTreeMap::<String, String>::new);
    let mut editing_sources = use_signal(|| false);

    // What the server already has. Adding one again is a silent no-op — the
    // server exits early and returns 200 — which is the worst of both: the
    // dialog closes, nothing happens, and nothing says why. Marking them is how
    // that question gets answered before it is asked.
    let existing = use_resource(|| async move {
        crate::api::client()?
            .list_packages(None, None, false)
            .await
            .map_err(|e| e.to_string())
    });
    let existing_names = move || -> Vec<String> {
        match &*existing.read_unchecked() {
            Some(Ok(list)) => list.iter().map(|p| p.name.clone()).collect(),
            _ => Vec::new(),
        }
    };

    let is_git = move || looks_like_git_url(entry().trim());

    let results = use_resource(move || async move {
        let q = debounced();
        // Nothing to look up for a remote: the AUR does not know about it, and
        // asking would spend a request to be told so. An empty box is not a
        // search either — but one character is, and the server answers it with
        // an exact lookup.
        if q.trim().is_empty() || looks_like_git_url(q.trim()) {
            return Ok(Vec::new());
        }
        // A search already made can answer anything that extends it, with no
        // request and no wait — which is most of typing, since a query grows a
        // character at a time.
        if let Some(mut narrowed) = cache.read().narrow(&q) {
            rank_results(&q, &mut narrowed);
            return Ok(narrowed);
        }
        let mut found = crate::api::client()?
            .search(&q)
            .await
            .map_err(|e| e.to_string())?;
        cache.write().insert(&q, &found);
        rank_results(&q, &mut found);
        Ok(found)
    });

    let close = move |_| {
        navigator().push(Route::Packages { q: String::new() });
    };

    let pending = move || source_for(&entry(), &git_ref(), &git_subfolder());

    // Already queued, or already on the server. Either way there is nothing to
    // add, and offering it again would produce a duplicate or an error.
    let already_taken = move |source: &SourceData| -> bool {
        let label = source_label(source);
        if queued().iter().any(|q| source_label(q) == label) {
            return true;
        }
        source_at(source).is_some_and(|name| existing_names().iter().any(|e| e == name))
    };

    // Queue what the field describes and empty it, ready for the next one.
    let mut enqueue = move || {
        let Some(source) = pending() else { return };
        if already_taken(&source) {
            return;
        }
        queued.push(source);
        // The edits belonged to whatever the field described; queueing a second
        // source means they can no longer be attributed, and a stale PKGBUILD
        // silently attached to the wrong package is worse than losing the edit.
        patched.set(BTreeMap::new());
        editing_sources.set(false);
        entry.set(String::new());
        debounced.set(String::new());
        git_ref.set("master".to_string());
        git_subfolder.set(String::new());
    };

    let sources_to_add = move || to_add(queued(), pending());

    let ready = !sources_to_add().is_empty() && !platforms_selected().is_empty() && !busy();

    // Takes `()` rather than an event, because two different events reach it:
    // a click on Add and Enter in the entry field.
    let submit = move |()| async move {
        let sources = sources_to_add();
        if sources.is_empty() {
            return;
        }
        // Whether the queue is what is being sent decides where a failure goes
        // back to: the queue it came from, or the field it was typed in.
        let from_queue = !queued().is_empty();
        busy.set(true);
        failures.set(Vec::new());
        added.set(Vec::new());
        adding.set(None);

        let client = match crate::api::client() {
            Ok(client) => client,
            Err(e) => {
                failures.set(vec![(String::new(), e)]);
                busy.set(false);
                return;
            }
        };

        // A patched add carries file edits, which the bulk request has nowhere
        // to put — and there is only ever one source when there are edits, so
        // it is a single add by definition.
        let (mut failed, mut remaining) = if patched().is_empty() {
            submit_bulk(&client, sources, platforms_selected(), added, adding).await
        } else {
            submit_patched(&client, sources, platforms_selected(), patched(), added).await
        };
        failed.shrink_to_fit();
        remaining.shrink_to_fit();
        adding.set(None);
        busy.set(false);

        if failed.is_empty() {
            // The list behind the dialog is where the new packages appear, and
            // closing is what refetches it.
            navigator().push(Route::Packages { q: String::new() });
        } else {
            // A failure from the field stays in the field, which still holds
            // it; moving it into the queue would show it twice.
            if from_queue {
                queued.set(remaining);
            }
            failures.set(failed);
        }
    };

    rsx! {
        div { class: "modal modal-open",
            // The editor needs room for two panes; without it the file list
            // and the text would each get half of a narrow column.
            div { class: "modal-box {dialog_width(editing_sources())}",
                h3 { class: "font-bold text-lg", "Add packages" }

                div { class: "py-4 space-y-3",
                    label { class: "form-control w-full",
                        span { class: "label-text text-sm",
                            "AUR package name or git URL"
                        }
                        input {
                            r#type: "text",
                            class: "input input-bordered w-full font-mono text-sm",
                            placeholder: "hello   ·   https://github.com/user/repo.git",
                            autofocus: true,
                            // A run is under way and the queue is fixed for its
                            // duration; typing into a field that no longer
                            // feeds it would be a dead end.
                            disabled: busy(),
                            value: "{entry}",
                            oninput: move |e| {
                                let typed = e.value();
                                entry.set(typed.clone());
                                // Each keystroke starts its own timer and then
                                // checks whether it is still the latest; the
                                // stale ones fall through without touching
                                // anything.
                                spawn(async move {
                                    let wait = if typed.trim().len() <= EXACT_LOOKUP_MAX {
                                        EXACT_LOOKUP_DEBOUNCE
                                    } else {
                                        SEARCH_DEBOUNCE
                                    };
                                    gloo_timers::future::sleep(wait).await;
                                    if entry() == typed {
                                        debounced.set(typed);
                                    }
                                });
                            },
                            // Enter submits. Adding one known package is the
                            // common case by far, and it should not take a
                            // detour through the queue to do it. Building a
                            // queue is done by clicking results, which is how
                            // one gets built anyway.
                            onkeydown: move |e: KeyboardEvent| {
                                if e.key() == Key::Enter {
                                    spawn(submit(()));
                                }
                            },
                        }
                    }

                    if is_git() {
                        // Only asked for once the entry is a remote. An AUR
                        // package has neither, and showing them greyed out on
                        // every add would be two dead fields most of the time.
                        div { class: "flex gap-3 flex-wrap items-end",
                            label { class: "form-control flex-1 min-w-32",
                                span { class: "label-text text-sm", "Ref" }
                                input {
                                    r#type: "text",
                                    class: "input input-bordered w-full font-mono text-sm",
                                    placeholder: "master",
                                    value: "{git_ref}",
                                    oninput: move |e| git_ref.set(e.value()),
                                }
                            }
                            label { class: "form-control flex-1 min-w-32",
                                span { class: "label-text text-sm", "Subfolder" }
                                input {
                                    r#type: "text",
                                    class: "input input-bordered w-full font-mono text-sm",
                                    placeholder: "repository root",
                                    value: "{git_subfolder}",
                                    oninput: move |e| git_subfolder.set(e.value()),
                                }
                            }
                            // A remote has no search results to click, so this
                            // is the only way to queue one.
                            button {
                                class: "btn btn-sm",
                                disabled: pending().is_none_or(|s| already_taken(&s)),
                                onclick: move |_| enqueue(),
                                "Add to list"
                            }
                        }
                    } else {
                        SearchResults {
                            query: entry(),
                            // Cloned out of the resource so the borrow ends
                            // here rather than living as long as the child's
                            // props.
                            results: results.read_unchecked().as_ref().cloned(),
                            taken: {
                                let mut taken = existing_names();
                                taken.extend(queued().iter().map(source_label));
                                taken
                            },
                            frozen: busy(),
                            onpick: move |name: String| {
                                entry.set(name);
                                enqueue();
                            },
                        }
                    }

                    QueuedList {
                        queued: queued(),
                        adding: adding(),
                        added: added(),
                        frozen: busy(),
                        onremove: move |label: String| {
                            queued.retain(|s| source_label(s) != label);
                        },
                    }

                    // Only for a single source, and only before anything is
                    // sent. The paths name files inside one source, so with two
                    // queued there is no way to say which they belong to —
                    // `--patch` on the CLI refuses the same case for the same
                    // reason.
                    if let Some(source) = single_source(&sources_to_add()) {
                        if editing_sources() {
                            AddSourceEditor {
                                source: source.clone(),
                                patched,
                                onclose: move |_| editing_sources.set(false),
                            }
                        } else {
                            div {
                                button {
                                    class: "btn btn-sm btn-ghost",
                                    disabled: busy(),
                                    onclick: move |_| editing_sources.set(true),
                                    if patched().is_empty() {
                                        "Edit sources before adding"
                                    } else {
                                        {edited_label(patched().len())}
                                    }
                                }
                                // The reason this exists at all: a package whose
                                // PKGBUILD does not parse cannot be added and
                                // then fixed, because the add never completes.
                                p { class: "text-xs opacity-50 pt-1",
                                    "Fix a PKGBUILD that does not parse, before it is ever parsed."
                                }
                            }
                        }
                    }

                    div {
                        span { class: "label-text text-sm", "Platforms" }
                        PlatformChecklist {
                            selected: platforms_selected(),
                            onchange: move |next| platforms_selected.set(next),
                            size: "checkbox-sm",
                            // Adds are sequential, so a change part way through
                            // would reach only the packages not yet sent, and
                            // the queue would end up split across two sets.
                            disabled: busy(),
                        }
                        // Whether a package builds for an architecture is the
                        // PKGBUILD's business, not ours, so this offers rather
                        // than promises. They apply to everything queued.
                        p { class: "text-xs opacity-50 pt-1",
                            "Applied to every package above. Which of these actually work depends on each package's PKGBUILD."
                        }
                    }

                    if !failures().is_empty() {
                        div { class: "alert alert-error text-sm flex-col items-start gap-1",
                            span { "Some packages could not be added:" }
                            for (label, message) in failures() {
                                div { key: "{label}", class: "text-xs",
                                    span { class: "font-mono", "{label}" }
                                    " — {message}"
                                }
                            }
                        }
                    }
                }

                div { class: "modal-action",
                    // Leaving mid-run would drop the future doing the adding,
                    // part way through a queue, with no record of where it got
                    // to. The run is bounded and reports progress beside this.
                    button {
                        class: "btn btn-ghost",
                        disabled: busy(),
                        onclick: close,
                        "Cancel"
                    }
                    button {
                        class: "btn btn-primary",
                        disabled: !ready,
                        onclick: move |_| submit(()),
                        if busy() {
                            span { class: "loading loading-spinner loading-xs" }
                            {progress_label(added().len(), sources_to_add().len())}
                        } else {
                            {add_button_label(sources_to_add().len())}
                        }
                    }
                }
            }
            // Clicking away closes, which is what a dimmed backdrop implies.
            // No colour of its own: `.modal` already dims the page behind it,
            // and a second translucent layer on top darkened it twice.
            // The backdrop closes the dialog too, so it freezes with everything
            // else rather than being the one way out of a frozen dialog.
            div {
                class: "modal-backdrop",
                onclick: move |e| if !busy() { close(e) },
            }
        }
    }
}

/// How the commit button reads for a given queue length.
///
/// Says the count, so it is clear before pressing it that this adds four things
/// and not the one still in the field.
fn add_button_label(count: usize) -> String {
    match count {
        0 => "Add".to_string(),
        1 => "Add 1 package".to_string(),
        n => format!("Add {n} packages"),
    }
}

/// Where one queued source has got to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ChipState {
    /// Not started. Removable, because nothing has happened to it yet.
    Waiting,
    /// The request for this one is in flight.
    Adding,
    /// The server has taken it.
    Added,
}

/// What to show against a chip, given what the run has reached.
fn chip_state(label: &str, adding: Option<&str>, added: &[String]) -> ChipState {
    if added.iter().any(|done| done == label) {
        ChipState::Added
    } else if adding == Some(label) {
        ChipState::Adding
    } else {
        ChipState::Waiting
    }
}

/// The one source, when there is exactly one.
///
/// Editing is offered only then. The edits are paths inside a source, so with
/// two queued there is nothing to attach them to; `--patch` on the CLI refuses
/// the same case.
fn single_source(sources: &[SourceData]) -> Option<&SourceData> {
    match sources {
        [only] => Some(only),
        _ => None,
    }
}

/// How the "edit sources" button reads once something has been edited.
fn edited_label(count: usize) -> String {
    match count {
        1 => "1 file edited".to_string(),
        n => format!("{n} files edited"),
    }
}

/// How wide the dialog is, which depends on whether the editor is open.
fn dialog_width(editing: bool) -> &'static str {
    if editing { "max-w-6xl" } else { "max-w-2xl" }
}

/// How the button reads while a run is under way.
///
/// Counts rather than a bare spinner: resolving dependencies takes long enough
/// that a still spinner and a stuck one look the same, and with several queued
/// the only question is how far it has got.
fn progress_label(done: usize, total: usize) -> String {
    if total > 1 {
        format!("Adding… {done} of {total}")
    } else {
        "Adding…".to_string()
    }
}

/// The sources queued so far, each removable until the run reaches it.
#[component]
fn QueuedList(
    queued: Vec<SourceData>,
    /// The source currently being added, if a run is under way.
    #[props(default)]
    adding: Option<String>,
    /// Sources this run has already added.
    #[props(default)]
    added: Vec<String>,
    /// A run is under way, so nothing can be taken out of it. `submit` copies
    /// the queue before it starts and works from that copy, so removing a chip
    /// here would take it off the screen while the request for it still went.
    #[props(default = false)]
    frozen: bool,
    onremove: EventHandler<String>,
) -> Element {
    if queued.is_empty() {
        return rsx! {};
    }
    rsx! {
        div { class: "flex flex-wrap gap-2",
            for source in queued.iter() {
                {
                    let label = source_label(source);
                    let state = chip_state(&label, adding.as_deref(), &added);
                    rsx! {
                        span {
                            key: "{label}",
                            class: "badge gap-1 py-3 {chip_class(state)}",
                            match state {
                                ChipState::Adding => rsx! {
                                    span { class: "loading loading-spinner loading-xs" }
                                },
                                ChipState::Added => rsx! {
                                    span { "aria-label": "added", "✓" }
                                },
                                ChipState::Waiting => rsx! {},
                            }
                            span { class: "font-mono text-xs", "{label}" }
                            // Only while it can still be taken back. Removing
                            // one mid-flight would not stop the request, and
                            // removing one already added would not undo it.
                            if state == ChipState::Waiting && !frozen {
                                button {
                                    class: "btn btn-ghost btn-xs px-1",
                                    "aria-label": "Remove {label}",
                                    onclick: {
                                        let label = label.clone();
                                        move |_| onremove.call(label.clone())
                                    },
                                    "✕"
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The colour a chip carries for its state.
fn chip_class(state: ChipState) -> &'static str {
    match state {
        ChipState::Waiting => "badge-neutral",
        ChipState::Adding => "badge-primary",
        ChipState::Added => "badge-success",
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
    /// Names already on the server or already queued. Shown, but not
    /// selectable: that a package is already handled is the useful answer, and
    /// better than hiding it and leaving someone to wonder where it went.
    taken: Vec<String>,
    /// A run is under way. The queue was snapshotted when it started, so a
    /// package picked now would join a list nothing reads again -- the pill
    /// would appear and then quietly not be added.
    #[props(default = false)]
    frozen: bool,
    onpick: EventHandler<String>,
) -> Element {
    let query = query.trim();
    if query.is_empty() {
        return rsx! {
            p { class: "text-sm opacity-60 py-2",
                "Type a package name to search the AUR, or paste a git URL."
            }
        };
    }

    match results {
        None => rsx! {
            div { class: "py-2", span { class: "loading loading-spinner loading-sm" } }
        },
        Some(Err(e)) => rsx! {
            div { class: "alert alert-error text-sm", span { "Search failed: {e}" } }
        },
        // Two different questions were asked, so they get two different
        // answers. A short query was looked up by exact name, and reporting
        // "nothing matches" for it would suggest a search that never happened.
        Some(Ok(found)) if found.is_empty() && query.len() <= EXACT_LOOKUP_MAX => rsx! {
            p { class: "text-sm opacity-60 py-2", "No AUR package is called “{query}”." }
        },
        Some(Ok(found)) if found.is_empty() => rsx! {
            p { class: "text-sm opacity-60 py-2", "Nothing in the AUR matches “{query}”." }
        },
        Some(Ok(found)) => rsx! {
            ul { class: "menu menu-sm p-0 max-h-56 overflow-y-auto border border-base-300 rounded-box flex-nowrap",
                for result in found {
                    {
                        let held = taken.contains(&result.name);
                        let locked = held || frozen;
                        rsx! {
                            li { key: "{result.name}",
                                button {
                                    class: if locked { "opacity-50 cursor-not-allowed" } else { "" },
                                    disabled: locked,
                                    onclick: {
                                        let name = result.name.clone();
                                        move |_| onpick.call(name.clone())
                                    },
                                    // Name and version on one line, the AUR's
                                    // summary under it: a search matches on the
                                    // description too, so without it a result
                                    // can look unrelated to what was typed.
                                    div { class: "flex flex-col items-start gap-0.5 min-w-0",
                                        div { class: "flex items-baseline gap-2",
                                            span { class: "font-mono", "{result.name}" }
                                            span { class: "opacity-50 text-xs", "{result.version}" }
                                            if held {
                                                span { class: "badge badge-outline badge-xs", "added" }
                                            }
                                        }
                                        if let Some(description) = result.description.as_deref() {
                                            span { class: "text-xs opacity-60 text-left", "{description}" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ChipState, QueuedList, SEARCH_CACHE_ENTRIES, SearchCache, SearchResults, add_button_label,
        chip_state, dialog_width, edited_label, git_source, progress_label, rank_results,
        single_source, source_at, source_for, source_label, to_add,
    };
    use aurcache_client::{SearchResult, SourceData};
    use dioxus::prelude::*;

    #[component]
    fn Harness(
        query: String,
        results: Option<Result<Vec<SearchResult>, String>>,
        taken: Vec<String>,
        #[props(default = false)] frozen: bool,
    ) -> Element {
        rsx! {
            SearchResults { query, results, taken, frozen, onpick: move |_| {} }
        }
    }

    fn result(name: &str, description: Option<&str>) -> SearchResult {
        SearchResult {
            name: name.to_string(),
            version: "1.0-1".to_string(),
            description: description.map(str::to_string),
        }
    }

    /// The property the whole thing rests on: the AUR matches substrings, so a
    /// longer query's answer is contained in a shorter one's.
    #[test]
    fn a_cached_search_answers_anything_that_extends_it() {
        let mut cache = SearchCache::default();
        cache.insert("hel", &[result("hello", None), result("helm", None)]);

        let narrowed = cache.narrow("hello").expect("hel can answer hello");
        assert_eq!(narrowed.len(), 1);
        assert_eq!(narrowed[0].name, "hello");
    }

    /// Narrowing must not consume what it narrowed: typing past a query and
    /// deleting back to it filters the original results again, not a remnant.
    #[test]
    fn narrowing_is_non_destructive() {
        let mut cache = SearchCache::default();
        cache.insert("hel", &[result("hello", None), result("helm", None)]);

        assert_eq!(cache.narrow("hello").unwrap().len(), 1);
        // Back to the original query, and back to the original answer.
        assert_eq!(cache.narrow("hel").unwrap().len(), 2);
        // And on to a different extension of the same base.
        assert_eq!(cache.narrow("helm").unwrap().len(), 1);
    }

    /// A result the AUR returned for matching in its *description* has to
    /// survive narrowing, or the list would silently shed exactly the results
    /// a name-only filter cannot explain.
    #[test]
    fn narrowing_keeps_description_matches() {
        let mut cache = SearchCache::default();
        cache.insert(
            "term",
            &[result("alacritty", Some("A fast terminal emulator"))],
        );

        let narrowed = cache.narrow("termin").expect("term can answer termin");
        assert_eq!(narrowed.len(), 1, "description match was dropped");
    }

    /// A query that is not an extension of anything cached needs the network;
    /// answering it from an unrelated set would invent results.
    #[test]
    fn an_unrelated_query_is_not_answered_locally() {
        let mut cache = SearchCache::default();
        cache.insert("hel", &[result("hello", None)]);

        assert!(cache.narrow("vim").is_none());
        // Shorter than the cached query: its answer is a superset we do not have.
        assert!(cache.narrow("he").is_none());
    }

    /// One- and two-character entries are exact lookups, not substring
    /// searches. Caching one would make `ab` claim to answer `abc`, when all it
    /// ever held was the package literally called `ab`.
    #[test]
    fn an_exact_lookup_is_never_used_as_a_base() {
        let mut cache = SearchCache::default();
        cache.insert("ab", &[result("ab", None)]);
        assert!(cache.narrow("abc").is_none());
    }

    /// Bounded, because exploring tries many unrelated queries and a broad
    /// result set is thousands of packages.
    #[test]
    fn the_cache_forgets_the_oldest_searches() {
        let mut cache = SearchCache::default();
        for i in 0..SEARCH_CACHE_ENTRIES + 4 {
            cache.insert(&format!("query{i}"), &[result("pkg", None)]);
        }
        assert_eq!(cache.0.len(), SEARCH_CACHE_ENTRIES);
        assert!(
            cache.narrow("query0x").is_none(),
            "oldest should be evicted"
        );
        let newest = format!("query{}x", SEARCH_CACHE_ENTRIES + 3);
        assert!(cache.narrow(&newest).is_some(), "newest should be kept");
    }

    /// Case is not part of the question: the AUR matches case-insensitively.
    #[test]
    fn narrowing_ignores_case() {
        let mut cache = SearchCache::default();
        cache.insert("Hel", &[result("Hello", None)]);
        assert_eq!(cache.narrow("hELLo").unwrap().len(), 1);
    }

    fn render(query: &str, results: Option<Result<Vec<SearchResult>, String>>) -> String {
        render_with(query, results, Vec::new())
    }

    /// The description is why a result that does not look like the query is in
    /// the list at all -- the AUR matches `name-desc`, so `hello` returns
    /// `edax-reversi` for its "othello" description. Showing the name alone
    /// left that unexplained.
    #[test]
    fn a_result_shows_what_the_package_is() {
        let found = Ok(vec![result(
            "edax-reversi",
            Some("Edax is a very strong othello engine"),
        )]);
        let html = render("hello", Some(found));
        assert!(
            html.contains("Edax is a very strong othello engine"),
            "the description is not on screen: {html}"
        );
    }

    /// A package with no description still renders, without an empty line where
    /// one would have been.
    #[test]
    fn a_result_without_a_description_renders_plainly() {
        let html = render("hello", Some(Ok(vec![result("hello", None)])));
        assert!(html.contains("hello"));
    }

    fn render_with(
        query: &str,
        results: Option<Result<Vec<SearchResult>, String>>,
        taken: Vec<String>,
    ) -> String {
        render_search(query, results, taken, false)
    }

    fn render_search(
        query: &str,
        results: Option<Result<Vec<SearchResult>, String>>,
        taken: Vec<String>,
        frozen: bool,
    ) -> String {
        let mut dom = VirtualDom::new_with_props(
            Harness,
            HarnessProps {
                query: query.to_string(),
                results,
                taken,
                frozen,
            },
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    /// Same reason as `Harness`: an `EventHandler` only exists inside a running
    /// runtime, so the list is rendered through a component rather than handed
    /// props from outside.
    #[component]
    fn QueueHarness(
        queued: Vec<SourceData>,
        adding: Option<String>,
        added: Vec<String>,
        #[props(default = false)] frozen: bool,
    ) -> Element {
        rsx! {
            QueuedList { queued, adding, added, frozen, onremove: move |_| {} }
        }
    }

    fn render_queue(queued: Vec<SourceData>) -> String {
        render_run(queued, None, Vec::new())
    }

    fn render_run(queued: Vec<SourceData>, adding: Option<&str>, added: Vec<String>) -> String {
        render_run_frozen(queued, adding, added, false)
    }

    fn render_run_frozen(
        queued: Vec<SourceData>,
        adding: Option<&str>,
        added: Vec<String>,
        frozen: bool,
    ) -> String {
        let mut dom = VirtualDom::new_with_props(
            QueueHarness,
            QueueHarnessProps {
                queued,
                adding: adding.map(ToString::to_string),
                added,
                frozen,
            },
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    /// While a run is under way the queue it is working from has already been
    /// copied, so a package picked out of the search list would show as a pill
    /// and then never be added. Every row is refused for the duration.
    #[test]
    fn search_results_cannot_be_picked_during_a_run() {
        let found = Ok(vec![SearchResult {
            name: "hello".to_string(),
            version: "1.0".to_string(),
            description: None,
        }]);

        let open = render_search("hello", Some(found.clone()), Vec::new(), false);
        assert!(open.contains("hello"));
        assert!(
            !open.contains("disabled"),
            "a free row should be pickable: {open}"
        );

        let frozen = render_search("hello", Some(found), Vec::new(), true);
        assert!(frozen.contains("hello"), "the row is still shown: {frozen}");
        assert!(
            frozen.contains("disabled"),
            "a row stayed pickable while a run was under way: {frozen}"
        );
    }

    /// Same reason from the other side: `submit` works from a copy of the
    /// queue, so taking a pill out mid-run removes it from the screen while
    /// the request for it still goes.
    #[test]
    fn queued_packages_cannot_be_removed_during_a_run() {
        let queued = vec![aur("hello"), aur("neofetch")];

        let open = render_run_frozen(queued.clone(), None, Vec::new(), false);
        assert!(
            open.contains("Remove hello"),
            "a waiting package should be removable: {open}"
        );

        // `neofetch` is still waiting its turn -- the tempting one to remove,
        // and the one whose removal would be a lie.
        let frozen = render_run_frozen(queued, Some("hello"), Vec::new(), true);
        assert!(frozen.contains("neofetch"), "the pill is still shown");
        assert!(
            !frozen.contains("Remove "),
            "a package could still be taken out of a running queue: {frozen}"
        );
    }

    fn aur(name: &str) -> SourceData {
        SourceData::Aur {
            name: name.to_string(),
        }
    }

    fn found(names: &[(&str, &str)]) -> Option<Result<Vec<SearchResult>, String>> {
        Some(Ok(names
            .iter()
            .map(|(name, version)| SearchResult {
                name: (*name).to_string(),
                version: (*version).to_string(),
                description: None,
            })
            .collect()))
    }

    /// An empty box has not asked anything, so it gets a prompt rather than a
    /// verdict.
    #[test]
    fn an_empty_box_is_not_a_failed_search() {
        for query in ["", "   "] {
            let html = render(query, Some(Ok(Vec::new())));
            assert!(html.contains("Type a package name"), "{html}");
            assert!(!html.contains("Nothing in the AUR"), "{html}");
        }
    }

    /// Short names are real: `a` and `zz` are both AUR packages. The server
    /// answers a query this short by looking the name up exactly, so nothing
    /// here may refuse to ask — a package nobody can add is worse than a
    /// needless request.
    #[test]
    fn a_one_or_two_character_name_is_still_searched() {
        for query in ["a", "zz"] {
            let html = render(query, found(&[(query, "1.0-1")]));
            assert!(html.contains(query), "{query} should be listed: {html}");
            assert!(!html.contains("Type a package name"), "{html}");
        }
    }

    /// The two lookups asked different questions, so an empty answer means
    /// different things. "Nothing matches" implies a search that, for a short
    /// query, never happened.
    #[test]
    fn an_empty_answer_says_which_question_was_asked() {
        let exact = render("ab", Some(Ok(Vec::new())));
        assert!(exact.contains("No AUR package is called"), "{exact}");

        let searched = render("nonesuch", Some(Ok(Vec::new())));
        assert!(
            searched.contains("Nothing in the AUR matches"),
            "{searched}"
        );
    }

    /// A search that is still running must not look like one that came back
    /// empty; the two are a spinner and a verdict.
    #[test]
    fn a_pending_search_shows_no_verdict() {
        let html = render("hello", None);
        assert!(html.contains("loading"), "{html}");
        assert!(!html.contains("Nothing in the AUR"), "{html}");
    }

    /// The whole point of marking them: adding a package the server already has
    /// is a silent no-op, so queueing it would close the dialog having done
    /// nothing, with nothing to say why. It stays visible, since "already
    /// handled" is the useful answer, but it cannot be picked.
    #[test]
    fn an_already_added_package_cannot_be_picked() {
        let html = render_with(
            "hello",
            found(&[("hello", "2.12.1-1"), ("hello-world", "1.0-3")]),
            vec!["hello".to_string()],
        );
        assert!(html.contains("hello"), "still listed: {html}");
        assert!(html.contains("added"), "marked as already added: {html}");
        assert!(html.contains("disabled"), "not selectable: {html}");
    }

    /// Only the taken ones. A result that is disabled when it should not be is
    /// a package nobody can add.
    #[test]
    fn a_package_that_is_not_added_stays_selectable() {
        let html = render_with("hello", found(&[("hello-world", "1.0-3")]), Vec::new());
        assert!(!html.contains("disabled"), "{html}");
        assert!(!html.contains(">added<"), "{html}");
    }

    /// Chips key on this label, so two sources with the same label are treated
    /// as the same thing — which is what makes the duplicate check work.
    #[test]
    fn a_source_label_identifies_it() {
        assert_eq!(
            source_label(&SourceData::Aur {
                name: "hello".to_string()
            }),
            "hello"
        );
        let git = git_source("https://e.invalid/r.git", "main", "sub").unwrap();
        assert_eq!(source_label(&git), "https://e.invalid/r.git#main/sub");

        // Same URL, different ref: different sources, so different labels.
        let other = git_source("https://e.invalid/r.git", "v2", "sub").unwrap();
        assert_ne!(source_label(&git), source_label(&other));
    }

    /// The rule behind the Add button. A queue is a deliberate list, so it is
    /// what gets sent; with nothing queued, the field is the whole request, so
    /// one known name never needs queueing first.
    #[test]
    fn the_queue_wins_over_the_field() {
        let aur = |name: &str| SourceData::Aur {
            name: name.to_string(),
        };

        // Nothing at all.
        assert!(to_add(Vec::new(), None).is_empty());

        // Just the field: that is the request.
        assert_eq!(
            to_add(Vec::new(), Some(aur("hello")))
                .iter()
                .map(source_label)
                .collect::<Vec<_>>(),
            ["hello"]
        );

        // A queue, and a field left with something in it: the queue is sent and
        // the field is not.
        assert_eq!(
            to_add(vec![aur("hello"), aur("neofetch")], Some(aur("yay")))
                .iter()
                .map(source_label)
                .collect::<Vec<_>>(),
            ["hello", "neofetch"]
        );
    }

    /// Says what pressing it will do, since the queue is what gets added and
    /// not whatever is still sitting in the field.
    /// Each queued source is listed and individually removable — queueing four
    /// and wanting three must not mean starting over.
    #[test]
    fn queued_sources_are_listed_and_removable() {
        let html = render_queue(vec![
            SourceData::Aur {
                name: "hello".to_string(),
            },
            git_source("https://e.invalid/r.git", "main", "").unwrap(),
        ]);
        assert!(html.contains("hello"), "{html}");
        assert!(html.contains("https://e.invalid/r.git#main"), "{html}");
        assert!(html.contains("Remove hello"), "each chip removable: {html}");
    }

    /// The AUR does not order by relevance: this is the real response to
    /// `?query=hello`, in which the package actually called `hello` is eighth.
    #[test]
    fn the_exact_match_comes_first() {
        let mut results: Vec<SearchResult> = [
            "howdy-git",
            "biopass-bin",
            "howdy-bin",
            "wsl-hello-sudo-bin",
            "howdy-beta-git",
            "howdy",
            "hello",
            "hello-world",
        ]
        .iter()
        .map(|name| SearchResult {
            name: (*name).to_string(),
            version: "1-1".to_string(),
            description: None,
        })
        .collect();

        rank_results("hello", &mut results);
        let names: Vec<_> = results.iter().map(|r| r.name.as_str()).collect();

        assert_eq!(names[0], "hello", "exact match first: {names:?}");
        assert_eq!(names[1], "hello-world", "then prefix matches: {names:?}");
        assert_eq!(
            names[2], "wsl-hello-sudo-bin",
            "then anything else containing it: {names:?}"
        );
        // The rest matched on description rather than name, and keep the AUR's
        // own order among themselves.
        assert_eq!(
            &names[3..],
            &[
                "howdy-git",
                "biopass-bin",
                "howdy-bin",
                "howdy-beta-git",
                "howdy"
            ]
        );
    }

    /// Someone typing `HELLO` means the same package.
    #[test]
    fn ranking_ignores_case() {
        let mut results: Vec<SearchResult> = ["hello-world", "Hello"]
            .iter()
            .map(|name| SearchResult {
                name: (*name).to_string(),
                version: "1-1".to_string(),
                description: None,
            })
            .collect();
        rank_results("  HELLO  ", &mut results);
        assert_eq!(results[0].name, "Hello");
    }

    /// Resolving dependencies is slow enough that a run of several packages
    /// needs to say where it has got to. A single spinner cannot distinguish
    /// "working on the fourth" from "stuck on the first".
    #[test]
    fn a_run_shows_which_package_it_is_on() {
        let html = render_run(
            vec![aur("hello"), aur("neofetch"), aur("yay")],
            Some("neofetch"),
            vec!["hello".to_string()],
        );

        // Done, in flight, and not started are three different things.
        assert!(html.contains("badge-success"), "hello is done: {html}");
        assert!(
            html.contains("loading-spinner"),
            "neofetch is in flight: {html}"
        );
        assert!(
            html.contains("badge-neutral"),
            "yay has not started: {html}"
        );
    }

    /// Removing one mid-flight would not stop its request, and removing one
    /// already added would not undo it — so only the untouched ones offer it.
    #[test]
    fn only_a_package_still_waiting_can_be_removed() {
        let html = render_run(
            vec![aur("hello"), aur("neofetch"), aur("yay")],
            Some("neofetch"),
            vec!["hello".to_string()],
        );
        assert!(html.contains("Remove yay"), "still waiting: {html}");
        assert!(!html.contains("Remove neofetch"), "in flight: {html}");
        assert!(!html.contains("Remove hello"), "already added: {html}");
    }

    #[test]
    fn chip_state_reports_where_each_package_is() {
        let added = vec!["hello".to_string()];
        assert_eq!(
            chip_state("hello", Some("neofetch"), &added),
            ChipState::Added
        );
        assert_eq!(
            chip_state("neofetch", Some("neofetch"), &added),
            ChipState::Adding
        );
        assert_eq!(
            chip_state("yay", Some("neofetch"), &added),
            ChipState::Waiting
        );
        // Nothing running: everything not yet added is waiting.
        assert_eq!(chip_state("yay", None, &added), ChipState::Waiting);
    }

    /// A count is only worth showing when there is more than one to count.
    #[test]
    fn the_progress_label_counts_only_a_real_queue() {
        assert_eq!(progress_label(0, 1), "Adding…");
        assert_eq!(progress_label(2, 5), "Adding… 2 of 5");
    }

    /// Edits are paths inside one source, so with two queued there is nothing
    /// to attach them to. The CLI refuses `--patch` with several packages for
    /// the same reason.
    #[test]
    fn sources_can_only_be_edited_one_at_a_time() {
        assert!(single_source(&[]).is_none());
        assert!(single_source(&[aur("hello")]).is_some());
        assert!(single_source(&[aur("hello"), aur("yay")]).is_none());
    }

    /// Two panes need room a modal does not have by default.
    #[test]
    fn the_dialog_widens_for_the_editor() {
        assert_ne!(dialog_width(true), dialog_width(false));
    }

    #[test]
    fn the_edit_button_counts_the_files() {
        assert_eq!(edited_label(1), "1 file edited");
        assert_eq!(edited_label(3), "3 files edited");
    }

    /// Nothing queued is not an empty box with a heading, it is nothing.
    #[test]
    fn an_empty_queue_renders_nothing() {
        assert_eq!(render_queue(Vec::new()), "");
    }

    #[test]
    fn the_button_counts_what_it_will_add() {
        assert_eq!(add_button_label(0), "Add");
        assert_eq!(add_button_label(1), "Add 1 package");
        assert_eq!(add_button_label(4), "Add 4 packages");
    }

    /// Only an AUR name can be checked against the server's package list; a git
    /// remote's pkgbase lives in a PKGBUILD nothing here has read.
    #[test]
    fn only_an_aur_source_predicts_its_pkgbase() {
        assert_eq!(
            source_at(&SourceData::Aur {
                name: "hello".to_string()
            }),
            Some("hello")
        );
        assert_eq!(
            source_at(&git_source("https://e.invalid/r.git", "main", "").unwrap()),
            None
        );
    }

    #[test]
    fn results_are_listed_with_their_versions() {
        let html = render(
            "hello",
            Some(Ok(vec![SearchResult {
                name: "hello-world".to_string(),
                version: "1.0-3".to_string(),
                description: None,
            }])),
        );
        assert!(html.contains("hello-world"), "{html}");
        assert!(html.contains("1.0-3"), "{html}");
    }

    /// The whole point of dropping the tabs: the field works out its own kind.
    #[test]
    fn the_entry_field_tells_a_remote_from_a_package_name() {
        assert!(matches!(
            source_for("hello", "master", ""),
            Some(SourceData::Aur { .. })
        ));
        // A name that merely looks git-ish is still a package name.
        assert!(matches!(
            source_for("paru-git", "master", ""),
            Some(SourceData::Aur { .. })
        ));
        assert!(matches!(
            source_for("lab.git", "master", ""),
            Some(SourceData::Aur { .. })
        ));
        assert!(matches!(
            source_for("https://github.com/user/repo.git", "master", ""),
            Some(SourceData::Git { .. })
        ));
        // The SCP-like shorthand has no scheme, only a user.
        assert!(matches!(
            source_for("aur@aur.archlinux.org:paru", "master", ""),
            Some(SourceData::Git { .. })
        ));
    }

    #[test]
    fn an_empty_field_describes_nothing() {
        assert!(source_for("", "master", "").is_none());
        assert!(source_for("   ", "master", "").is_none());
    }

    #[test]
    fn an_aur_name_is_trimmed() {
        let Some(SourceData::Aur { name }) = source_for("  hello  ", "master", "") else {
            panic!("expected an AUR source");
        };
        assert_eq!(name, "hello");
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

/// Editing a not-yet-added package's sources.
///
/// Reads the pristine files through the preview endpoints, which take the
/// source in the request body because there is no package to name yet. Edits
/// are kept here as whole files and travel with the add; the server diffs each
/// against the upstream it fetches, so nothing is stored until the package is.
#[component]
fn AddSourceEditor(
    source: SourceData,
    patched: Signal<BTreeMap<String, String>>,
    onclose: EventHandler<()>,
) -> Element {
    let mut patched = patched;
    let files = use_resource({
        let source = source.clone();
        move || {
            let source = source.clone();
            async move {
                crate::api::client()?
                    .preview_source_files(&source)
                    .await
                    .map(|list| list.files)
                    .map_err(|e| e.to_string())
            }
        }
    });

    let mut selected = use_signal(|| Option::<String>::None);
    // What the file looks like upstream, so an edit can be measured against it
    // and reverted to it.
    let mut pristine = use_signal(String::new);
    let mut draft = use_signal(String::new);
    let mut error = use_signal(|| Option::<String>::None);

    let open_file = move |source: SourceData, path: String| async move {
        error.set(None);
        let client = match crate::api::client() {
            Ok(client) => client,
            Err(e) => return error.set(Some(e)),
        };
        match client.preview_source_file(&source, &path).await {
            Ok(content) => {
                pristine.set(content.original_content.clone());
                // An edit already made to this file wins over the upstream
                // copy, so reopening it shows the work rather than losing it.
                draft.set(
                    patched
                        .peek()
                        .get(&path)
                        .cloned()
                        .unwrap_or(content.original_content),
                );
                selected.set(Some(path));
            }
            Err(e) => error.set(Some(e.to_string())),
        }
    };

    // The point of opening the editor is nearly always the PKGBUILD, so it is
    // opened rather than offered.
    use_effect({
        let source = source.clone();
        move || {
            if selected.peek().is_some() {
                return;
            }
            let Some(Ok(list)) = &*files.read_unchecked() else {
                return;
            };
            let Some(first) = list
                .iter()
                .find(|p| p.as_str() == "PKGBUILD")
                .or_else(|| list.first())
                .cloned()
            else {
                return;
            };
            let source = source.clone();
            spawn(async move { open_file(source, first).await });
        }
    });

    let dirty = draft() != pristine();
    // Recorded when it differs from upstream and dropped when it matches again,
    // so typing a change and undoing it by hand leaves nothing behind.
    let mut keep = move || {
        let Some(path) = selected() else { return };
        if draft() == pristine() {
            patched.write().remove(&path);
        } else {
            patched.write().insert(path, draft());
        }
    };

    rsx! {
        div { class: "space-y-2",
            crate::source_editor::SourcePane {
                title: source_label(&source),
                files: files.read_unchecked().clone(),
                selected: selected(),
                onselect: {
                    let source = source.clone();
                    move |path: String| {
                        // Hold what is on screen before leaving it, so moving
                        // between files does not quietly discard an edit.
                        keep();
                        let source = source.clone();
                        spawn(async move { open_file(source, path).await });
                    }
                },
                modified: patched().keys().cloned().collect(),
                draft,
                dirty,
                // A modal has less room than a page.
                height: "h-64",
                actions: rsx! {
                    button {
                        class: "btn btn-ghost btn-sm",
                        disabled: !dirty,
                        onclick: move |_| draft.set(pristine()),
                        "Revert to upstream"
                    }
                    button {
                        class: "btn btn-primary btn-sm",
                        onclick: move |_| {
                            keep();
                            onclose.call(());
                        },
                        "Done"
                    }
                },
                notices: rsx! {
                    if let Some(message) = error() {
                        div { class: "alert alert-error text-sm", span { "{message}" } }
                    }
                },
            }
        }
    }
}
