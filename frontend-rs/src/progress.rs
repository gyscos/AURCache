//! Package adds that outlive the dialog which started them.
//!
//! Adding a package can take a while — the server resolves dependencies and
//! reaches the AUR for each one — and the dialog used to sit there spinning
//! until it finished, with the rest of the site unreachable behind it. Nothing
//! required that: the bulk add returns a job id immediately and its progress is
//! a stored log read by offset, explicitly so an observer can attach late or go
//! away without the job noticing.
//!
//! So the dialog closes as soon as the work is handed over, and the progress
//! appears as a card in the corner that can be dismissed. Dismissing stops
//! watching, not adding — the job runs on the server either way.
//!
//! The execution lives here rather than in the dialog because the dialog is
//! unmounted by then: a future spawned by a component dies with it, and the
//! card is what survives to own the polling.

use std::collections::BTreeMap;
use std::time::Duration;

use crate::listing::ViewParams;
use aurcache_client::{
    AddPackageRequest, AddPackagesRequest, BulkAddOutcome, RestoreOutcome, SourceData,
};
use dioxus::prelude::*;

/// How often to ask a running bulk add what it has done.
const POLL_INTERVAL: Duration = Duration::from_millis(700);

/// What the dialog hands over when it closes.
#[derive(Clone, PartialEq)]
pub struct AddRequest {
    pub sources: Vec<SourceData>,
    pub platforms: Vec<String>,
    /// File edits, which only ever accompany a single source and go through
    /// the endpoint that accepts them rather than the bulk one.
    pub patched: BTreeMap<String, String>,
}

/// What a card has to do.
#[derive(Clone, PartialEq)]
pub enum Work {
    /// Nothing has been sent yet: submit it, then follow the job.
    Submit(AddRequest),
    /// A bulk add already running server-side -- picked up from the operations
    /// list -- so there is nothing to send and the entries so far are replayed
    /// from offset zero.
    FollowAdd { operation: i32, label: String },
    /// A restore already running. Reported separately from an add because the
    /// two say genuinely different things: a package was added or already
    /// existed, versus a package was imported, skipped, overwritten or had its
    /// patch adopted. Flattening them into one word would lose that.
    FollowRestore { operation: i32, label: String },
}

/// One add being watched.
#[derive(Clone, PartialEq)]
pub struct Job {
    pub id: u64,
    /// How many sources were handed over, for "3 of 5".
    pub total: usize,
    /// What finished successfully, each with the word for what happened to it:
    /// "added" or "existed" for an add, "imported", "skipped", "overwritten" or
    /// "patch adopted" for a restore. Kept per item rather than as counters so
    /// the card can summarise in the kind's own vocabulary.
    pub succeeded: Vec<(String, String)>,
    pub failed: Vec<(String, String)>,
    /// The source being worked on, when the job reports one.
    pub current: Option<String>,
    pub finished: bool,
    /// Set once a card has taken ownership, so a re-render cannot start the
    /// same add twice.
    pub started: bool,
    pub work: Work,
}

impl Job {
    /// Sources resolved so far, however they turned out.
    #[must_use]
    pub fn resolved_count(&self) -> usize {
        self.succeeded.len() + self.failed.len()
    }

    /// How the finished work reads, in the vocabulary of what ran.
    ///
    /// Counts per outcome rather than a bare total: "12 imported, 3 skipped"
    /// answers a question that "15 done" does not, and for a restore the
    /// difference between the two is the whole point of running it.
    #[must_use]
    pub fn summary(&self) -> String {
        let mut counts: Vec<(String, usize)> = Vec::new();
        for (_, outcome) in &self.succeeded {
            match counts.iter_mut().find(|(word, _)| word == outcome) {
                Some((_, n)) => *n += 1,
                None => counts.push((outcome.clone(), 1)),
            }
        }
        if !self.failed.is_empty() {
            counts.push(("failed".to_string(), self.failed.len()));
        }
        counts
            .into_iter()
            .map(|(word, n)| format!("{n} {word}"))
            .collect::<Vec<_>>()
            .join(", ")
    }

    #[must_use]
    pub fn title(&self) -> String {
        match &self.work {
            Work::Submit(request) => match (self.total, request.sources.first()) {
                (1, Some(source)) => format!("Adding {}", source_label(source)),
                (n, _) => format!("Adding {n} packages"),
            },
            Work::FollowAdd { label, .. } | Work::FollowRestore { label, .. } => label.clone(),
        }
    }

    /// The operation this card is watching, once there is one.
    ///
    /// Only a followed job knows it up front; one being submitted learns it
    /// from the response, which is why re-attaching to an add started in this
    /// same browser is not offered -- its card is already on screen.
    #[must_use]
    pub fn operation_id(&self) -> Option<i32> {
        match &self.work {
            Work::FollowAdd { operation, .. } | Work::FollowRestore { operation, .. } => {
                Some(*operation)
            }
            Work::Submit(_) => None,
        }
    }
}

/// Every add currently on screen.
///
/// Provided above the router so the cards survive navigation: the whole point
/// is that someone can carry on using the site while an add runs.
pub fn use_jobs_provider() -> Signal<Vec<Job>> {
    use_context_provider(|| Signal::new(Vec::new()))
}

pub fn use_jobs() -> Signal<Vec<Job>> {
    use_context()
}

pub fn start_add(mut jobs: Signal<Vec<Job>>, request: AddRequest) {
    // Monotonic rather than an index: cards are removed as they are dismissed,
    // so positions are not stable identities.
    static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    jobs.push(Job {
        id,
        total: request.sources.len(),
        succeeded: Vec::new(),
        failed: Vec::new(),
        current: None,
        finished: false,
        started: false,
        work: Work::Submit(request),
    });
}

/// Replays from offset zero rather than from the current counters, so the card
/// shows what happened before it was opened rather than only what happens next.
pub fn watch_add(jobs: Signal<Vec<Job>>, operation: i32, total: i32, label: String) {
    watch_operation(jobs, total, Work::FollowAdd { operation, label });
}

pub fn watch_restore(jobs: Signal<Vec<Job>>, operation: i32, total: i32, label: String) {
    watch_operation(jobs, total, Work::FollowRestore { operation, label });
}

fn watch_operation(mut jobs: Signal<Vec<Job>>, total: i32, work: Work) {
    let operation = match &work {
        Work::FollowAdd { operation, .. } | Work::FollowRestore { operation, .. } => *operation,
        Work::Submit(_) => return,
    };
    // Already watching it: opening a second card for one job would poll twice
    // and show two answers to the same question.
    if jobs
        .read()
        .iter()
        .any(|job| job.operation_id() == Some(operation))
    {
        return;
    }

    static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1 << 32);
    let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    jobs.push(Job {
        id,
        total: usize::try_from(total).unwrap_or(0),
        succeeded: Vec::new(),
        failed: Vec::new(),
        current: None,
        finished: false,
        started: false,
        work,
    });
}

/// Stop showing a card. The add itself is the server's business and continues.
pub fn dismiss_card(mut jobs: Signal<Vec<Job>>, id: u64) {
    jobs.write().retain(|job| job.id != id);
}

/// Apply `f` to one job, if it is still on screen.
///
/// Every update goes through this because a card can be dismissed mid-add, and
/// writing to an index that has since shifted would corrupt a different job.
fn update_job(mut jobs: Signal<Vec<Job>>, id: u64, f: impl FnOnce(&mut Job)) {
    if let Some(job) = jobs.write().iter_mut().find(|job| job.id == id) {
        f(job);
    }
}

/// Whether a job is still being shown; a dismissed one should stop polling.
fn still_watched(jobs: Signal<Vec<Job>>, id: u64) -> bool {
    jobs.read().iter().any(|job| job.id == id)
}

/// The label a failure is reported against, matching what the request carried.
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
        // This dialog never builds one -- the upload it belongs to was never
        // implemented server-side -- but the variant exists, so it gets a label
        // rather than a panic.
        SourceData::Upload { .. } => "uploaded archive".to_string(),
    }
}

/// Add every source in one batched request, then follow the job.
///
/// Batched because resolving each source to its pkgbase is an AUR request, and
/// one call resolves the lot where adding them one at a time spends a request
/// per package before fetching anything.
async fn submit_bulk_add(jobs: Signal<Vec<Job>>, id: u64, request: AddRequest) {
    let client = match crate::api::client() {
        Ok(client) => client,
        Err(e) => {
            update_job(jobs, id, |job| {
                job.failed.push((String::new(), e));
                job.finished = true;
            });
            return;
        }
    };

    let labels: Vec<String> = request.sources.iter().map(source_label).collect();

    let accepted = match client
        .add_packages(&AddPackagesRequest {
            platforms: Some(request.platforms.clone()),
            build_flags: None,
            sources: request.sources.clone(),
        })
        .await
    {
        Ok(accepted) => accepted,
        Err(e) => {
            update_job(jobs, id, |job| {
                job.failed.push((String::new(), e.to_string()));
                job.finished = true;
            });
            return;
        }
    };

    let mut seen = 0_usize;
    loop {
        // Dismissed: stop asking. The job carries on server-side.
        if !still_watched(jobs, id) {
            return;
        }

        let progress = match client.bulk_add_progress(accepted.job_id, seen).await {
            Ok(progress) => progress,
            // The job is still running and we have merely lost sight of it, so
            // say that rather than reporting packages as failed, which they
            // are not.
            Err(e) => {
                update_job(jobs, id, |job| {
                    job.failed
                        .push((String::new(), format!("lost track of the add: {e}")));
                    job.finished = true;
                });
                return;
            }
        };

        seen += progress.entries.len();
        for entry in progress.entries {
            let landed = !matches!(entry.outcome, BulkAddOutcome::Failed { .. });
            update_job(jobs, id, |job| match entry.outcome {
                BulkAddOutcome::Added => job.succeeded.push((entry.name, "added".to_string())),
                // Distinguished from added: nothing changed, which is a
                // different answer to "did that work" than a fresh add.
                BulkAddOutcome::Existed => {
                    job.succeeded.push((entry.name, "already here".to_string()));
                }
                BulkAddOutcome::Failed { error } => job.failed.push((entry.name, error)),
            });
            if landed {
                // A package the list, if someone has it open, does not know
                // about yet. Let it re-fetch now rather than on its next tick.
                crate::poll::packages_changed();
            }
        }

        if progress.finished {
            update_job(jobs, id, |job| {
                job.current = None;
                job.finished = true;
            });
            return;
        }

        // The job works through the list in order, so once N outcomes are in,
        // the one in flight is the N-th.
        let current = labels.get(seen).cloned();
        update_job(jobs, id, |job| job.current = current);
        gloo_timers::future::sleep(POLL_INTERVAL).await;
    }
}

/// Follow a job that is already running, from the beginning of its log.
///
/// The same loop `submit_bulk_add` ends in, without the request that starts one. The
/// entries are replayed from offset zero so a card opened halfway through shows
/// what already happened -- which is the point of the log being a record rather
/// than a stream.
async fn poll_bulk_add(jobs: Signal<Vec<Job>>, id: u64, operation: i32) {
    let client = match crate::api::client() {
        Ok(client) => client,
        Err(e) => {
            update_job(jobs, id, |job| {
                job.failed.push((String::new(), e));
                job.finished = true;
            });
            return;
        }
    };

    let mut seen = 0_usize;
    loop {
        if !still_watched(jobs, id) {
            return;
        }

        let progress = match client.bulk_add_progress(operation, seen).await {
            Ok(progress) => progress,
            Err(e) => {
                update_job(jobs, id, |job| {
                    job.failed
                        .push((String::new(), format!("lost track of the add: {e}")));
                    job.finished = true;
                });
                return;
            }
        };

        seen += progress.entries.len();
        for entry in progress.entries {
            let landed = !matches!(entry.outcome, BulkAddOutcome::Failed { .. });
            update_job(jobs, id, |job| match entry.outcome {
                BulkAddOutcome::Added => job.succeeded.push((entry.name, "added".to_string())),
                // Distinguished from added: nothing changed, which is a
                // different answer to "did that work" than a fresh add.
                BulkAddOutcome::Existed => {
                    job.succeeded.push((entry.name, "already here".to_string()));
                }
                BulkAddOutcome::Failed { error } => job.failed.push((entry.name, error)),
            });
            if landed {
                // A package the list, if someone has it open, does not know
                // about yet. Let it re-fetch now rather than on its next tick.
                crate::poll::packages_changed();
            }
        }

        // The total is whatever the job says, not what the list said when the
        // card was opened: a job can only be followed after it started, so the
        // server is the authority on how big it is.
        let total = usize::try_from(progress.total).unwrap_or(0);
        update_job(jobs, id, |job| job.total = total);

        if progress.finished {
            update_job(jobs, id, |job| {
                job.current = None;
                job.finished = true;
            });
            return;
        }
        gloo_timers::future::sleep(POLL_INTERVAL).await;
    }
}

/// Follow a running restore.
///
/// The same loop as an add's, against the restore endpoint and its own
/// vocabulary. Kept as a second function rather than made generic over the
/// entry type: the two share a transport and nothing else, and one function
/// covering both would be a match on the kind in every branch anyway.
async fn poll_restore(jobs: Signal<Vec<Job>>, id: u64, operation: i32) {
    let client = match crate::api::client() {
        Ok(client) => client,
        Err(e) => {
            update_job(jobs, id, |job| {
                job.failed.push((String::new(), e));
                job.finished = true;
            });
            return;
        }
    };

    let mut seen = 0_usize;
    loop {
        if !still_watched(jobs, id) {
            return;
        }

        let progress = match client.restore_progress(operation, seen).await {
            Ok(progress) => progress,
            Err(e) => {
                update_job(jobs, id, |job| {
                    job.failed
                        .push((String::new(), format!("lost track of the restore: {e}")));
                    job.finished = true;
                });
                return;
            }
        };

        seen += progress.entries.len();
        for entry in progress.entries {
            // "skipped" changed nothing, but the others did; a spare re-fetch
            // for a skip is cheaper than branching four ways here.
            let landed = !matches!(entry.outcome, RestoreOutcome::Failed { .. });
            update_job(jobs, id, |job| match entry.outcome {
                RestoreOutcome::Imported => {
                    job.succeeded.push((entry.pkgbase, "imported".to_string()));
                }
                RestoreOutcome::Skipped => {
                    job.succeeded.push((entry.pkgbase, "skipped".to_string()));
                }
                RestoreOutcome::Overwritten => {
                    job.succeeded
                        .push((entry.pkgbase, "overwritten".to_string()));
                }
                RestoreOutcome::PatchAdopted => {
                    job.succeeded
                        .push((entry.pkgbase, "patch adopted".to_string()));
                }
                RestoreOutcome::Failed { error } => job.failed.push((entry.pkgbase, error)),
            });
            if landed {
                crate::poll::packages_changed();
            }
        }

        let total = usize::try_from(progress.total).unwrap_or(0);
        update_job(jobs, id, |job| job.total = total);

        if progress.finished {
            update_job(jobs, id, |job| {
                job.current = None;
                job.finished = true;
            });
            return;
        }
        gloo_timers::future::sleep(POLL_INTERVAL).await;
    }
}

/// Add sources that carry file edits, one at a time.
///
/// The bulk request has nowhere to put file edits, and there is only ever one
/// source when there are any, so this is a single add by definition.
async fn add_patched_sources(jobs: Signal<Vec<Job>>, id: u64, request: AddRequest) {
    let client = match crate::api::client() {
        Ok(client) => client,
        Err(e) => {
            update_job(jobs, id, |job| {
                job.failed.push((String::new(), e));
                job.finished = true;
            });
            return;
        }
    };

    for source in request.sources {
        if !still_watched(jobs, id) {
            return;
        }
        let label = source_label(&source);
        update_job(jobs, id, |job| job.current = Some(label.clone()));

        let result = client
            .add_package(&AddPackageRequest {
                platforms: Some(request.platforms.clone()),
                build_flags: None,
                source,
                patched_files: Some(request.patched.clone()),
            })
            .await;

        let landed = result.is_ok();
        update_job(jobs, id, |job| match result {
            Ok(()) => job.succeeded.push((label, "added".to_string())),
            Err(e) => job.failed.push((label, e.to_string())),
        });
        if landed {
            crate::poll::packages_changed();
        }
    }

    update_job(jobs, id, |job| {
        job.current = None;
        job.finished = true;
    });
}

/// The stack of add cards, mounted once above the router.
#[component]
pub fn ProgressOverlay() -> Element {
    let jobs = use_jobs();
    let ids: Vec<u64> = jobs.read().iter().map(|job| job.id).collect();

    if ids.is_empty() {
        return rsx! {};
    }

    rsx! {
        // Above the drawer and the modal backdrop, and out of the way of the
        // content: this is deliberately not something to be dealt with before
        // the site can be used again.
        div { class: "fixed bottom-4 right-4 z-50 flex flex-col gap-2 w-80 max-w-[calc(100vw-2rem)]",
            for id in ids {
                JobCard { key: "{id}", id }
            }
        }
    }
}

/// One add's card, which also owns the work.
///
/// The future belongs to the card rather than to the dialog because the dialog
/// is gone by now; a future spawned in a component dies when that component
/// unmounts, and this one lives as long as the card is on screen.
#[component]
fn JobCard(id: u64) -> Element {
    let mut jobs = use_jobs();

    use_future(move || async move {
        // Claimed exactly once. `use_future` runs on mount, but a re-render
        // that remounted the card would otherwise start the same add again.
        let work = {
            let mut guard = jobs.write();
            let Some(job) = guard.iter_mut().find(|job| job.id == id) else {
                return;
            };
            if job.started {
                return;
            }
            job.started = true;
            job.work.clone()
        };

        match work {
            Work::Submit(request) if request.patched.is_empty() => {
                submit_bulk_add(jobs, id, request).await;
            }
            Work::Submit(request) => add_patched_sources(jobs, id, request).await,
            Work::FollowAdd { operation, .. } => poll_bulk_add(jobs, id, operation).await,
            Work::FollowRestore { operation, .. } => {
                poll_restore(jobs, id, operation).await;
            }
        }
    });

    let Some(job) = jobs.read().iter().find(|job| job.id == id).cloned() else {
        return rsx! {};
    };

    let done = job.resolved_count();
    let title = job.title();
    let failed = !job.failed.is_empty();

    rsx! {
        div { class: "card bg-base-100 shadow-lg border border-base-300",
            div { class: "card-body p-3 gap-2",
                div { class: "flex items-start gap-2",
                    if !job.finished {
                        span { class: "loading loading-spinner loading-xs mt-1" }
                    } else if failed {
                        span { class: "badge badge-error badge-xs mt-1" }
                    } else {
                        span { class: "badge badge-success badge-xs mt-1" }
                    }

                    div { class: "flex-1 min-w-0",
                        div { class: "font-medium text-sm truncate", "{title}" }
                        div { class: "text-xs opacity-70",
                            if job.finished {
                                {job.summary()}
                            } else {
                                "{done} of {job.total}"
                            }
                        }
                    }

                    button {
                        class: "btn btn-ghost btn-xs btn-circle",
                        // Dismissing stops watching, not adding; the server
                        // finishes the job either way.
                        title: if job.finished { "Dismiss" } else { "Hide (the add continues)" },
                        onclick: move |_| dismiss_card(jobs, id),
                        "✕"
                    }
                }

                // Only while there is more to come: the name of the last
                // package is not interesting once the job is done.
                if let Some(current) = job.current.clone()
                    && !job.finished
                {
                    div { class: "text-xs opacity-60 truncate", "{current}" }
                }

                if job.total > 1 && !job.finished {
                    progress {
                        class: "progress progress-primary w-full h-1",
                        value: "{done}",
                        max: "{job.total}",
                    }
                }

                // Failures are the one thing worth taking space for: a package
                // that did not add is something to act on.
                if failed {
                    div { class: "text-xs space-y-1 max-h-32 overflow-y-auto",
                        for (name , error) in job.failed.clone() {
                            div { class: "text-error break-words",
                                if name.is_empty() {
                                    "{error}"
                                } else {
                                    "{name}: {error}"
                                }
                            }
                        }
                    }
                }

                if job.finished && !failed {
                    Link {
                        class: "btn btn-ghost btn-xs self-start",
                        to: crate::routes::Route::Packages { view: ViewParams::default(), q: String::new() },
                        "View packages"
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aurcache_client::GitSourceSpec;

    fn aur(name: &str) -> SourceData {
        SourceData::Aur {
            name: name.to_string(),
        }
    }

    fn job(sources: Vec<SourceData>) -> Job {
        Job {
            id: 1,
            total: sources.len(),
            succeeded: Vec::new(),
            failed: Vec::new(),
            current: None,
            finished: false,
            started: false,
            work: Work::Submit(AddRequest {
                sources,
                platforms: vec!["x86_64".to_string()],
                patched: BTreeMap::new(),
            }),
        }
    }

    /// One package is named; several are counted. A card is small, and a list
    /// of names would not fit where the count always does.
    #[test]
    fn the_title_names_a_single_package_and_counts_the_rest() {
        assert_eq!(job(vec![aur("hello")]).title(), "Adding hello");
        assert_eq!(
            job(vec![aur("hello"), aur("neofetch")]).title(),
            "Adding 2 packages"
        );
    }

    /// A git source has no name to show, so the card falls back to the remote,
    /// which is what someone typed and will recognise.
    #[test]
    fn a_git_source_is_labelled_by_its_remote() {
        let source = SourceData::Git {
            spec: GitSourceSpec {
                url: "https://example.com/pkg.git".to_string(),
                r#ref: "main".to_string(),
                subfolder: "sub".to_string(),
            },
        };
        assert_eq!(
            source_label(&source),
            "https://example.com/pkg.git#main/sub"
        );
        assert_eq!(
            job(vec![source]).title(),
            "Adding https://example.com/pkg.git#main/sub"
        );
    }

    /// A followed job is titled by what the list said, because the sources are
    /// the server's and were never in this browser.
    #[test]
    fn a_followed_job_keeps_the_label_it_was_opened_with() {
        let followed = Job {
            id: 2,
            total: 4,
            succeeded: Vec::new(),
            failed: Vec::new(),
            current: None,
            finished: false,
            started: false,
            work: Work::FollowAdd {
                operation: 7,
                label: "Adding 4 packages".to_string(),
            },
        };
        assert_eq!(followed.title(), "Adding 4 packages");
        assert_eq!(followed.operation_id(), Some(7));
        // A submitted job has no operation until the server answers, which is
        // why re-attaching is only offered for jobs this browser did not start.
        assert_eq!(job(vec![aur("hello")]).operation_id(), None);
    }

    /// A restore reports in its own words, which is why outcomes are carried
    /// per item rather than as one "done" counter: "12 imported, 3 skipped"
    /// answers a question that "15 done" does not.
    #[test]
    fn the_summary_counts_each_outcome_separately() {
        let mut j = job(vec![aur("a")]);
        j.succeeded.push(("one".into(), "imported".into()));
        j.succeeded.push(("two".into(), "imported".into()));
        j.succeeded.push(("three".into(), "skipped".into()));
        j.failed.push(("four".into(), "bad patch".into()));

        assert_eq!(j.summary(), "2 imported, 1 skipped, 1 failed");
    }

    /// An add's vocabulary is its own: a package that was already there did not
    /// get added, and saying so is the difference between the two answers.
    #[test]
    fn an_add_distinguishes_added_from_already_here() {
        let mut j = job(vec![aur("a"), aur("b")]);
        j.succeeded.push(("a".into(), "added".into()));
        j.succeeded.push(("b".into(), "already here".into()));
        assert_eq!(j.summary(), "1 added, 1 already here");
    }

    /// Progress counts what has been resolved either way. A failure is as
    /// finished as a success, and a bar that only counted successes would
    /// stall on a run where something failed.
    #[test]
    fn progress_counts_failures_as_resolved() {
        let mut j = job(vec![aur("a"), aur("b"), aur("c")]);
        assert_eq!(j.resolved_count(), 0);
        j.succeeded.push(("a".to_string(), "added".to_string()));
        j.failed
            .push(("b".to_string(), "no such package".to_string()));
        assert_eq!(j.resolved_count(), 2);
    }
}
