//! One package: its state, what it depends on, and where it came from.
//!
//! Built around the question someone actually arrives with — "is this package
//! healthy, and if not what do I do" — rather than around the data model. The
//! header answers it; everything below is reference material.
//!
//! Deliberately not a list of builds. That was the old centrepiece, and it
//! buried what matters: for each architecture, the most recent build and — when
//! that one failed — the older build the repository still serves for it. The
//! full history is a click away.

use crate::api::client;
use crate::dates::RelativeDate;
use crate::format::{format_bytes, format_duration, now_secs};
use crate::platforms::PlatformChecklist;
use crate::routes::Route;
use crate::status::{BuildStatusBadge, StatusBadge};
use aurcache_client::{Build, ExtendedPackage, PackageFile, PackageSource, PatchPackageRequest};
use aurcache_common::build_state::BuildState;
use dioxus::prelude::*;

/// How many builds to pull for the summary. Enough for a stable typical
/// duration without fetching a long history the page does not show.
const BUILD_SAMPLE: u64 = 20;

/// Whether this build's output is what the repository serves for its
/// architecture.
fn succeeded(build: &Build) -> bool {
    matches!(
        BuildState::from_i32(build.status),
        Some(BuildState::Successful)
    )
}

/// The architectures this package has builds for, ordered the way the platform
/// picker lists them, with any architecture the package no longer targets (but
/// still has history for) after those.
fn platforms_of(builds: &[Build]) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for build in builds {
        if !seen.iter().any(|p| p == &build.platform) {
            seen.push(build.platform.clone());
        }
    }
    // Stable sort: known architectures fall into picker order, and anything
    // unrecognised keeps the order it was first seen in, after them.
    seen.sort_by_key(|p| {
        crate::platforms::ALL
            .iter()
            .position(|a| a == p)
            .unwrap_or(crate::platforms::ALL.len())
    });
    seen
}

/// The most recent build on one architecture, whatever its outcome.
///
/// The API returns builds newest-first, but that is an ordering this screen
/// depends on for correctness, so it picks the maximum explicitly rather than
/// trusting position.
fn latest_on<'a>(builds: &'a [Build], platform: &str) -> Option<&'a Build> {
    builds
        .iter()
        .filter(|b| b.platform == platform)
        .max_by_key(|b| (b.start_time, b.number))
}

/// The newest successful build on one architecture — the one whose packages the
/// repository serves for it.
fn in_repo_on<'a>(builds: &'a [Build], platform: &str) -> Option<&'a Build> {
    builds
        .iter()
        .filter(|b| b.platform == platform && succeeded(b))
        .max_by_key(|b| (b.end_time, b.number))
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
        .filter(|b| succeeded(b))
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

    // Issued together rather than one after the other. They share no data, and
    // the package request is the slow one — it makes a live AUR lookup
    // server-side — so running them in sequence added the build query's latency
    // on top of it for no reason. The browser is the executor; this is
    // concurrency, not threads.
    let (package, builds) = futures_util::future::join(
        client.get_package(&pkgbase),
        client.list_builds(Some(&pkgbase), Some(BUILD_SAMPLE), None),
    )
    .await;

    // A failed build query should not lose the package itself: the build
    // summary is secondary, and the rest of the page is still worth showing.
    Ok((
        package.map_err(|e| e.to_string())?,
        builds.unwrap_or_default(),
    ))
}

#[component]
pub fn Package(pkgbase: String) -> Element {
    // `use_reactive` so the fetch follows the route. Navigating between two
    // packages reuses this component -- same route, different parameter -- and
    // a resource whose closure captured the old name simply never re-runs: the
    // URL changes, no request is made, and the previous package stays on
    // screen looking like the one that was clicked.
    let mut data = use_resource(use_reactive(&pkgbase, load));

    // Refresh while this package or one of its recent builds is still in
    // flight, so a build finishing updates the status and the build summary
    // without a reload; a slow tick otherwise as a catch-all.
    let busy = matches!(&*data.read_unchecked(), Some(Ok((pkg, builds)))
        if BuildState::from_i32(pkg.status).is_some_and(BuildState::is_in_progress)
            || builds.iter().any(|b| BuildState::from_i32(b.status).is_some_and(BuildState::is_in_progress)));
    crate::poll::use_poll(data, busy);

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
                    PackageHeader { pkg: pkg.clone(), trail: vec![] }
                    // The sidebar starts level with the builds card rather than
                    // below it, so the builds card is only as wide as it needs
                    // and the space beside it is used.
                    //
                    // A fixed 26rem sidebar rather than a fraction: its content
                    // is URLs and versions, which wrap badly, and a fraction of
                    // a wide viewport is more than they need while a fraction
                    // of a narrow one is less.
                    div { class: "grid grid-cols-1 lg:grid-cols-[minmax(0,1fr)_26rem] gap-4 items-start",
                        // Dependency lists are unbounded — thirty entries is
                        // ordinary — so they take the flexible column.
                        div { class: "space-y-4 min-w-0",
                            BuildSummary {
                                pkgbase: pkg.name.clone(),
                                builds: builds.clone(),
                                on_changed: move |()| data.restart(),
                            }
                            Relations { pkg: pkg.clone() }
                        }
                        div { class: "space-y-4 min-w-0",
                            SourceCard { pkg: pkg.clone() }
                            BuildConfigCard {
                                pkg: pkg.clone(),
                                on_changed: move |()| data.restart(),
                            }
                            ProducesCard { pkg: pkg.clone() }
                        }
                    }
                    // Below the fold of the page proper: it is the one
                    // irreversible action here, and it has no business sitting
                    // beside Rebuild where people click without reading.
                    RemoveCard { pkgbase: pkg.name.clone() }
                }
            },
        }
    }
}

#[component]
pub fn PackageHeader(pkg: ExtendedPackage, trail: Vec<(String, Option<Route>)>) -> Element {
    let description = pkg.description.clone();

    rsx! {
        div { class: "card bg-base-100 shadow-xl",
            div { class: "card-body",
                div { class: "flex flex-wrap items-start gap-3",
                    div { class: "min-w-0",
                        // The trail *is* the heading, rather than a small copy
                        // of it above: the package name appeared twice
                        // otherwise. Ancestors are muted so the page you are on
                        // still reads as the title.
                        //
                        // Laid out as ordinary inline text, not as a flex
                        // row: a baseline-aligned flex row takes its baseline
                        // from whichever item has the taller ascent, and the
                        // package name is monospace while the crumbs are not.
                        //
                        // `leading-8` is what actually lands "Packages" on the
                        // same pixel as the identical word on the package list.
                        // There the heading's 28px line box is centred in this
                        // 32px row, so it starts 2px down; here the line box is
                        // the full 32px and starts at 0, and the extra
                        // half-leading puts the baseline in the same place. Let
                        // the box size itself instead and it comes out 29px
                        // tall — the monospace name widens it — which centres
                        // to a half pixel and rounds the whole heading up by
                        // one.
                        div { class: "flex items-center min-h-8",
                            h1 { class: "card-title block leading-8 break-all",
                                Link {
                                    class: "opacity-60 link-hover",
                                    to: Route::Packages { q: String::new() },
                                    "Packages"
                                }
                                span { class: "opacity-30 mx-2", "/" }
                                if trail.is_empty() {
                                    span { class: "font-mono", "{pkg.name}" }
                                } else {
                                    Link {
                                        class: "font-mono opacity-60 link-hover",
                                        to: Route::Package { pkgbase: pkg.name.clone() },
                                        "{pkg.name}"
                                    }
                                    for (index, (label, route)) in trail.iter().enumerate() {
                                        span { key: "sep-{index}", class: "opacity-30 mx-2", "/" }
                                        match route.clone() {
                                            Some(route) => rsx! {
                                                Link {
                                                    key: "{index}",
                                                    class: "opacity-60 link-hover",
                                                    to: route,
                                                    "{label}"
                                                }
                                            },
                                            None => rsx! { span { key: "{index}", "{label}" } },
                                        }
                                    }
                                }
                            }
                        }
                        div { class: "flex items-center gap-3 flex-wrap mt-1",
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
                        VersionLine { pkg }
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
    let upstream = pkg.upstream_version;

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

/// The most recent build on each architecture, and how long a build usually
/// takes.
///
/// One row per architecture rather than a single "latest": a package builds for
/// each architecture on its own — a PKGBUILD can compile cleanly for x86_64 and
/// fail to cross-compile for armv7h — and a single latest build shows whichever
/// architecture ran last while saying nothing about the rest. When an
/// architecture's most recent build failed, a second line names the older build
/// its repository still serves.
#[component]
fn BuildSummary(pkgbase: String, builds: Vec<Build>, on_changed: EventHandler<()>) -> Element {
    let now = now_secs();
    let typical = typical_duration(&builds);

    // (label, build) in display order: each architecture's most recent build,
    // and after a failed one the build the repository still serves for it.
    let mut rows: Vec<(String, Build)> = Vec::new();
    for platform in platforms_of(&builds) {
        let Some(newest) = latest_on(&builds, &platform) else {
            continue;
        };
        rows.push((platform.clone(), newest.clone()));
        // A second line only when that most recent build did not succeed:
        // otherwise it *is* what the repository serves, so `in_repo_on` would
        // return the same build and the row would just be noise.
        if !succeeded(newest)
            && let Some(repo) = in_repo_on(&builds, &platform)
        {
            rows.push(("↳ in repo".to_string(), repo.clone()));
        }
    }

    rsx! {
        div { class: "card bg-base-100 shadow-xl",
            div { class: "card-body py-4",
                div { class: "flex items-center gap-3 flex-wrap",
                    Link {
                        class: "card-title text-base link-hover",
                        to: Route::PackageBuilds { pkgbase: pkgbase.clone() },
                        "Builds"
                    }
                    if let Some(typical) = typical {
                        span { class: "text-sm opacity-60",
                            "typically {format_duration(Some(0), Some(typical))}"
                        }
                    }
                    div { class: "flex-1" }
                    // Rebuilding produces a build, so the button belongs with
                    // the builds rather than in the page header.
                    RebuildButton { pkgbase, on_changed }
                }

                if rows.is_empty() {
                    p { class: "opacity-60 text-sm", "This package has never been built." }
                } else {
                    div { class: "divide-y divide-base-300",
                        for (index, (label, entry)) in rows.iter().enumerate() {
                            BuildRow {
                                key: "{index}-{entry.number}",
                                label: label.clone(),
                                entry: entry.clone(),
                                now,
                            }
                        }
                    }
                }
            }
        }
    }
}

/// One line of the build summary: an architecture and its most recent build, or
/// "↳ in repo" and the build still served for it after that one failed.
///
/// "in repo" rather than "latest successful": the label says what it means for
/// the reader — this is the version pacman will install right now.
///
/// The prop is `entry`, not `build`: Dioxus generates a props builder whose own
/// `build()` finalises it, so a prop of that name is ambiguous at the call site.
#[component]
fn BuildRow(label: String, entry: Build, now: i64) -> Element {
    rsx! {
        Link {
            class: "flex items-center gap-3 py-2 hover:bg-base-200 px-2 -mx-2 rounded flex-wrap",
            to: Route::Build { pkgbase: entry.pkg_name.clone(), number: entry.number },
            span { class: "text-sm opacity-60 w-20 shrink-0", "{label}" }
            span { class: "font-mono text-sm", "{entry.number}" }
            BuildStatusBadge { status: entry.status }
            span { class: "font-mono text-sm opacity-70", "{entry.version}" }
            div { class: "flex-1" }
            span { class: "text-sm opacity-60", RelativeDate { ts: entry.start_time, now } }
            span { class: "font-mono text-sm opacity-60 w-16 text-right",
                {format_duration(entry.start_time, entry.end_time)}
            }
        }
    }
}

#[component]
fn Relations(pkg: ExtendedPackage) -> Element {
    let ExtendedPackage {
        dependencies,
        dependents,
        ..
    } = pkg;

    rsx! {
        RelationList {
            title: "Dependencies",
            empty: "Nothing — this package builds on its own.",
            items: dependencies,
            // Only dependencies gate this package's build. A dependent that is
            // unsatisfied is waiting on *this* package, which is its problem to
            // display, not a reason to flag anything here.
            show_blocking: true,
        }
        RelationList {
            title: "Dependents",
            empty: "Nothing depends on this package.",
            items: dependents,
            show_blocking: false,
        }
    }
}

#[component]
fn RelationList(
    title: String,
    empty: String,
    items: Vec<aurcache_client::PackageDependency>,
    show_blocking: bool,
) -> Element {
    let blocking = items.iter().filter(|item| !item.satisfied).count();

    rsx! {
        div { class: "card bg-base-100 shadow-xl",
            div { class: "card-body",
                h2 { class: "card-title text-base",
                    "{title}"
                    if !items.is_empty() {
                        span { class: "badge badge-sm badge-neutral", "{items.len()}" }
                    }
                    if show_blocking && blocking > 0 {
                        span { class: "badge badge-sm badge-warning",
                            "{blocking} blocking"
                        }
                    }
                }
                if items.is_empty() {
                    p { class: "opacity-60 text-sm", "{empty}" }
                } else {
                    ul { class: "divide-y divide-base-300",
                        for item in items.iter() {
                            RelationRow { item: item.clone(), show_blocking }
                        }
                    }
                }
            }
        }
    }
}

/// One dependency, and why it is or is not holding the build back.
#[component]
fn RelationRow(item: aurcache_client::PackageDependency, show_blocking: bool) -> Element {
    let blocking = show_blocking && !item.satisfied;

    rsx! {
        li { key: "{item.id}", class: "py-2 flex items-center gap-2 flex-wrap",
            Link {
                class: "link link-primary font-mono text-sm break-all",
                to: Route::Package { pkgbase: item.name.clone() },
                "{item.name}"
            }
            if !item.version_constraint.is_empty() {
                span { class: "font-mono text-xs opacity-60", "{item.version_constraint}" }
            }
            div { class: "flex-1" }
            if blocking {
                // Say what is actually wrong. "failed" alone does not
                // distinguish a dependency that never built from one that built
                // to a version too old to satisfy the constraint — and the
                // second looks healthy everywhere else.
                match item.built_version.clone() {
                    Some(built) => rsx! {
                        span { class: "font-mono text-xs opacity-70", "has {built}" }
                        span { class: "badge badge-warning badge-sm", "too old" }
                    },
                    None => rsx! {
                        span { class: "badge badge-warning badge-sm", "never built" }
                    },
                }
            } else if let Some(built) = item.built_version.clone() {
                span { class: "font-mono text-xs opacity-50", "{built}" }
            }
            BuildStatusBadge { status: item.status }
        }
    }
}

#[component]
fn SourceCard(pkg: ExtendedPackage) -> Element {
    rsx! {
        div { class: "card bg-base-100 shadow-xl",
            div { class: "card-body",
                h2 { class: "card-title text-base", "Source" }
                // Where the source comes from. The descriptive fields below
                // are read from the checkout, so they are the same for an AUR
                // package and a git one.
                match &pkg.package_source {
                    PackageSource::Aur(aur) => rsx! {
                        Field { label: "Origin",
                            a { class: "link link-primary", href: "{aur.aur_url}",
                                target: "_blank", rel: "noopener noreferrer", "AUR ↗" }
                        }
                    },
                    PackageSource::AurNotFound(_) => rsx! {
                        div { class: "alert alert-warning text-sm mb-2",
                            span { "No longer found on the AUR." }
                        }
                    },
                    PackageSource::Git(spec) => rsx! {
                        Field { label: "Origin", span { "Git" } }
                        Field { label: "URL",
                            match browsable_url(&spec.url) {
                                Some(href) => rsx! {
                                    a {
                                        class: "link link-primary font-mono text-xs break-all",
                                        href: "{href}",
                                        target: "_blank",
                                        rel: "noopener noreferrer",
                                        "{spec.url}"
                                    }
                                },
                                // An SSH remote is not a page a browser can
                                // open, so it stays text rather than becoming
                                // a link that goes nowhere.
                                None => rsx! {
                                    span { class: "font-mono text-xs break-all", "{spec.url}" }
                                },
                            }
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

                if let Some(maintainer) = pkg.maintainer.clone() {
                    Field { label: "Maintainer", span { "{maintainer}" } }
                }
                if let Some(licenses) = pkg.licenses.clone() {
                    Field { label: "Licenses", span { "{licenses}" } }
                }
                if let Some(url) = pkg.project_url.clone() {
                    Field { label: "Upstream",
                        a { class: "link link-primary break-all", href: "{url}",
                            target: "_blank", rel: "noopener noreferrer", "{url}" }
                    }
                }
                if matches!(&pkg.package_source, PackageSource::Aur(aur) if aur.aur_flagged_outdated) {
                    div { class: "alert alert-warning text-sm mt-2",
                        span { "Flagged out of date on the AUR." }
                    }
                }
            }
        }
    }
}

#[component]
fn BuildConfigCard(pkg: ExtendedPackage, on_changed: EventHandler<()>) -> Element {
    rsx! {
        div { class: "card bg-base-100 shadow-xl",
            div { class: "card-body",
                h2 { class: "card-title text-base", "Build config" }
                PlatformField {
                    pkgbase: pkg.name.clone(),
                    selected: pkg.selected_platforms.clone(),
                    on_changed,
                }
                BuildFlagsField {
                    pkgbase: pkg.name.clone(),
                    // A package with no flags stores an empty string, which the
                    // API splits on `;` into one empty flag rather than none.
                    // Left in, it renders as a chip with no label.
                    flags: pkg
                        .selected_build_flags
                        .clone()
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|flag| !flag.trim().is_empty())
                        .collect(),
                    on_changed,
                }
                Field { label: "Patch",
                    if pkg.has_patch {
                        span { class: "badge badge-warning badge-sm", "applied" }
                    } else {
                        span { class: "opacity-60", "none" }
                    }
                }
                // Editing the sources is what creates or clears that patch, so
                // it sits with it rather than in the page header.
                Link {
                    class: "btn btn-sm btn-block mt-2",
                    to: Route::PackageSource { pkgbase: pkg.name.clone(), path: vec![] },
                    "Edit sources"
                }
                // The per-package makepkg.conf/pacman.conf overrides. They
                // need a page of their own — two full-height editors — and
                // without a way through it is reachable only by typing its URL.
                Link {
                    class: "btn btn-sm btn-block mt-2",
                    to: Route::PackageConfigFiles { pkgbase: pkg.name },
                    "Config files"
                }
            }
        }
    }
}

/// A download count, or nothing at all.
///
/// Zero is rendered as "not downloaded yet" rather than "0": for a package
/// built minutes ago the number is uninformative, and a bare 0 next to a
/// healthy build reads as something being broken.
fn downloads_label(downloads: i64) -> String {
    match downloads {
        ..=0 => "not downloaded yet".to_string(),
        1 => "1 download".to_string(),
        n => format!("{n} downloads"),
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

    // Once the package has built, the repository rows are the truth: they say
    // which of the declared names actually produced an artifact, on which
    // platform, and how big it is. Before the first build there are no rows, so
    // the declared names are all the page can show.
    let built = !pkg.files.is_empty();
    let total = total_size(&pkg.files);

    rsx! {
        div { class: "card bg-base-100 shadow-xl",
            div { class: "card-body",
                h2 { class: "card-title text-base",
                    "Produces"
                    if built {
                        span { class: "badge badge-sm badge-neutral", "{pkg.files.len()}" }
                    } else if split {
                        span { class: "badge badge-sm badge-neutral", "{names.len()}" }
                    }
                }
                ul { class: "divide-y divide-base-300",
                    if built {
                        for file in pkg.files.iter() {
                            // Arch and size first, at their natural width; the
                            // filename takes the rest and scrolls sideways
                            // rather than wrapping. In a 26rem column a
                            // `name-version-arch.pkg.tar.zst` is wider than the
                            // row, and wrapping it broke the small fields onto
                            // their own lines too ("30 MiB" split in two).
                            li { key: "{file.filename}", class: "py-2 flex items-center gap-2",
                                span { class: "badge badge-ghost badge-xs shrink-0", "{file.platform}" }
                                FileSize { file: file.clone() }
                                span {
                                    class: "font-mono text-xs flex-1 min-w-0 overflow-x-auto \
                                            whitespace-nowrap",
                                    title: "{file.filename}",
                                    "{file.filename}"
                                }
                            }
                        }
                    } else {
                        // Declared names, not artifacts: these are pkgbase /
                        // split-package names, short enough to sit on one line.
                        for name in names.iter() {
                            li { key: "{name}", class: "py-2 flex items-center gap-2",
                                NotTracked { what: "size" }
                                span { class: "font-mono text-sm break-all min-w-0", "{name}" }
                            }
                        }
                    }
                }
                if let Some(total) = total {
                    div { class: "flex items-baseline gap-2 pt-2 text-sm",
                        span { class: "opacity-60", "Total" }
                        span { class: "font-mono", {format_bytes(total)} }
                    }
                }
                // Counted across every version and architecture this package
                // has produced -- the question is how often it has been
                // installed, not how often one particular build was fetched.
                div { class: "flex items-baseline gap-2 pt-2",
                    span { class: "text-sm", {downloads_label(pkg.downloads)} }
                    span {
                        class: "cursor-help text-xs opacity-40 hover:opacity-80 transition-opacity",
                        title: "Fetches of this package from the repository, across every version \
                                and architecture. Up to date on every load — counts still buffered \
                                in the server are included. Resumed (partial) downloads are not \
                                counted.",
                        "?"
                    }
                }
                if !built {
                    // Nothing is in the repository yet, so these are what the
                    // PKGBUILD declares rather than what exists.
                    p { class: "text-xs opacity-50 mt-2",
                        "Declared package names. Nothing has been built into the repository yet."
                    }
                }
            }
        }
    }
}

/// The same repository as a page a browser can open, if it is one.
///
/// Git remotes are not all web addresses. `git+ssh://git@github.com/...` is a
/// supported and documented source here, and so is `git@host:path`; neither is
/// something a browser can follow. Only http(s) remotes become links, so the
/// rest render as plain text instead of a link that goes nowhere.
///
/// The `git+` prefix is makepkg's way of marking a source as a git repository,
/// not part of the address.
fn browsable_url(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let url = raw.strip_prefix("git+").unwrap_or(raw);
    (url.starts_with("https://") || url.starts_with("http://")).then(|| url.to_string())
}

/// Queue a rebuild of this package.
///
/// Disabled while in flight so a double click does not queue twice, and it
/// reports what happened rather than silently doing nothing — a rebuild is
/// otherwise invisible until the build list refreshes.
#[component]
fn RebuildButton(pkgbase: String, on_changed: EventHandler<()>) -> Element {
    let mut busy = use_signal(|| false);
    let mut error = use_signal(|| Option::<String>::None);

    rsx! {
        div { class: "flex items-center gap-2",
            if let Some(message) = error() {
                span { class: "text-xs text-error", "{message}" }
            }
            button {
                class: "btn btn-primary btn-sm",
                disabled: busy(),
                onclick: move |_| {
                    let pkgbase = pkgbase.clone();
                    async move {
                        busy.set(true);
                        error.set(None);
                        let outcome = match client() {
                            Ok(client) => client
                                .update_package(&pkgbase, &aurcache_client::UpdatePackageRequest {
                                    force: true,
                                })
                                .await
                                .map_err(|e| e.to_string()),
                            Err(e) => Err(e),
                        };
                        if let Err(e) = outcome {
                            error.set(Some(e));
                        } else {
                            on_changed.call(());
                        }
                        busy.set(false);
                    }
                },
                if busy() {
                    span { class: "loading loading-spinner loading-xs" }
                }
                "Rebuild"
            }
        }
    }
}

/// The platforms a package is built for, with an inline editor.
///
/// Changing this changes which dependencies are required — a PKGBUILD can
/// declare `depends_aarch64` separately — so the server re-resolves the
/// dependency graph on save. That is why the page reloads afterwards rather
/// than patching the field in place.
#[component]
pub fn PlatformField(
    pkgbase: String,
    selected: Vec<String>,
    on_changed: EventHandler<()>,
) -> Element {
    let mut editing = use_signal(|| false);
    let mut draft = use_signal(|| selected.clone());
    let mut busy = use_signal(|| false);
    let mut error = use_signal(|| Option::<String>::None);

    // Reset the draft each time the editor opens, so a cancelled edit does not
    // linger into the next one.
    let start = {
        let selected = selected.clone();
        move |_| {
            draft.set(selected.clone());
            error.set(None);
            editing.set(true);
        }
    };

    rsx! {
        div { class: "flex gap-2 py-1 text-sm",
            span { class: "opacity-60 w-24 shrink-0", "Platforms" }
            div { class: "min-w-0 flex-1",
                if editing() {
                    div { class: "flex flex-col gap-1",
                        PlatformChecklist {
                            selected: draft(),
                            onchange: move |next| draft.set(next),
                        }
                        if let Some(message) = error() {
                            span { class: "text-xs text-error", "{message}" }
                        }
                        div { class: "flex gap-2 pt-1",
                            button {
                                class: "btn btn-primary btn-xs",
                                // A package built for nothing would never build
                                // again, so saving an empty set is refused
                                // rather than accepted and puzzled over later.
                                disabled: busy() || draft().is_empty(),
                                onclick: {
                                    move |_| {
                                        let pkgbase = pkgbase.clone();
                                        async move {
                                            busy.set(true);
                                            error.set(None);
                                            let outcome = match client() {
                                                Ok(client) => client
                                                    .patch_package(&pkgbase, &aurcache_client::PatchPackageRequest {
                                                        platforms: Some(draft()),
                                                        ..Default::default()
                                                    })
                                                    .await
                                                    .map_err(|e| e.to_string()),
                                                Err(e) => Err(e),
                                            };
                                            match outcome {
                                                Ok(()) => {
                                                    editing.set(false);
                                                    on_changed.call(());
                                                }
                                                Err(e) => error.set(Some(e)),
                                            }
                                            busy.set(false);
                                        }
                                    }
                                },
                                "Save"
                            }
                            button {
                                class: "btn btn-ghost btn-xs",
                                disabled: busy(),
                                onclick: move |_| editing.set(false),
                                "Cancel"
                            }
                        }
                    }
                } else {
                    div { class: "flex items-center gap-2 flex-wrap",
                        span { class: "font-mono text-xs", {selected.join(", ")} }
                        button {
                            class: "btn btn-ghost btn-xs",
                            onclick: start,
                            "Change"
                        }
                    }
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

/// The combined size of every artifact, or `None` if any one of them is unknown.
///
/// All-or-nothing on purpose: summing only the known sizes would print a total
/// smaller than the parts it is made of, which reads as a bug rather than as
/// missing data. `Option`'s `Sum` gives exactly this — one `None` and the whole
/// total is `None`.
fn total_size(files: &[PackageFile]) -> Option<u64> {
    if files.is_empty() {
        return None;
    }
    files
        .iter()
        .map(|f| f.size.and_then(|s| u64::try_from(s).ok()))
        .sum()
}

/// One artifact's size, or the placeholder when it is not known.
///
/// Unknown means the row predates the size column and the file was already gone
/// when the startup backfill looked, so there is nothing to report -- rendering
/// it as `0 B` would claim the package file is empty.
#[component]
fn FileSize(file: PackageFile) -> Element {
    match file.size.and_then(|s| u64::try_from(s).ok()) {
        Some(bytes) => rsx! {
            // `shrink-0` + `whitespace-nowrap` so "30 MiB" keeps its one line
            // when the row is tight — it is the filename beside it that gives.
            span { class: "font-mono text-sm opacity-70 shrink-0 whitespace-nowrap",
                {format_bytes(bytes)}
            }
        },
        None => rsx! {
            NotTracked { what: "size" }
        },
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

    fn sample(
        number: i32,
        platform: &str,
        status: BuildState,
        start: Option<i64>,
        end: Option<i64>,
    ) -> Build {
        Build {
            number,
            pkg_name: "hello".to_string(),
            version: "1.0-1".to_string(),
            status: status.as_i32(),
            start_time: start,
            end_time: end,
            platform: platform.to_string(),
            size: None,
            waiting_reason: None,
        }
    }

    /// Each architecture's most recent build is found on its own: the newest
    /// build overall does not stand in for an architecture that has none.
    #[test]
    fn the_latest_build_is_per_architecture() {
        let builds = vec![
            sample(1, "x86_64", BuildState::Successful, Some(100), Some(150)),
            sample(2, "aarch64", BuildState::Failed, Some(200), Some(210)),
            sample(3, "x86_64", BuildState::Failed, Some(300), Some(310)),
        ];
        assert_eq!(latest_on(&builds, "x86_64").map(|b| b.number), Some(3));
        assert_eq!(latest_on(&builds, "aarch64").map(|b| b.number), Some(2));
        assert_eq!(latest_on(&builds, "armv7h"), None);
    }

    /// The repository serves the newest *successful* build for an architecture,
    /// which is exactly what its latest build is not once that one has failed.
    #[test]
    fn the_repo_build_is_the_newest_successful_one_for_its_architecture() {
        let builds = vec![
            sample(1, "x86_64", BuildState::Successful, Some(100), Some(150)),
            sample(2, "x86_64", BuildState::Successful, Some(200), Some(260)),
            sample(3, "x86_64", BuildState::Failed, Some(300), Some(310)),
        ];
        assert_eq!(in_repo_on(&builds, "x86_64").map(|b| b.number), Some(2));
        assert_ne!(
            latest_on(&builds, "x86_64").map(|b| b.number),
            in_repo_on(&builds, "x86_64").map(|b| b.number),
            "a failing latest build is the case the second row exists for"
        );
    }

    /// One architecture failing says nothing about what the repository serves
    /// for another.
    #[test]
    fn architectures_do_not_share_repo_state() {
        let builds = vec![
            sample(1, "x86_64", BuildState::Successful, Some(100), Some(150)),
            sample(2, "aarch64", BuildState::Failed, Some(200), Some(210)),
        ];
        assert_eq!(in_repo_on(&builds, "x86_64").map(|b| b.number), Some(1));
        assert_eq!(in_repo_on(&builds, "aarch64"), None);
    }

    /// Architectures are listed the way the platform picker lists them, whatever
    /// order builds arrive in; one the package no longer targets still shows if
    /// it has history, after the recognised ones.
    #[test]
    fn platforms_are_listed_in_picker_order() {
        let builds = vec![
            sample(1, "armv7h", BuildState::Successful, Some(100), Some(150)),
            sample(2, "x86_64", BuildState::Successful, Some(200), Some(260)),
            sample(3, "riscv64", BuildState::Failed, Some(300), Some(310)),
            sample(4, "aarch64", BuildState::Successful, Some(400), Some(460)),
            sample(5, "x86_64", BuildState::Failed, Some(500), Some(510)),
        ];
        assert_eq!(
            platforms_of(&builds),
            vec!["x86_64", "aarch64", "armv7h", "riscv64"]
        );
    }

    /// A package with no builds has no architectures to show.
    #[test]
    fn a_package_that_never_built_has_no_architectures() {
        assert!(platforms_of(&[]).is_empty());
    }

    /// Median, not mean: the 40-minute outlier must not become "typical".
    #[test]
    fn the_typical_duration_resists_an_outlier() {
        let builds = vec![
            sample(1, "x86_64", BuildState::Successful, Some(0), Some(60)),
            sample(2, "x86_64", BuildState::Successful, Some(0), Some(70)),
            sample(3, "x86_64", BuildState::Successful, Some(0), Some(2400)),
        ];
        assert_eq!(typical_duration(&builds), Some(70));
    }

    /// A failure measures how fast something broke, not how long a build takes.
    #[test]
    fn failed_builds_do_not_count_towards_the_typical_duration() {
        let builds = vec![
            sample(1, "x86_64", BuildState::Successful, Some(0), Some(600)),
            sample(2, "x86_64", BuildState::Failed, Some(0), Some(5)),
            sample(3, "x86_64", BuildState::Failed, Some(0), Some(5)),
        ];
        assert_eq!(typical_duration(&builds), Some(600));
    }

    /// An unfinished or never-run build contributes no duration, and a package
    /// with none at all has no typical time rather than a zero one.
    #[test]
    fn a_package_with_no_completed_builds_has_no_typical_duration() {
        assert_eq!(typical_duration(&[]), None);
        let running = vec![sample(1, "x86_64", BuildState::Active, Some(100), None)];
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

    fn file(name: &str, size: Option<i64>) -> PackageFile {
        PackageFile {
            filename: name.to_string(),
            platform: "x86_64".to_string(),
            size,
        }
    }

    #[test]
    fn total_size_adds_every_artifact() {
        let files = vec![
            file("a.pkg.tar.zst", Some(1000)),
            file("b.pkg.tar.zst", Some(24)),
        ];
        assert_eq!(total_size(&files), Some(1024));
    }

    /// One unknown size makes the whole total unknown: a partial sum would
    /// render a total visibly smaller than the rows above it.
    #[test]
    fn one_unknown_size_makes_the_total_unknown() {
        let files = vec![
            file("a.pkg.tar.zst", Some(1000)),
            file("b.pkg.tar.zst", None),
        ];
        assert_eq!(total_size(&files), None);
    }

    /// A package that has never built has no artifacts, and no total to show —
    /// not a total of zero.
    #[test]
    fn a_package_with_no_artifacts_has_no_total() {
        assert_eq!(total_size(&[]), None);
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
            files: vec![],
            downloads: 0,
            dependencies: vec![],
            dependents: vec![],
            has_patch: false,
            description: None,
            project_url: None,
            licenses: None,
            maintainer: None,
            first_submitted: None,
            last_modified: None,
        }
    }
}

#[cfg(test)]
mod url_tests {
    use super::browsable_url;

    #[test]
    fn web_remotes_become_links() {
        assert_eq!(
            browsable_url("https://github.com/user/repo.git").as_deref(),
            Some("https://github.com/user/repo.git")
        );
        // `git+` marks the source as a repository; it is not part of the
        // address, and a browser would choke on it.
        assert_eq!(
            browsable_url("git+https://github.com/user/repo").as_deref(),
            Some("https://github.com/user/repo")
        );
        assert_eq!(
            browsable_url("http://example.com/r.git").as_deref(),
            Some("http://example.com/r.git")
        );
    }

    /// SSH remotes are supported sources here — the docs use
    /// `git+ssh://git@github.com/EpicGames/UnrealEngine` — but they are not
    /// pages. A link to one is worse than no link.
    #[test]
    fn ssh_remotes_do_not_become_links() {
        for raw in [
            "git+ssh://git@github.com/EpicGames/UnrealEngine",
            "ssh://git@example.com/repo.git",
            "git@github.com:user/repo.git",
            "file:///srv/local.git",
        ] {
            assert_eq!(browsable_url(raw), None, "{raw}");
        }
    }

    /// A scheme that merely contains "http" is not an http URL.
    #[test]
    fn only_a_real_http_scheme_counts() {
        assert_eq!(browsable_url("nothttps://example.com"), None);
        assert_eq!(browsable_url(""), None);
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
fn RemoveCard(pkgbase: String) -> Element {
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
