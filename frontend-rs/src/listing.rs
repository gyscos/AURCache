//! Sorting and filtering for the package and build lists.
//!
//! Done in the browser over the fetched page rather than as query parameters:
//! the lists are already fetched whole, so this stays instant and needs no API
//! change. If a repository ever outgrows one page, this is the thing that has
//! to move server-side — the pure functions here are what would be ported.

use aurcache_client::{Build, SimplePackage};
use aurcache_types::build_state::BuildState;

/// Which column a list is ordered by.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SortKey {
    Name,
    Status,
    Time,
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
                    SortKey::Time => SortDir::Desc,
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
        .filter(|build| name_matches(&build.pkg_name, query) && status.matches(build.status))
        .cloned()
        .collect()
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
                .then(b.start_time.cmp(&a.start_time)),
            SortKey::Status => status_rank(a.status).cmp(&status_rank(b.status)),
            SortKey::Time => a.start_time.cmp(&b.start_time),
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
            status: status.as_i32(),
            outofdate,
            latest_version: None,
            upstream_version: None,
        }
    }

    fn build(id: i32, pkg: &str, status: BuildState, start: Option<i64>) -> Build {
        Build {
            id,
            pkg_id: 1,
            pkg_name: pkg.to_string(),
            version: "1.0-1".to_string(),
            status: status.as_i32(),
            start_time: start,
            end_time: None,
            platform: "x86_64".to_string(),
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
            builds.iter().map(|b| b.id).collect::<Vec<_>>(),
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
        assert_eq!(builds.iter().map(|b| b.id).collect::<Vec<_>>(), [3, 2, 1]);
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

use aurcache_types::build_state::BuildState as State;
use dioxus::prelude::*;

/// Search box and status filter, shared by both lists.
#[component]
pub fn ListControls(
    query: Signal<String>,
    status: Signal<StatusFilter>,
    placeholder: String,
    shown: usize,
    total: usize,
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
                class: "flex items-center gap-1 hover:opacity-100 opacity-90",
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
