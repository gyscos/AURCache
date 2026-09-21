//! One worker: what it is, what it has done, and what it is configured with.
//!
//! Split from the fleet list because the settings are the bulk of it -- a
//! machine declares a couple of dozen, each with a description worth reading,
//! which is a page rather than a column. The list answers "is the fleet
//! healthy"; this answers "what is this machine actually running".
//!
//! Read-only for now. Nothing here is set from the server yet
//! (`design/worker-configuration.md`), so the page's job is to say where each
//! value came from and which of them the worker refused.

use crate::dates::RelativeDate;
use crate::format::now_secs;
use crate::routes::Route;
use crate::screens::workers::{KindBadge, Liveness, StatusBadge, architectures};
use aurcache_client::Worker as WorkerRow;
use aurcache_common::worker_config::{
    EffectiveConfig, EffectiveSetting, EffectiveSource, SettingDecl, SettingStatus,
};
use dioxus::prelude::*;

/// How many characters of a fingerprint disambiguate two workers of one name.
///
/// The git short-hash idiom, and for the same reason: long enough that a
/// collision needs contriving, short enough to read in a URL. The page shows
/// the fingerprint in full.
const SHORT_FINGERPRINT: usize = 12;

/// The head of a fingerprint, as a URL carries it.
#[must_use]
pub(crate) fn short_fingerprint(fingerprint: &str) -> String {
    fingerprint.chars().take(SHORT_FINGERPRINT).collect()
}

/// What a URL naming a worker turned out to mean.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Resolved<'a> {
    /// No worker answers to it.
    None,
    /// Exactly one does.
    One(&'a WorkerRow),
    /// Several do, and the URL does not say which.
    ///
    /// Not a case to guess at: the usual way to get here is a machine replaced
    /// by another of the same hostname, so the rows differ in exactly the thing
    /// the reader cares about -- one is retired and one is live.
    Several(Vec<&'a WorkerRow>),
}

/// Find the worker a URL points at.
///
/// A fingerprint identifies one outright. A name may not: a worker is called
/// whatever its machine calls itself, and a retired row keeps its name for
/// ever, so this hands back the ambiguity rather than resolving it by picking.
pub(crate) fn resolve<'a>(workers: &'a [WorkerRow], name: &str, fingerprint: &str) -> Resolved<'a> {
    let matches: Vec<&WorkerRow> = if fingerprint.is_empty() {
        workers.iter().filter(|w| w.name == name).collect()
    } else {
        // Hex, and a URL is not a place to be particular about its case.
        let prefix = fingerprint.to_ascii_lowercase();
        workers
            .iter()
            .filter(|w| w.cert_fingerprint.to_ascii_lowercase().starts_with(&prefix))
            .collect()
    };
    match matches.len() {
        0 => Resolved::None,
        1 => Resolved::One(matches[0]),
        _ => Resolved::Several(matches),
    }
}

/// A worker name as the URL carries it.
///
/// Split on `/` because the route segment is a catch-all: a name is free text,
/// and one containing a slash has to survive the trip rather than being read
/// back as something else.
#[must_use]
pub(crate) fn name_segments(name: &str) -> Vec<String> {
    name.split('/').map(str::to_string).collect()
}

/// Names claimed by more than one worker: a link to one of those goes by
/// fingerprint, never by name.
///
/// Counted once per table render rather than once per row — the fleet only
/// grows, and asking the whole fleet per row is quadratic.
pub(crate) fn shared_names(fleet: &[WorkerRow]) -> std::collections::HashSet<&str> {
    let mut seen = std::collections::HashSet::new();
    let mut shared = std::collections::HashSet::new();
    for worker in fleet {
        if !seen.insert(worker.name.as_str()) {
            shared.insert(worker.name.as_str());
        }
    }
    shared
}

/// The URL for a worker, among the fleet it belongs to.
///
/// The bare name wherever it is unambiguous, which is nearly always. A worker
/// sharing its name with another is linked by fingerprint instead, so a link
/// from the list never lands on a chooser -- that is for a name someone typed
/// or pasted.
///
/// `shared` comes from [`shared_names`]: the table counts once per render and
/// hands each row its answer.
#[must_use]
pub(crate) fn worker_route(worker: &WorkerRow, shared: bool) -> Route {
    if shared {
        Route::WorkerByFingerprint {
            fingerprint: short_fingerprint(&worker.cert_fingerprint),
        }
    } else {
        Route::Worker {
            name: name_segments(&worker.name),
        }
    }
}

/// One worker, by the name it calls itself.
#[component]
pub fn Worker(name: Vec<String>) -> Element {
    rsx! {
        WorkerPage { name: name.join("/"), fingerprint: String::new() }
    }
}

/// One worker, by the fingerprint that is its actual identity.
#[component]
pub fn WorkerByFingerprint(fingerprint: String) -> Element {
    rsx! {
        WorkerPage { name: String::new(), fingerprint }
    }
}

/// The page itself, once the URL has been turned into a worker.
///
/// Resolved against the fleet list rather than an endpoint of its own:
/// everything the header shows is already in it, including the liveness and the
/// build record the server derives, and a second shape for one row would be a
/// second thing to keep in step.
#[component]
fn WorkerPage(name: String, fingerprint: String) -> Element {
    let workers = use_resource(move || async move {
        crate::api::client()?
            .list_workers()
            .await
            .map_err(|e| e.to_string())
    });

    rsx! {
        div { class: "flex flex-col gap-4",
            match &*workers.read_unchecked() {
                None => rsx! {
                    span { class: "loading loading-spinner loading-md" }
                },
                Some(Err(e)) => rsx! {
                    div { class: "alert alert-error alert-soft", "{e}" }
                },
                Some(Ok(list)) => match resolve(list, &name, &fingerprint) {
                    // A revoked worker keeps its row, so this is a name that
                    // never enrolled -- or one whose row was removed outright.
                    Resolved::None => rsx! {
                        div { class: "alert alert-warning alert-soft",
                            if name.is_empty() {
                                "No worker with that certificate."
                            } else {
                                "No worker called "
                                span { class: "font-mono", "{name}" }
                                "."
                            }
                        }
                    },
                    Resolved::One(worker) => rsx! {
                        WorkerHeader { worker: worker.clone() }
                        div { class: "card bg-base-100 shadow-xl",
                            div { class: "card-body",
                                h2 { class: "card-title text-base", "Settings" }
                                p { class: "text-sm opacity-70",
                                    "What this worker accepts, and what each setting resolved to on that machine. "
                                    "Set them in its environment; AURCache does not override them."
                                }
                                WorkerConfig { id: worker.id }
                            }
                        }
                    },
                    Resolved::Several(matches) => rsx! {
                        WorkerChooser { name: name.clone(), matches: matches.into_iter().cloned().collect::<Vec<_>>() }
                    },
                },
            }
        }
    }
}

/// Which of the machines called this did you mean?
///
/// Shows what tells them apart -- whether each is still in the fleet, when it
/// last called in, and the fingerprint that is its actual identity.
#[component]
fn WorkerChooser(name: String, matches: Vec<WorkerRow>) -> Element {
    let now = now_secs();
    rsx! {
        div { class: "card bg-base-100 shadow-xl",
            div { class: "card-body",
                h1 { class: "card-title",
                    span { class: "font-mono", "{name}" }
                }
                p { class: "text-sm opacity-70",
                    "{matches.len()} workers call themselves this. A name is whatever a machine "
                    "reports; the fingerprint below is the identity behind it."
                }
                ul { class: "flex flex-col gap-2 mt-2",
                    for worker in matches.iter() {
                        li { key: "{worker.cert_fingerprint}",
                            Link {
                                class: "flex flex-wrap items-center gap-2 p-2 rounded hover:bg-base-200",
                                to: Route::WorkerByFingerprint {
                                    fingerprint: short_fingerprint(&worker.cert_fingerprint),
                                },
                                span { class: "font-mono text-sm", "{short_fingerprint(&worker.cert_fingerprint)}" }
                                StatusBadge { status: worker.status }
                                Liveness { worker: worker.clone() }
                                span { class: "text-sm opacity-60",
                                    if worker.last_seen.is_some() {
                                        RelativeDate { ts: worker.last_seen, now }
                                    } else {
                                        "never seen"
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Who this machine is, and how it is doing.
///
/// The same facts the list column shows, because they are the ones an operator
/// arrives having just read -- the page should confirm what they clicked, not
/// restate it differently.
#[component]
fn WorkerHeader(worker: WorkerRow) -> Element {
    let now = now_secs();
    rsx! {
        div { class: "card bg-base-100 shadow-xl",
            div { class: "card-body",
                div { class: "flex items-center min-h-8",
                    h1 { class: "card-title block leading-8 break-all",
                        Link {
                            class: "opacity-60 link-hover",
                            to: Route::Workers {},
                            "Workers"
                        }
                        span { class: "opacity-30 mx-2", "/" }
                        span { class: "font-mono", "{worker.name}" }
                    }
                }
                div { class: "flex flex-wrap items-center gap-2",
                    StatusBadge { status: worker.status }
                    KindBadge { worker: worker.clone() }
                    Liveness { worker: worker.clone() }
                }
                dl { class: "grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 text-sm mt-2",
                    dt { class: "opacity-60", "Architectures" }
                    dd { {architectures(&worker)} }

                    dt { class: "opacity-60", "Reserved for" }
                    dd {
                        if worker.package_affinity.is_empty() {
                            span { class: "opacity-40", "—" }
                        } else {
                            div { class: "flex flex-wrap gap-1",
                                for package in worker.package_affinity.iter() {
                                    Link {
                                        key: "{package}",
                                        class: "badge badge-outline badge-sm font-mono link-hover",
                                        to: Route::Package { pkgbase: package.clone() },
                                        "{package}"
                                    }
                                }
                            }
                        }
                    }

                    dt { class: "opacity-60", "Priority" }
                    dd {
                        if worker.priority == 0 {
                            span { class: "opacity-40", title: "No preference", "—" }
                        } else {
                            "{worker.priority}"
                        }
                    }

                    dt { class: "opacity-60", "Builds" }
                    dd { "{worker.active_builds} running · {worker.successful_builds} succeeded · {worker.failed_builds} failed" }

                    dt { class: "opacity-60", "Last seen" }
                    dd {
                        if worker.last_seen.is_some() {
                            RelativeDate { ts: worker.last_seen, now }
                        } else {
                            span { class: "opacity-60", "never" }
                        }
                    }

                    // The identity behind the name, which is not unique. Last
                    // because it is the one nobody reads until they have to.
                    dt { class: "opacity-60", "Fingerprint" }
                    dd { class: "font-mono break-all opacity-70", "{worker.cert_fingerprint}" }
                }
            }
        }
    }
}

/// A worker's declared settings, and what each of them resolved to.
///
/// Fetched apart from the workers list: that list is polled while anything is
/// building, and a declaration is kilobytes of descriptions that change only
/// when a worker is upgraded.
///
/// Read-only. Values are the worker's own — from its environment or its
/// built-in defaults — and this says which, because "the variable I set is not
/// the value it is running" is the failure this exists to make visible.
#[component]
fn WorkerConfig(id: i32) -> Element {
    let config = use_resource(move || async move {
        crate::api::client()?
            .worker_config(id)
            .await
            .map_err(|e| e.to_string())
    });

    rsx! {
        div {
            match &*config.read_unchecked() {
                None => rsx! {
                    span { class: "loading loading-spinner loading-sm" }
                },
                Some(Err(e)) => rsx! {
                    div { class: "alert alert-error alert-soft text-sm", "{e}" }
                },
                Some(Ok(view)) => match &view.settings {
                    // Not "no settings": an older worker declares nothing
                    // because it cannot, and saying so points at the remedy.
                    None => rsx! {
                        div { class: "text-sm opacity-70",
                            "This worker's version does not report what it can be configured with. "
                            "Upgrading it fills this in."
                        }
                    },
                    Some(declared) if declared.is_empty() => rsx! {
                        div { class: "text-sm opacity-70", "This worker declares no settings." }
                    },
                    Some(declared) => rsx! { SettingsTable { declared: declared.clone(), effective: view.effective.clone() } },
                },
            }
        }
    }
}

/// The declared settings, grouped the way the worker grouped them.
#[component]
fn SettingsTable(declared: Vec<SettingDecl>, effective: Option<EffectiveConfig>) -> Element {
    let groups = group_by_category(&declared);
    rsx! {
        if effective.is_none() {
            div { class: "text-sm opacity-70 mb-3",
                "Waiting for this worker's first report; these are the settings it accepts, not yet the values it is running."
            }
        }
        div { class: "flex flex-col gap-4",
            for (category, settings) in groups {
                div { key: "{category}",
                    div { class: "text-xs uppercase tracking-wide opacity-60 mb-1", "{category}" }
                    div { class: "overflow-x-auto",
                        table { class: "table table-sm",
                            tbody {
                                for decl in settings {
                                    SettingRow {
                                        key: "{decl.key}",
                                        decl: decl.clone(),
                                        effective: effective
                                            .as_ref()
                                            .and_then(|e| e.settings.get(&decl.key).cloned()),
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Declared settings by category, each category in the order the worker first
/// mentioned it.
///
/// Grouped rather than sorted: the worker's order puts related settings
/// together, and a category can be declared in two tables (the protocol's and
/// the executor's) without showing up twice.
fn group_by_category(declared: &[SettingDecl]) -> Vec<(String, Vec<&SettingDecl>)> {
    let mut groups: Vec<(String, Vec<&SettingDecl>)> = Vec::new();
    for decl in declared {
        match groups.iter_mut().find(|(name, _)| *name == decl.category) {
            Some((_, settings)) => settings.push(decl),
            None => groups.push((decl.category.clone(), vec![decl])),
        }
    }
    groups
}

/// One setting: what it is running, where that came from, and what it would
/// fall back to.
#[component]
fn SettingRow(decl: SettingDecl, effective: Option<EffectiveSetting>) -> Element {
    let rejected = effective
        .as_ref()
        .is_some_and(|e| e.status == SettingStatus::Rejected);
    let value = effective.as_ref().and_then(|e| e.value.clone());
    rsx! {
        tr {
            td { class: "align-top w-1/3",
                div { class: "font-mono text-sm", "{decl.key}" }
                div { class: "text-xs opacity-60", "{decl.description}" }
            }
            td { class: "align-top",
                match value {
                    Some(value) => rsx! {
                        span { class: "font-mono text-sm", "{value}" }
                    },
                    // Unset is a real answer for a budget or a limit: no cap at
                    // all, which is not the same as a cap of zero.
                    None => rsx! {
                        span { class: "opacity-40 text-sm", title: "Not set: no limit", "—" }
                    },
                }
            }
            td { class: "align-top text-xs w-64 text-right",
                if let Some(effective) = effective.as_ref() {
                    SourceNote { decl, effective: effective.clone() }
                } else {
                    span { class: "opacity-40", "not reported" }
                }
            }
        }
        if rejected && let Some(reason) = effective.and_then(|e| e.reason) {
            tr {
                td { colspan: 3, class: "pt-0",
                    div { class: "alert alert-warning alert-soft text-xs py-1",
                        "{reason}"
                    }
                }
            }
        }
    }
}

/// Where a value came from, in the operator's own vocabulary.
///
/// Names the variable rather than the concept: what someone needs in order to
/// change a value is the name they would edit on that machine, and "pinned by
/// the environment" does not tell them which line to look at.
#[component]
fn SourceNote(decl: SettingDecl, effective: EffectiveSetting) -> Element {
    let var = decl.env_var.clone().unwrap_or_default();
    let (label, class) = match effective.source {
        EffectiveSource::Env => (format!("pinned by {var}"), "badge-warning"),
        EffectiveSource::EnvDefault => (format!("{var}_DEFAULT"), "badge-ghost"),
        EffectiveSource::Server => ("set here".to_string(), "badge-info"),
        EffectiveSource::Default => ("built-in default".to_string(), "badge-ghost"),
    };
    rsx! {
        div { class: "flex flex-col gap-1 items-end text-right",
            span { class: "badge {class} badge-sm font-mono whitespace-nowrap", "{label}" }
            // Only worth saying where it differs from what is running: repeating
            // the value as its own fallback is noise on most rows.
            if let Some(fallback) = decl.default.as_ref()
                && effective.value.as_ref() != Some(fallback)
            {
                span { class: "opacity-60 whitespace-nowrap", "falls back to {fallback}" }
            }
            if effective.status == SettingStatus::Overridden {
                span { class: "opacity-60", "a value is set here but not in effect" }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Resolved, group_by_category, name_segments, resolve, shared_names, short_fingerprint,
        worker_route,
    };
    use crate::routes::Route;
    use aurcache_client::{ApprovalStatus, Worker as WorkerRow};
    use aurcache_common::worker_config::{Applies, SettingDecl, ValueKind};

    fn worker(id: i32, name: &str, fingerprint: &str, status: ApprovalStatus) -> WorkerRow {
        WorkerRow {
            id,
            name: name.to_string(),
            status,
            cert_fingerprint: fingerprint.to_string(),
            native_arches: vec!["x86_64".to_string()],
            emulated_arches: Vec::new(),
            package_affinity: Vec::new(),
            priority: 0,
            last_seen: None,
            version: None,
            kind: None,
            online: false,
            active_builds: 0,
            successful_builds: 0,
            failed_builds: 0,
            settings_rejected: None,
        }
    }

    #[test]
    fn a_name_nothing_answers_to_resolves_to_nothing() {
        let fleet = [worker(
            1,
            "builder-01",
            "aaaa1111",
            ApprovalStatus::Approved,
        )];
        assert_eq!(resolve(&fleet, "builder-02", ""), Resolved::None);
    }

    #[test]
    fn a_unique_name_is_enough() {
        let fleet = [
            worker(1, "builder-01", "aaaa1111", ApprovalStatus::Approved),
            worker(2, "builder-arm", "bbbb2222", ApprovalStatus::Approved),
        ];
        assert_eq!(resolve(&fleet, "builder-01", ""), Resolved::One(&fleet[0]));
    }

    /// The case that rules out a unique-name constraint: a machine is replaced
    /// by another with the same hostname, and the retired row keeps its name.
    /// Neither is the obvious answer, so the URL has to say which.
    #[test]
    fn a_shared_name_resolves_to_all_of_them() {
        let fleet = [
            worker(1, "builder-01", "aaaa1111", ApprovalStatus::Revoked),
            worker(2, "builder-01", "bbbb2222", ApprovalStatus::Approved),
        ];
        assert_eq!(
            resolve(&fleet, "builder-01", ""),
            Resolved::Several(vec![&fleet[0], &fleet[1]])
        );
        // The fingerprint picks one out on its own, case-insensitively.
        assert_eq!(resolve(&fleet, "", "bbbb"), Resolved::One(&fleet[1]));
        assert_eq!(resolve(&fleet, "", "BBBB"), Resolved::One(&fleet[1]));
    }

    /// A fingerprint nothing starts with resolves to nothing, rather than
    /// widening back to every worker.
    #[test]
    fn a_fingerprint_matching_none_of_them_resolves_to_nothing() {
        let fleet = [
            worker(1, "builder-01", "aaaa1111", ApprovalStatus::Approved),
            worker(2, "builder-01", "bbbb2222", ApprovalStatus::Approved),
        ];
        assert_eq!(resolve(&fleet, "", "cccc"), Resolved::None);
    }

    /// Links from the list never land on the chooser: the bare name where it is
    /// unambiguous, the fingerprint too where it is not.
    #[test]
    fn a_link_is_bare_until_the_name_is_shared() {
        let unique = [worker(
            1,
            "builder-01",
            "aaaa1111",
            ApprovalStatus::Approved,
        )];
        let shared = shared_names(&unique);
        assert!(shared.is_empty());
        assert_eq!(
            worker_route(&unique[0], shared.contains("builder-01")),
            Route::Worker {
                name: vec!["builder-01".to_string()]
            }
        );

        let fleet = [
            worker(1, "builder-01", "aaaa11112222", ApprovalStatus::Revoked),
            worker(2, "builder-01", "bbbb22223333", ApprovalStatus::Approved),
            worker(3, "builder-arm", "cccc33334444", ApprovalStatus::Approved),
        ];
        let shared = shared_names(&fleet);
        assert_eq!(shared, ["builder-01"].into_iter().collect());
        assert_eq!(
            worker_route(&fleet[1], shared.contains("builder-01")),
            Route::WorkerByFingerprint {
                fingerprint: "bbbb22223333".to_string(),
            }
        );
        // Everyone else still links by name.
        assert!(matches!(
            worker_route(&fleet[2], shared.contains("builder-arm")),
            Route::Worker { .. }
        ));
    }

    /// A name with a slash in it goes into the URL in one piece and comes back
    /// the same. Nothing stops `WORKER_NAME` containing one, and a name read
    /// back short is a page about the wrong machine.
    #[test]
    fn a_name_with_a_slash_survives_the_url() {
        let fleet = [worker(1, "ci/runner", "aaaa1111", ApprovalStatus::Approved)];
        let Route::Worker { name } = worker_route(&fleet[0], false) else {
            panic!("a unique name should link by name");
        };
        assert_eq!(name, vec!["ci".to_string(), "runner".to_string()]);
        assert_eq!(name.join("/"), "ci/runner");
        assert_eq!(name_segments("ci/runner"), name);
    }

    /// Short enough to read, and never longer than the fingerprint it came
    /// from -- the fixtures and tests use short ones.
    #[test]
    fn a_short_fingerprint_is_a_prefix() {
        assert_eq!(short_fingerprint("0123456789abcdef0123"), "0123456789ab");
        assert_eq!(short_fingerprint("abc"), "abc");
        assert_eq!(short_fingerprint(""), "");
    }

    fn decl(key: &str, category: &str) -> SettingDecl {
        SettingDecl {
            key: key.to_string(),
            kind: ValueKind::Text,
            description: String::new(),
            category: category.to_string(),
            default: None,
            env_var: None,
            applies: Applies::NextJob,
        }
    }

    /// A category is declared in two tables -- the protocol's and the
    /// executor's -- so grouping has to collect both under one heading rather
    /// than starting a second one when the category changes back.
    #[test]
    fn a_category_declared_twice_is_shown_once() {
        let declared = [
            decl("build_timeout", "Build limits"),
            decl("keyserver", "Signatures"),
            decl("build_memory_max", "Build limits"),
        ];
        let groups = group_by_category(&declared);
        let names: Vec<_> = groups.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["Build limits", "Signatures"]);
        assert_eq!(groups[0].1.len(), 2);
    }

    /// Categories appear in the order the worker first mentioned them, which is
    /// the order it grouped related settings in.
    #[test]
    fn categories_keep_the_workers_order() {
        let declared = [
            decl("concurrency", "Scheduling"),
            decl("cache_ttl", "Caches"),
        ];
        let groups = group_by_category(&declared);
        assert_eq!(groups[0].0, "Scheduling");
        assert_eq!(groups[1].0, "Caches");
    }
}
