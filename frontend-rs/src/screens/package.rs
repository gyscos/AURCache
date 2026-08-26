//! One package: its state, what it depends on, and where it came from.
//!
//! Built around the question someone actually arrives with — "is this package
//! healthy, and if not what do I do" — rather than around the data model. The
//! header answers it; everything below is reference material.
//!
//! Deliberately not a list of builds. That was the old centrepiece, and it
//! buried the two builds that matter: the most recent one, and the one whose
//! output is actually in the repository. The full history is a click away.

use crate::api::client;
use crate::format::{format_age, format_duration, now_secs};
use crate::routes::Route;
use crate::status::{BuildStatusBadge, StatusBadge};
use aurcache_client::{Build, ExtendedPackage, PackageSource};
use aurcache_types::build_state::BuildState;
use dioxus::prelude::*;

/// How many builds to pull for the summary. Enough for a stable typical
/// duration without fetching a long history the page does not show.
const BUILD_SAMPLE: u64 = 20;

/// The most recent build, whatever its outcome.
///
/// The API returns builds newest-first, but that is an ordering this screen
/// depends on for correctness, so it picks the maximum explicitly rather than
/// trusting position.
fn latest(builds: &[Build]) -> Option<&Build> {
    builds.iter().max_by_key(|b| (b.start_time, b.id))
}

/// The newest build that succeeded — the one whose packages are in the repo.
fn in_repo(builds: &[Build]) -> Option<&Build> {
    builds
        .iter()
        .filter(|b| matches!(BuildState::from_i32(b.status), Some(BuildState::Successful)))
        .max_by_key(|b| (b.end_time, b.id))
}

/// How long a build of this package usually takes.
///
/// Median rather than mean: one pathological build (a machine that swapped, a
/// dependency that rebuilt the world) drags an average somewhere no build has
/// ever actually landed.
///
/// Only successful builds count. A failure's duration measures how quickly
/// something broke, which is a different quantity and usually much shorter.
fn typical_duration(builds: &[Build]) -> Option<i64> {
    let mut durations: Vec<i64> = builds
        .iter()
        .filter(|b| matches!(BuildState::from_i32(b.status), Some(BuildState::Successful)))
        .filter_map(|b| match (b.start_time, b.end_time) {
            (Some(start), Some(end)) if end >= start => Some(end - start),
            _ => None,
        })
        .collect();

    if durations.is_empty() {
        return None;
    }
    durations.sort_unstable();
    // Even counts take the lower of the two middle values rather than
    // averaging: the result stays a duration some build actually took.
    Some(durations[(durations.len() - 1) / 2])
}

/// The package names this build produces.
///
/// A split package declares them; anything else produces one package named
/// after its pkgbase.
fn produced_names(pkg: &ExtendedPackage) -> Vec<String> {
    match &pkg.split_packages {
        Some(names) if !names.is_empty() => names.clone(),
        _ => vec![pkg.name.clone()],
    }
}

async fn load(pkgbase: String) -> Result<(ExtendedPackage, Vec<Build>), String> {
    let client = client()?;
    let package = client
        .get_package(&pkgbase)
        .await
        .map_err(|e| e.to_string())?;
    // A failure here should not lose the package itself: the build summary is
    // secondary, and the rest of the page is still worth showing.
    let builds = client
        .list_builds(Some(&pkgbase), Some(BUILD_SAMPLE), None)
        .await
        .unwrap_or_default();
    Ok((package, builds))
}

#[component]
pub fn Package(pkgbase: String) -> Element {
    let data = use_resource({
        let pkgbase = pkgbase.clone();
        move || load(pkgbase.clone())
    });

    rsx! {
        match &*data.read_unchecked() {
            None => rsx! {
                div { class: "flex justify-center p-16",
                    span { class: "loading loading-spinner loading-lg" }
                }
            },
            Some(Err(e)) => rsx! {
                div { class: "alert alert-error", span { "Could not load {pkgbase}: {e}" } }
            },
            Some(Ok((pkg, builds))) => rsx! {
                div { class: "space-y-4",
                    PackageHeader { pkg: pkg.clone() }
                    BuildSummary { pkgbase: pkg.name.clone(), builds: builds.to_vec() }
                    div { class: "grid grid-cols-1 lg:grid-cols-3 gap-4",
                        // Dependency lists are unbounded — thirty entries is
                        // ordinary — so they take the wide column. The metadata
                        // beside them is always the same handful of short
                        // fields and fits a fixed sidebar.
                        div { class: "lg:col-span-2 space-y-4", Relations { pkg: pkg.clone() } }
                        div { class: "space-y-4",
                            SourceCard { pkg: pkg.clone() }
                            BuildConfigCard { pkg: pkg.clone() }
                            ProducesCard { pkg: pkg.clone() }
                        }
                    }
                }
            },
        }
    }
}

#[component]
fn PackageHeader(pkg: ExtendedPackage) -> Element {
    let description = match &pkg.package_source {
        PackageSource::Aur(aur) => aur.description.clone(),
        _ => None,
    };

    rsx! {
        div { class: "card bg-base-100 shadow-xl",
            div { class: "card-body",
                div { class: "flex flex-wrap items-start gap-3",
                    div { class: "min-w-0",
                        div { class: "flex items-center gap-3 flex-wrap",
                            h1 { class: "text-2xl font-bold font-mono break-all", "{pkg.name}" }
                            StatusBadge { status: pkg.status, outofdate: pkg.outofdate }
                            if pkg.has_patch {
                                span { class: "badge badge-warning badge-sm", "patched" }
                            }
                            if !pkg.directly_requested {
                                span {
                                    class: "badge badge-outline badge-sm",
                                    title: "Pulled in as a dependency, not requested directly",
                                    "dependency"
                                }
                            }
                        }
                        if let Some(description) = description {
                            p { class: "opacity-70 mt-1", "{description}" }
                        }
                        VersionLine { pkg: pkg.clone() }
                    }
                    div { class: "flex-1" }
                    div { class: "flex gap-2",
                        button { class: "btn btn-primary btn-sm", "Rebuild" }
                        Link {
                            class: "btn btn-sm",
                            to: Route::PackageSource { pkgbase: pkg.name.clone(), path: vec![] },
                            "Edit sources"
                        }
                    }
                }
            }
        }
    }
}

/// What is built versus what is available upstream.
///
/// Rendered as one line rather than two labelled fields, because the only
/// reason to show both is the comparison between them.
#[component]
fn VersionLine(pkg: ExtendedPackage) -> Element {
    let built = pkg.latest_version.clone();
    let upstream = pkg.upstream_version.clone();

    rsx! {
        div { class: "mt-2 flex items-center gap-2 text-sm font-mono flex-wrap",
            match built {
                Some(built) => rsx! { span { "{built}" } },
                // Never built: say so rather than showing a bare dash, which
                // reads as missing data.
                None => rsx! { span { class: "opacity-60 italic font-sans", "never built" } },
            }
            match upstream {
                Some(upstream) => rsx! {
                    span { class: "opacity-40", "·" }
                    span { class: "opacity-70", "upstream {upstream}" }
                },
                None => rsx! {
                    span { class: "opacity-40", "·" }
                    span { class: "opacity-60 italic font-sans", "upstream not checked yet" }
                },
            }
        }
    }
}

/// The two builds worth knowing about, and how long a build usually takes.
#[component]
fn BuildSummary(pkgbase: String, builds: Vec<Build>) -> Element {
    let now = now_secs();
    let newest = latest(&builds);
    let repo = in_repo(&builds);
    // Only worth its own row when it is not the build already shown above. When
    // they differ, that gap is the story: the newest attempt failed and the
    // repository still holds something older.
    let show_repo = match (newest, repo) {
        (Some(newest), Some(repo)) => newest.id != repo.id,
        (None, Some(_)) => true,
        _ => false,
    };
    let typical = typical_duration(&builds);

    rsx! {
        div { class: "card bg-base-100 shadow-xl",
            div { class: "card-body py-4",
                div { class: "flex items-center gap-3 flex-wrap",
                    h2 { class: "card-title text-base", "Builds" }
                    div { class: "flex-1" }
                    if let Some(typical) = typical {
                        span { class: "text-sm opacity-60",
                            "typically {format_duration(Some(0), Some(typical))}"
                        }
                    }
                    Link {
                        class: "link link-primary text-sm",
                        to: Route::PackageBuilds { pkgbase: pkgbase.clone() },
                        "All builds →"
                    }
                }

                match newest {
                    None => rsx! {
                        p { class: "opacity-60 text-sm", "This package has never been built." }
                    },
                    Some(newest) => rsx! {
                        div { class: "divide-y divide-base-300",
                            BuildRow { label: "Latest", entry: newest.clone(), now }
                            if show_repo {
                                if let Some(repo) = repo {
                                    BuildRow { label: "In repo", entry: repo.clone(), now }
                                }
                            }
                        }
                    },
                }
            }
        }
    }
}

/// One line of the build summary.
///
/// "In repo" rather than "latest successful": the label says what it means for
/// the reader — this is the version pacman will install right now.
///
/// The prop is `entry`, not `build`: Dioxus generates a props builder whose own
/// `build()` finalises it, so a prop of that name is ambiguous at the call site.
#[component]
fn BuildRow(label: String, entry: Build, now: i64) -> Element {
    rsx! {
        Link {
            class: "flex items-center gap-3 py-2 hover:bg-base-200 px-2 -mx-2 rounded flex-wrap",
            to: Route::Build { id: entry.id },
            span { class: "text-sm opacity-60 w-20 shrink-0", "{label}" }
            span { class: "font-mono text-sm", "#{entry.id}" }
            BuildStatusBadge { status: entry.status }
            span { class: "font-mono text-sm opacity-70", "{entry.version}" }
            div { class: "flex-1" }
            span { class: "text-sm opacity-60", {format_age(entry.start_time, now)} }
            span { class: "font-mono text-sm opacity-60 w-16 text-right",
                {format_duration(entry.start_time, entry.end_time)}
            }
        }
    }
}

#[component]
fn Relations(pkg: ExtendedPackage) -> Element {
    rsx! {
        RelationList {
            title: "Dependencies",
            empty: "Nothing — this package builds on its own.",
            items: pkg.dependencies.clone(),
        }
        RelationList {
            title: "Dependents",
            empty: "Nothing depends on this package.",
            items: pkg.dependents.clone(),
        }
    }
}

#[component]
fn RelationList(
    title: String,
    empty: String,
    items: Vec<aurcache_client::PackageDependency>,
) -> Element {
    rsx! {
        div { class: "card bg-base-100 shadow-xl",
            div { class: "card-body",
                h2 { class: "card-title text-base",
                    "{title}"
                    if !items.is_empty() {
                        span { class: "badge badge-sm badge-neutral", "{items.len()}" }
                    }
                }
                if items.is_empty() {
                    p { class: "opacity-60 text-sm", "{empty}" }
                } else {
                    ul { class: "divide-y divide-base-300",
                        for item in items.iter() {
                            li { key: "{item.id}", class: "py-2 flex items-center gap-3",
                                Link {
                                    class: "link link-primary font-mono text-sm break-all",
                                    to: Route::Package { pkgbase: item.name.clone() },
                                    "{item.name}"
                                }
                                if !item.version_constraint.is_empty() {
                                    span { class: "font-mono text-xs opacity-60",
                                        "{item.version_constraint}"
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

#[component]
fn SourceCard(pkg: ExtendedPackage) -> Element {
    rsx! {
        div { class: "card bg-base-100 shadow-xl",
            div { class: "card-body",
                h2 { class: "card-title text-base", "Source" }
                match &pkg.package_source {
                    PackageSource::Aur(aur) => rsx! {
                        Field { label: "Origin",
                            a { class: "link link-primary", href: "{aur.aur_url}",
                                target: "_blank", rel: "noopener noreferrer", "AUR ↗" }
                        }
                        if let Some(maintainer) = aur.maintainer.clone() {
                            Field { label: "Maintainer", span { "{maintainer}" } }
                        }
                        if let Some(licenses) = aur.licenses.clone() {
                            Field { label: "Licenses", span { "{licenses}" } }
                        }
                        if let Some(url) = aur.project_url.clone() {
                            Field { label: "Upstream",
                                a { class: "link link-primary break-all", href: "{url}",
                                    target: "_blank", rel: "noopener noreferrer", "{url}" }
                            }
                        }
                        if aur.aur_flagged_outdated {
                            div { class: "alert alert-warning text-sm mt-2",
                                span { "Flagged out of date on the AUR." }
                            }
                        }
                    },
                    PackageSource::AurNotFound(_) => rsx! {
                        div { class: "alert alert-warning text-sm",
                            span { "No longer found on the AUR." }
                        }
                    },
                    PackageSource::Git(spec) => rsx! {
                        Field { label: "Origin", span { "Git" } }
                        Field { label: "URL",
                            span { class: "font-mono text-xs break-all", "{spec.url}" }
                        }
                        Field { label: "Ref", span { class: "font-mono text-xs", "{spec.r#ref}" } }
                        if !spec.subfolder.is_empty() {
                            Field { label: "Subfolder",
                                span { class: "font-mono text-xs", "{spec.subfolder}" }
                            }
                        }
                    },
                    PackageSource::Upload(_) => rsx! {
                        Field { label: "Origin", span { "Uploaded archive" } }
                    },
                }
            }
        }
    }
}

#[component]
fn BuildConfigCard(pkg: ExtendedPackage) -> Element {
    rsx! {
        div { class: "card bg-base-100 shadow-xl",
            div { class: "card-body",
                h2 { class: "card-title text-base", "Build config" }
                Field { label: "Platforms",
                    span { class: "font-mono text-xs", "{pkg.selected_platforms.join(\", \")}" }
                }
                {
                    let flags: Vec<String> = pkg
                        .selected_build_flags
                        .clone()
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|flag| !flag.trim().is_empty())
                        .collect();
                    (!flags.is_empty()).then(|| rsx! {
                        Field { label: "Flags",
                            span { class: "font-mono text-xs break-all", "{flags.join(\" \")}" }
                        }
                    })
                }
                Field { label: "Patch",
                    if pkg.has_patch {
                        Link {
                            class: "link link-primary",
                            to: Route::PackageSource { pkgbase: pkg.name.clone(), path: vec![] },
                            "applied →"
                        }
                    } else {
                        span { class: "opacity-60", "none" }
                    }
                }
            }
        }
    }
}

/// What lands in the repository when this package builds.
#[component]
fn ProducesCard(pkg: ExtendedPackage) -> Element {
    let names = produced_names(&pkg);
    let split = pkg
        .split_packages
        .as_ref()
        .is_some_and(|names| names.len() > 1);

    rsx! {
        div { class: "card bg-base-100 shadow-xl",
            div { class: "card-body",
                h2 { class: "card-title text-base",
                    "Produces"
                    if split {
                        span { class: "badge badge-sm badge-neutral", "{names.len()}" }
                    }
                }
                ul { class: "divide-y divide-base-300",
                    for name in names.iter() {
                        li { key: "{name}", class: "py-2 flex items-center gap-2",
                            span { class: "font-mono text-sm break-all", "{name}" }
                            div { class: "flex-1" }
                            NotTracked { what: "size" }
                        }
                    }
                }
                // These names come from the package's own declaration, not from
                // the repository. Until the artifacts are exposed, the page
                // cannot say which of them are actually built and present.
                p { class: "text-xs opacity-50 mt-2",
                    "Declared package names. Built artifacts and their sizes are not tracked yet."
                }
            }
        }
    }
}

/// A value the backend does not expose yet.
///
/// Rendered as an explicit marker rather than a blank or a dash: an empty cell
/// reads as "this package has no value", which is a different and wrong claim.
#[component]
fn NotTracked(what: String) -> Element {
    rsx! {
        span {
            class: "badge badge-ghost badge-xs opacity-50",
            title: "Not tracked yet: {what}",
            "—"
        }
    }
}

/// A label/value row inside a sidebar card.
#[component]
fn Field(label: String, children: Element) -> Element {
    rsx! {
        div { class: "flex gap-2 py-1 text-sm",
            span { class: "opacity-60 w-24 shrink-0", "{label}" }
            div { class: "min-w-0", {children} }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(id: i32, status: BuildState, start: Option<i64>, end: Option<i64>) -> Build {
        Build {
            id,
            pkg_id: 1,
            pkg_name: "hello".to_string(),
            version: "1.0-1".to_string(),
            status: status.as_i32(),
            start_time: start,
            end_time: end,
            platform: "x86_64".to_string(),
            waiting_reason: None,
        }
    }

    /// The newest build is whichever started last, not whichever the API
    /// happened to return first.
    #[test]
    fn the_latest_build_is_the_most_recent_one() {
        let builds = vec![
            sample(1, BuildState::Successful, Some(100), Some(150)),
            sample(3, BuildState::Failed, Some(300), Some(310)),
            sample(2, BuildState::Successful, Some(200), Some(260)),
        ];
        assert_eq!(latest(&builds).map(|b| b.id), Some(3));
    }

    /// What is in the repository is the newest *successful* build, which is
    /// exactly what the latest build is not when the latest one failed.
    #[test]
    fn the_repo_build_is_the_newest_successful_one() {
        let builds = vec![
            sample(1, BuildState::Successful, Some(100), Some(150)),
            sample(3, BuildState::Failed, Some(300), Some(310)),
            sample(2, BuildState::Successful, Some(200), Some(260)),
        ];
        assert_eq!(in_repo(&builds).map(|b| b.id), Some(2));
        assert_ne!(
            latest(&builds).map(|b| b.id),
            in_repo(&builds).map(|b| b.id),
            "a failing latest build is the case the second row exists for"
        );
    }

    /// A package whose newest build succeeded needs only one row.
    #[test]
    fn a_healthy_package_has_one_build_worth_showing() {
        let builds = vec![
            sample(1, BuildState::Successful, Some(100), Some(150)),
            sample(2, BuildState::Successful, Some(200), Some(260)),
        ];
        assert_eq!(
            latest(&builds).map(|b| b.id),
            in_repo(&builds).map(|b| b.id)
        );
    }

    /// Nothing has succeeded yet, so nothing is in the repository.
    #[test]
    fn a_package_that_never_succeeded_has_nothing_in_the_repo() {
        let builds = vec![sample(1, BuildState::Failed, Some(100), Some(150))];
        assert!(in_repo(&builds).is_none());
        assert_eq!(latest(&builds).map(|b| b.id), Some(1));
    }

    /// Median, not mean: the 40-minute outlier must not become "typical".
    #[test]
    fn the_typical_duration_resists_an_outlier() {
        let builds = vec![
            sample(1, BuildState::Successful, Some(0), Some(60)),
            sample(2, BuildState::Successful, Some(0), Some(70)),
            sample(3, BuildState::Successful, Some(0), Some(2400)),
        ];
        assert_eq!(typical_duration(&builds), Some(70));
    }

    /// A failure measures how fast something broke, not how long a build takes.
    #[test]
    fn failed_builds_do_not_count_towards_the_typical_duration() {
        let builds = vec![
            sample(1, BuildState::Successful, Some(0), Some(600)),
            sample(2, BuildState::Failed, Some(0), Some(5)),
            sample(3, BuildState::Failed, Some(0), Some(5)),
        ];
        assert_eq!(typical_duration(&builds), Some(600));
    }

    /// An unfinished or never-run build contributes no duration, and a package
    /// with none at all has no typical time rather than a zero one.
    #[test]
    fn a_package_with_no_completed_builds_has_no_typical_duration() {
        assert_eq!(typical_duration(&[]), None);
        let running = vec![sample(1, BuildState::Active, Some(100), None)];
        assert_eq!(typical_duration(&running), None);
    }

    /// Every produced name is shown, and a package that declares none still
    /// produces one named after itself.
    #[test]
    fn produced_names_fall_back_to_the_pkgbase() {
        let mut pkg = package();
        assert_eq!(produced_names(&pkg), vec!["hello".to_string()]);

        pkg.split_packages = Some(vec!["hello".into(), "hello-docs".into()]);
        assert_eq!(
            produced_names(&pkg),
            vec!["hello".to_string(), "hello-docs".to_string()]
        );

        // An empty list is a declaration of nothing, which is not meaningful —
        // treat it as the un-split case rather than rendering an empty card.
        pkg.split_packages = Some(vec![]);
        assert_eq!(produced_names(&pkg), vec!["hello".to_string()]);
    }

    fn package() -> ExtendedPackage {
        ExtendedPackage {
            id: 1,
            name: "hello".to_string(),
            directly_requested: true,
            status: BuildState::Successful.as_i32(),
            outofdate: 0,
            latest_version: Some("1.0-1".to_string()),
            selected_platforms: vec!["x86_64".to_string()],
            selected_build_flags: None,
            upstream_version: Some("1.0-1".to_string()),
            package_source: PackageSource::Git(aurcache_client::GitSourceSpec {
                url: "https://example.com/hello.git".to_string(),
                r#ref: "main".to_string(),
                subfolder: String::new(),
            }),
            split_packages: None,
            dependencies: vec![],
            dependents: vec![],
            has_patch: false,
        }
    }
}
