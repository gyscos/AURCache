//! Sorting and filtering for the package and build lists.
//!
//! Done in the browser over the whole fetched list rather than as query
//! parameters. Both lists are fetched entire, so filtering, sorting and paging
//! stay instant, and each of the three sees the results of the other two —
//! server-side paging would mean a filter that only searched the page you were
//! already on. If the fetch itself ever becomes the problem, the pure functions
//! here are what would move.

use aurcache_client::{Build, SimplePackage};
use aurcache_common::build_state::BuildState;

/// Which column a list is ordered by.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SortKey {
    Name,
    Status,
    Time,
    Size,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SortDir {
    Asc,
    Desc,
}

impl SortDir {
    #[must_use]
    pub fn flipped(self) -> Self {
        match self {
            Self::Asc => Self::Desc,
            Self::Desc => Self::Asc,
        }
    }

    /// The arrow shown on the active column.
    pub fn arrow(self) -> &'static str {
        match self {
            Self::Asc => "▲",
            Self::Desc => "▼",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Sort {
    pub key: SortKey,
    pub dir: SortDir,
}

impl Sort {
    /// Clicking a column sorts by it; clicking the active one reverses.
    ///
    /// A new column starts descending for time — the newest build is what you
    /// want first — and ascending for everything else, where A-Z is the
    /// natural reading.
    #[must_use]
    pub fn toggled(self, key: SortKey) -> Self {
        if self.key == key {
            Self {
                key,
                dir: self.dir.flipped(),
            }
        } else {
            Self {
                key,
                dir: match key {
                    // Newest build and largest package first: for these two the
                    // interesting end is the top, where A-Z is the natural
                    // reading for everything else.
                    SortKey::Time | SortKey::Size => SortDir::Desc,
                    _ => SortDir::Asc,
                },
            }
        }
    }
}

/// How a status filter is expressed: everything, or one particular state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StatusFilter(pub Option<BuildState>);

impl StatusFilter {
    pub const ANY: Self = Self(None);

    pub fn id(self) -> String {
        self.0
            .map_or_else(|| "any".to_string(), |s| s.as_i32().to_string())
    }

    pub fn from_id(id: &str) -> Self {
        Self(id.parse::<i32>().ok().and_then(BuildState::from_i32))
    }

    fn matches(self, status: i32) -> bool {
        self.0
            .is_none_or(|wanted| BuildState::from_i32(status) == Some(wanted))
    }
}

/// Case-insensitive substring match.
///
/// Substring rather than prefix because package names are compound —
/// searching `gtk` should find `lib32-gtk3`, which a prefix match would miss.
fn name_matches(name: &str, query: &str) -> bool {
    let query = query.trim();
    query.is_empty() || name.to_lowercase().contains(&query.to_lowercase())
}

/// Order two statuses so the ones needing attention come first.
///
/// Not the numeric order of the enum, which is an implementation detail:
/// sorting by status is asking "what needs me", so failures lead and
/// up-to-date packages trail.
fn status_rank(status: i32) -> u8 {
    match BuildState::from_i32(status) {
        Some(BuildState::Failed) => 0,
        Some(BuildState::Active) => 1,
        Some(BuildState::WaitingForDeps) => 2,
        Some(BuildState::Enqueued) => 3,
        Some(BuildState::Successful) => 4,
        None => 5,
    }
}

pub fn filter_packages(
    packages: &[SimplePackage],
    query: &str,
    status: StatusFilter,
) -> Vec<SimplePackage> {
    packages
        .iter()
        .filter(|pkg| name_matches(&pkg.name, query) && status.matches(pkg.status))
        .cloned()
        .collect()
}

pub fn sort_packages(packages: &mut [SimplePackage], sort: Sort) {
    packages.sort_by(|a, b| {
        let ordering = match sort.key {
            SortKey::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            // An out-of-date package is a package needing attention, so it
            // ranks with the unhealthy ones rather than with the successes.
            SortKey::Status => (status_rank(a.status), a.outofdate == 0)
                .cmp(&(status_rank(b.status), b.outofdate == 0)),
            // A package row carries no timestamp, so the package list does
            // not offer this column; falling back to name would present an
            // ordering that has nothing to do with time.
            SortKey::Time => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            // `Option`'s own ordering is what this wants: `None` sorts below
            // every `Some`, so unrecorded sizes group at one end rather than
            // among the small ones, and land last under the descending order a
            // Size column opens in.
            SortKey::Size => a.total_size.cmp(&b.total_size),
        };
        match sort.dir {
            SortDir::Asc => ordering,
            SortDir::Desc => ordering.reverse(),
        }
    });
}

pub fn filter_builds(builds: &[Build], query: &str, status: StatusFilter) -> Vec<Build> {
    builds
        .iter()
        .filter(|build| name_matches(&build_id(build), query) && status.matches(build.status))
        .cloned()
        .collect()
}

/// A build's identity, as the list shows it and as people refer to it.
///
/// Filtering matches this rather than the package name alone, so both halves of
/// what is on screen are searchable: `hello` finds every build of it, because
/// the match is a substring one, and `hello/3` finds the one. Matching only the
/// package meant typing what the row said found nothing.
fn build_id(build: &Build) -> String {
    format!("{}/{}", build.pkg_name, build.number)
}

pub fn sort_builds(builds: &mut [Build], sort: Sort) {
    builds.sort_by(|a, b| {
        let ordering = match sort.key {
            SortKey::Name => a
                .pkg_name
                .to_lowercase()
                .cmp(&b.pkg_name.to_lowercase())
                // A package's own builds are then newest-first, so the groups
                // read as histories rather than as an arbitrary jumble.
                .then(b.number.cmp(&a.number)),
            SortKey::Status => status_rank(a.status).cmp(&status_rank(b.status)),
            SortKey::Time => a.start_time.cmp(&b.start_time),
            SortKey::Size => a.size.cmp(&b.size),
        };
        match sort.dir {
            SortDir::Asc => ordering,
            SortDir::Desc => ordering.reverse(),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package(name: &str, status: BuildState, outofdate: i32) -> SimplePackage {
        SimplePackage {
            id: 1,
            name: name.to_string(),
            // These tests are about the search and sort helpers, which do not
            // look at it; the packages screen owns the dependency filter.
            directly_requested: true,
            status: status.as_i32(),
            outofdate,
            latest_version: None,
            upstream_version: None,
            total_size: None,
        }
    }

    fn sized(name: &str, total_size: Option<i64>) -> SimplePackage {
        SimplePackage {
            total_size,
            ..package(name, BuildState::Successful, 0)
        }
    }

    /// Descending is what a Size column opens in, and it puts the biggest first
    /// with the unrecorded ones trailing -- below even a zero-byte package,
    /// since `None` sorts below every `Some`.
    #[test]
    fn sorting_by_size_puts_the_largest_first_and_the_unknown_last() {
        let mut packages = vec![
            sized("small", Some(10)),
            sized("unknown", None),
            sized("large", Some(9000)),
            sized("empty", Some(0)),
        ];
        sort_packages(
            &mut packages,
            Sort {
                key: SortKey::Size,
                dir: SortDir::Desc,
            },
        );
        let order: Vec<&str> = packages.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(order, ["large", "small", "empty", "unknown"]);
    }

    /// Reversing moves the unknowns to the other end as a group; they are never
    /// interleaved with real sizes in either direction.
    #[test]
    fn reversing_the_size_sort_keeps_the_unknown_together() {
        let mut packages = vec![
            sized("small", Some(10)),
            sized("unknown", None),
            sized("large", Some(9000)),
        ];
        sort_packages(
            &mut packages,
            Sort {
                key: SortKey::Size,
                dir: SortDir::Asc,
            },
        );
        let order: Vec<&str> = packages.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(order, ["unknown", "small", "large"]);
    }

    /// A Size column opens descending: the big ones are the reason to sort by
    /// size at all, so they should be on screen without a second click.
    #[test]
    fn size_opens_descending() {
        let sort = Sort {
            key: SortKey::Name,
            dir: SortDir::Asc,
        };
        assert_eq!(sort.toggled(SortKey::Size).dir, SortDir::Desc);
    }

    /// A page is a window on the list, not a prefix of it.
    #[test]
    fn a_page_is_the_slice_it_says_it_is() {
        let items: Vec<usize> = (0..250).collect();

        let first = paginate(&items, 0);
        assert_eq!(first.items.len(), PAGE_SIZE);
        assert_eq!(first.items[0], 0);
        assert_eq!(first.index, 0);
        assert_eq!(first.pages, 3);
        assert_eq!(first.first, 0);
        assert_eq!(first.total, 250);

        let second = paginate(&items, 1);
        assert_eq!(second.items[0], PAGE_SIZE);
        assert_eq!(second.first, PAGE_SIZE);

        // The last page is the remainder, not a short read of a full one.
        let last = paginate(&items, 2);
        assert_eq!(last.items.len(), 50);
        assert_eq!(last.items[0], 200);
    }

    /// Narrowing a filter while on a later page leaves the requested page past
    /// the end. Unclamped that renders an empty table, which reads as "nothing
    /// matches" when the rows are merely somewhere else.
    #[test]
    fn a_page_past_the_end_falls_back_to_the_last_one() {
        let items: Vec<usize> = (0..5).collect();
        let page = paginate(&items, 7);

        assert_eq!(page.index, 0);
        assert_eq!(page.pages, 1);
        assert_eq!(page.items.len(), 5);
    }

    /// An empty list is one empty page, so the pager reads "1 of 1" rather
    /// than dividing by zero on the way to saying so.
    #[test]
    fn an_empty_list_is_a_single_empty_page() {
        let page = paginate::<usize>(&[], 3);

        assert_eq!(page.pages, 1);
        assert_eq!(page.index, 0);
        assert!(page.items.is_empty());
        assert_eq!(page.total, 0);
    }

    /// The list shows `package/number`, so that is what typing it must match.
    /// Matching the package name alone meant copying what a row said found
    /// nothing at all.
    #[test]
    fn builds_are_filtered_by_the_name_the_list_shows() {
        let builds = vec![
            build(3, "hello", BuildState::Successful, Some(30)),
            build(4, "hello", BuildState::Successful, Some(40)),
            build(3, "neofetch", BuildState::Successful, Some(50)),
        ];

        // The package alone still finds all of its builds.
        let by_package = filter_builds(&builds, "hello", StatusFilter::ANY);
        assert_eq!(by_package.len(), 2);

        // And the identity finds the one.
        let by_id = filter_builds(&builds, "hello/4", StatusFilter::ANY);
        assert_eq!(by_id.len(), 1);
        assert_eq!(by_id[0].number, 4);

        // A build of another package with the same number is not it.
        let other = filter_builds(&builds, "neofetch/3", StatusFilter::ANY);
        assert_eq!(other.len(), 1);
        assert_eq!(other[0].pkg_name, "neofetch");

        assert!(filter_builds(&builds, "hello/9", StatusFilter::ANY).is_empty());
    }

    fn build(number: i32, pkg: &str, status: BuildState, start: Option<i64>) -> Build {
        Build {
            number,
            pkg_name: pkg.to_string(),
            version: "1.0-1".to_string(),
            status: status.as_i32(),
            start_time: start,
            end_time: None,
            platform: "x86_64".to_string(),
            size: None,
            peak_memory: None,
            waiting_reason: None,
        }
    }

    /// Searching by a fragment has to find compound names, which is most of
    /// them — `gtk` should find `lib32-gtk3`.
    #[test]
    fn searching_matches_anywhere_in_the_name() {
        let packages = vec![
            package("gtk3", BuildState::Successful, 0),
            package("lib32-gtk3", BuildState::Successful, 0),
            package("firefox", BuildState::Successful, 0),
        ];
        let found = filter_packages(&packages, "gtk", StatusFilter::ANY);
        assert_eq!(found.len(), 2);
        assert!(found.iter().all(|p| p.name.contains("gtk")));
    }

    #[test]
    fn searching_ignores_case_and_surrounding_space() {
        let packages = vec![package("Firefox", BuildState::Successful, 0)];
        for query in ["firefox", "FIREFOX", "  fire  "] {
            assert_eq!(
                filter_packages(&packages, query, StatusFilter::ANY).len(),
                1,
                "{query:?}"
            );
        }
    }

    /// An empty query is not a filter — it must not hide everything.
    #[test]
    fn an_empty_query_keeps_everything() {
        let packages = vec![
            package("a", BuildState::Successful, 0),
            package("b", BuildState::Failed, 0),
        ];
        assert_eq!(filter_packages(&packages, "", StatusFilter::ANY).len(), 2);
        assert_eq!(
            filter_packages(&packages, "   ", StatusFilter::ANY).len(),
            2
        );
    }

    #[test]
    fn the_status_filter_selects_one_state() {
        let packages = vec![
            package("a", BuildState::Successful, 0),
            package("b", BuildState::Failed, 0),
            package("c", BuildState::Failed, 0),
        ];
        let failed = filter_packages(&packages, "", StatusFilter(Some(BuildState::Failed)));
        assert_eq!(failed.len(), 2);
        assert_eq!(filter_packages(&packages, "", StatusFilter::ANY).len(), 3);
    }

    /// Sorting by status is asking "what needs me", so failures lead and
    /// healthy packages trail — not the numeric order of the enum.
    #[test]
    fn sorting_by_status_puts_problems_first() {
        let mut packages = vec![
            package("ok", BuildState::Successful, 0),
            package("broken", BuildState::Failed, 0),
            package("running", BuildState::Active, 0),
        ];
        sort_packages(
            &mut packages,
            Sort {
                key: SortKey::Status,
                dir: SortDir::Asc,
            },
        );
        let order: Vec<&str> = packages.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(order, ["broken", "running", "ok"]);
    }

    /// An out-of-date package needs attention too, so it sorts ahead of a
    /// current one in the same state.
    #[test]
    fn an_out_of_date_package_outranks_a_current_one() {
        let mut packages = vec![
            package("current", BuildState::Successful, 0),
            package("stale", BuildState::Successful, 1),
        ];
        sort_packages(
            &mut packages,
            Sort {
                key: SortKey::Status,
                dir: SortDir::Asc,
            },
        );
        assert_eq!(packages[0].name, "stale");
    }

    #[test]
    fn sorting_by_name_is_case_insensitive() {
        let mut packages = vec![
            package("zlib", BuildState::Successful, 0),
            package("Apache", BuildState::Successful, 0),
        ];
        sort_packages(
            &mut packages,
            Sort {
                key: SortKey::Name,
                dir: SortDir::Asc,
            },
        );
        assert_eq!(packages[0].name, "Apache");
    }

    #[test]
    fn sorting_builds_by_time_orders_by_when_they_started() {
        let mut builds = vec![
            build(1, "a", BuildState::Successful, Some(300)),
            build(2, "b", BuildState::Successful, Some(100)),
            build(3, "c", BuildState::Successful, Some(200)),
        ];
        sort_builds(
            &mut builds,
            Sort {
                key: SortKey::Time,
                dir: SortDir::Desc,
            },
        );
        assert_eq!(
            builds.iter().map(|b| b.number).collect::<Vec<_>>(),
            [1, 3, 2],
            "newest first"
        );
    }

    /// Grouping by package is only useful if each group reads as a history.
    #[test]
    fn builds_of_one_package_stay_newest_first_within_the_group() {
        let mut builds = vec![
            build(1, "pkg", BuildState::Successful, Some(100)),
            build(2, "pkg", BuildState::Successful, Some(300)),
            build(3, "other", BuildState::Successful, Some(200)),
        ];
        sort_builds(
            &mut builds,
            Sort {
                key: SortKey::Name,
                dir: SortDir::Asc,
            },
        );
        assert_eq!(
            builds.iter().map(|b| b.number).collect::<Vec<_>>(),
            [3, 2, 1]
        );
    }

    /// Clicking the active column reverses it; clicking a new one starts from
    /// the direction that column is usually read in.
    #[test]
    fn clicking_a_column_toggles_or_switches() {
        let by_name = Sort {
            key: SortKey::Name,
            dir: SortDir::Asc,
        };
        assert_eq!(by_name.toggled(SortKey::Name).dir, SortDir::Desc);
        // Time starts newest-first, which is what a log is read as.
        assert_eq!(by_name.toggled(SortKey::Time).dir, SortDir::Desc);
        // Everything else starts A-Z.
        assert_eq!(by_name.toggled(SortKey::Status).dir, SortDir::Asc);
    }

    #[test]
    fn a_status_filter_round_trips_through_its_id() {
        assert_eq!(StatusFilter::from_id("any"), StatusFilter::ANY);
        for state in [BuildState::Failed, BuildState::Successful] {
            let filter = StatusFilter(Some(state));
            assert_eq!(StatusFilter::from_id(&filter.id()), filter);
        }
        // An unrecognised value shows everything rather than nothing.
        assert_eq!(StatusFilter::from_id("999"), StatusFilter::ANY);
    }
}

// ---------------------------------------------------------------------------
// Shared controls
// ---------------------------------------------------------------------------

use crate::routes::Route;
use aurcache_common::build_state::BuildState as State;
use dioxus::prelude::*;

/// A list card's title, with an optional action on the right.
///
/// Shared so the two lists cannot drift: the packages header sat in a flex row
/// with a button and the builds header was a bare `h2`, which made one block a
/// button-height taller than the other. `min-h-8` matches a `btn-sm`, so the
/// header occupies the same height whether or not it has an action.
#[component]
pub fn ListHeader(title: String, children: Element) -> Element {
    rsx! {
        div { class: "flex items-center gap-2 min-h-8",
            h2 { class: "card-title", "{title}" }
            div { class: "flex-1" }
            {children}
        }
    }
}

/// How long the box has to be still before the address bar follows it.
///
/// The list filters on every keystroke; only the URL waits. Browsers rate-limit
/// history writes — Safari at roughly a hundred per thirty seconds — and typing
/// a word is easily a dozen.
const URL_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(300);

/// A search term that lives in the URL fragment, so a search can be linked to.
///
/// Seeded from the route on the way in and written back as it changes, always
/// with `replace` rather than `push`: one history entry per keystroke would turn
/// Back into a backspace.
///
/// `to_route` builds the route for a given term, which is all that differs
/// between the pages doing this — keeping it in one place is what stops the
/// fragment meaning something different on each.
///
/// `sync` is a parameter rather than the caller skipping this hook, because a
/// hook that is sometimes called breaks the order Dioxus identifies them by.
/// The packages list passes `false` when it is rendered behind the add dialog,
/// which owns the fragment there.
pub fn use_url_search(
    initial: String,
    sync: bool,
    to_route: fn(String) -> Route,
) -> Signal<String> {
    let term = use_signal(|| initial);

    use_effect(move || {
        if !sync {
            return;
        }
        // Reads `term`, so it re-runs when the box changes and not on every
        // unrelated render.
        let value = term();
        spawn(async move {
            gloo_timers::future::sleep(URL_DEBOUNCE).await;
            // Only the last keystroke of a burst writes: the earlier timers find
            // the box has moved on and do nothing.
            if *term.peek() == value {
                navigator().replace(to_route(value));
            }
        });
    });

    term
}

/// Search box and status filter, shared by both lists.
///
/// `children` is rendered inline after the status filter, for a list that has
/// one more control to sit beside it — the packages list puts its "show
/// dependencies" toggle there. Nothing passes it on the builds list.
#[component]
pub fn ListControls(
    query: Signal<String>,
    status: Signal<StatusFilter>,
    placeholder: String,
    shown: usize,
    total: usize,
    children: Element,
) -> Element {
    let mut query = query;
    let mut status = status;

    rsx! {
        div { class: "flex flex-wrap items-center gap-2",
            input {
                class: "input input-bordered input-sm w-full sm:w-64",
                r#type: "search",
                placeholder,
                value: "{query}",
                aria_label: "Filter by name",
                oninput: move |e| query.set(e.value()),
            }
            select {
                class: "select select-bordered select-sm",
                aria_label: "Filter by status",
                onchange: move |e| status.set(StatusFilter::from_id(&e.value())),
                option {
                    value: StatusFilter::ANY.id(),
                    selected: status() == StatusFilter::ANY,
                    "Any status"
                }
                for state in [
                    State::Failed,
                    State::Active,
                    State::WaitingForDeps,
                    State::Enqueued,
                    State::Successful,
                ] {
                    option {
                        key: "{state.as_i32()}",
                        value: StatusFilter(Some(state)).id(),
                        selected: status() == StatusFilter(Some(state)),
                        "{state_label(state)}"
                    }
                }
            }
            {children}
            div { class: "flex-1" }
            // Only worth saying when a filter is actually hiding something.
            if shown != total {
                span { class: "text-sm opacity-60", "{shown} of {total}" }
            }
        }
    }
}

fn state_label(state: State) -> &'static str {
    match state {
        State::Active => "Building",
        State::Successful => "Successful",
        State::Failed => "Failed",
        State::Enqueued => "Enqueued",
        State::WaitingForDeps => "Waiting for deps",
    }
}

/// A column header that sorts the list when clicked.
#[component]
pub fn SortableHeader(
    label: String,
    column: SortKey,
    sort: Signal<Sort>,
    class: String,
) -> Element {
    let mut sort = sort;
    let active = sort().key == column;
    // Conveys the ordering to assistive tech, which the arrow only shows
    // visually.
    let aria_sort = if active {
        match sort().dir {
            SortDir::Asc => "ascending",
            SortDir::Desc => "descending",
        }
    } else {
        "none"
    };

    rsx! {
        th { class,
            button {
                // `inline-flex`, not `flex`: a block-level flex container fills
                // the cell and packs its label at the left, so a `text-right`
                // header (Size) sorted the same as the values below it but did
                // not line up with them. Inline-level means the cell's own
                // text-align places it.
                class: "inline-flex items-center gap-1 hover:opacity-100 opacity-90",
                onclick: move |_| sort.set(sort().toggled(column)),
                aria_sort,
                "{label}"
                if active {
                    span { class: "text-xs opacity-70", "{sort().dir.arrow()}" }
                }
            }
        }
    }
}

/// How many rows a page holds.
///
/// 100 because that is what both lists previously fetched — the behaviour
/// people are used to — except that it was a ceiling with nothing beyond it
/// rather than the first page of everything.
pub const PAGE_SIZE: usize = 100;

/// One page of `items`, with the page number that was actually used.
///
/// The page is clamped rather than trusted. Narrowing a filter while on a later
/// page leaves the requested page past the end of a now-shorter list, and an
/// unclamped slice would render an empty table with no indication that the rows
/// are simply somewhere else.
#[must_use]
pub fn paginate<T: Clone>(items: &[T], page: usize) -> Page<T> {
    let pages = items.len().div_ceil(PAGE_SIZE).max(1);
    let index = page.min(pages - 1);
    let start = index * PAGE_SIZE;
    let end = (start + PAGE_SIZE).min(items.len());
    Page {
        items: items[start..end].to_vec(),
        index,
        pages,
        first: start,
        total: items.len(),
    }
}

/// One page of a list, and where it sits in the whole.
pub struct Page<T> {
    pub items: Vec<T>,
    /// Zero-based, and clamped to the list that actually exists.
    pub index: usize,
    /// At least one, so "page 1 of 1" is what an empty list reads as.
    pub pages: usize,
    /// Index of the first row on this page, for the human-readable range.
    pub first: usize,
    pub total: usize,
}

/// Page controls, shown only when there is more than one page.
///
/// Takes plain numbers rather than the [`Page`] itself: a component's props
/// must be `Clone + PartialEq`, and a generic payload would impose that on
/// every list this is used from for no benefit — it only ever renders counts.
#[component]
pub fn Pager(
    page: Signal<usize>,
    index: usize,
    pages: usize,
    first: usize,
    count: usize,
    total: usize,
) -> Element {
    let mut page = page;
    if pages <= 1 {
        return rsx! {};
    }
    let (from, to) = (first + 1, first + count);

    rsx! {
        div { class: "flex flex-wrap items-center gap-2 mt-2",
            span { class: "text-sm opacity-60", "Showing {from}\u{2013}{to} of {total}" }
            div { class: "flex-1" }
            div { class: "join",
                button {
                    class: "btn btn-sm join-item",
                    disabled: index == 0,
                    // Not `page -= 1`: the rendered page is clamped, so the
                    // signal can be further along than what is on screen, and
                    // stepping back from the stale value would appear to do
                    // nothing.
                    onclick: move |_| page.set(index.saturating_sub(1)),
                    aria_label: "Previous page",
                    "\u{2039}"
                }
                button { class: "btn btn-sm join-item no-animation", disabled: true,
                    "Page {index + 1} of {pages}"
                }
                button {
                    class: "btn btn-sm join-item",
                    disabled: index + 1 >= pages,
                    onclick: move |_| page.set(index + 1),
                    aria_label: "Next page",
                    "\u{203a}"
                }
            }
        }
    }
}
