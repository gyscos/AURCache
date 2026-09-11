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
    /// The worker that ran a build. Builds only; packages have no worker.
    Worker,
    /// How long a build ran, from the timestamps it records.
    Duration,
    /// A build's peak memory, from its cgroup. Builds only.
    Memory,
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
                    // Newest build, largest package, longest build, highest
                    // peak first: for these the interesting end is the top,
                    // where A-Z is the natural reading for everything else.
                    SortKey::Time | SortKey::Size | SortKey::Duration | SortKey::Memory => {
                        SortDir::Desc
                    }
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

    /// How this filter is spelled in a URL.
    ///
    /// Not [`Self::id`]: that is the numeric wire value, which is right for a
    /// `<select>` where nobody reads it and wrong for a path someone may read
    /// or share -- `/builds/status/building` says what `/builds/status/0` does
    /// not. `Active` is spelled "building" because that is the word the rest of
    /// the UI uses for it, including the Workers page link that lands here.
    #[must_use]
    pub fn slug(self) -> &'static str {
        match self.0 {
            None => "any",
            Some(BuildState::Active) => "building",
            Some(BuildState::Successful) => "successful",
            Some(BuildState::Failed) => "failed",
            Some(BuildState::Enqueued) => "enqueued",
            Some(BuildState::WaitingForDeps) => "waiting",
        }
    }

    /// Parse [`Self::slug`], falling back to "any" for anything unrecognised:
    /// a hand-edited or outdated URL should show every build rather than an
    /// error page.
    #[must_use]
    pub fn from_slug(slug: &str) -> Self {
        Self(match slug {
            "building" => Some(BuildState::Active),
            "successful" => Some(BuildState::Successful),
            "failed" => Some(BuildState::Failed),
            "enqueued" => Some(BuildState::Enqueued),
            "waiting" => Some(BuildState::WaitingForDeps),
            _ => None,
        })
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
            // Packages have no worker, duration or peak memory of their own;
            // all of them fall back to the name so the column headers stay
            // interchangeable.
            SortKey::Time | SortKey::Worker | SortKey::Duration | SortKey::Memory => {
                a.name.to_lowercase().cmp(&b.name.to_lowercase())
            }
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
    match build.worker_name.as_deref() {
        // The worker is part of what the row shows, so it is part of what the
        // filter matches -- the same rule that put the build number in here.
        // It is also what the Workers page links to, which is how "show me
        // this machine's builds" is spelled.
        Some(worker) => format!("{}/{} {worker}", build.pkg_name, build.number),
        None => format!("{}/{}", build.pkg_name, build.number),
    }
}

/// How long a build ran, in the same units as its timestamps.
///
/// `None` for a build that has not ended: it has no duration to speak of, and
/// sorting treats that the way it treats an unrecorded size or peak -- grouped
/// at one end rather than among the finished builds.
fn duration(build: &Build) -> Option<i64> {
    build
        .start_time
        .zip(build.end_time)
        .map(|(start, end)| end - start)
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
            // Unclaimed builds have no worker. `None` sorts before `Some`, so
            // ascending puts the queue first and descending puts it last --
            // either way they group together rather than scattering.
            SortKey::Worker => a
                .worker_name
                .as_deref()
                .map(str::to_lowercase)
                .cmp(&b.worker_name.as_deref().map(str::to_lowercase))
                // Within one worker, newest first, as the name grouping does.
                .then(b.number.cmp(&a.number)),
            // A build still running has no end yet, so no duration. `None`
            // sorts before `Some`, which under the descending order a Duration
            // column opens in puts the unfinished builds last, after the
            // longest — the interesting end first.
            SortKey::Duration => duration(a)
                .cmp(&duration(b))
                // Ties are ordered newest first for a stable readout.
                .then(b.number.cmp(&a.number)),
            // As with size, unrecorded peaks group at one end: `None` under
            // every `Some`, and last under the descending order a Memory
            // column opens in.
            SortKey::Memory => a
                .peak_memory
                .cmp(&b.peak_memory)
                .then(b.number.cmp(&a.number)),
        };
        match sort.dir {
            SortDir::Asc => ordering,
            SortDir::Desc => ordering.reverse(),
        }
    });
}

#[cfg(test)]
mod tests {

    /// A default view writes nothing, which is what keeps `/packages?` from
    /// becoming `/packages?s=any&o=name-asc&d=0`.
    #[test]
    fn a_default_view_encodes_to_nothing() {
        assert_eq!(ViewParams::default().to_string(), "");
    }

    /// Only what differs from the list's own default is written, so an
    /// untouched list has a clean URL and a changed one says what changed.
    #[test]
    fn only_non_default_state_reaches_the_url() {
        let default_sort = Sort {
            key: SortKey::Name,
            dir: SortDir::Asc,
        };
        let untouched =
            ViewParams::from_state(StatusFilter::ANY, default_sort, default_sort, false);
        assert_eq!(untouched.to_string(), "");

        let changed = ViewParams::from_state(
            StatusFilter(Some(BuildState::Failed)),
            Sort {
                key: SortKey::Worker,
                dir: SortDir::Desc,
            },
            default_sort,
            true,
        );
        assert_eq!(changed.to_string(), "s=failed&o=worker-desc&d=1");
    }

    /// Whatever `Display` writes must parse back to the same view: the URL is
    /// the only place this state lives across a reload.
    #[test]
    fn a_view_round_trips_through_its_query() {
        for view in [
            ViewParams::default(),
            ViewParams::with_status(BuildState::Active),
            ViewParams {
                status: Some(BuildState::Failed),
                sort: Some(Sort {
                    key: SortKey::Size,
                    dir: SortDir::Desc,
                }),
                dependencies: true,
            },
        ] {
            let encoded = view.to_string();
            assert_eq!(ViewParams::from(encoded.as_str()), view, "{encoded:?}");
        }
    }

    /// A hand-edited or outdated URL shows a list rather than an error: unknown
    /// keys and values fall back to the default instead of being rejected.
    #[test]
    fn an_unparsable_query_falls_back_to_the_default_view() {
        assert_eq!(ViewParams::from("s=nonsense"), ViewParams::default());
        assert_eq!(ViewParams::from("o=bogus-sideways"), ViewParams::default());
        assert_eq!(ViewParams::from("whatever"), ViewParams::default());
        // A key we do not know is ignored, and the rest still applies.
        assert_eq!(
            ViewParams::from("zz=1&s=failed").status,
            Some(BuildState::Failed)
        );
    }
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
            worker_name: None,
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

    /// A build still running has no duration, so it sorts last under the
    /// descending order a Duration column opens in, after the longest.
    #[test]
    fn sorting_builds_by_duration_puts_the_longest_first() {
        let mut builds = vec![
            build(1, "a", BuildState::Successful, Some(100)),
            build(2, "b", BuildState::Successful, Some(100)),
            build(3, "c", BuildState::Successful, Some(100)),
        ];
        // #1 runs 100s, #2 runs 300s, #3 is still running (end not recorded).
        builds[0].end_time = Some(200);
        builds[1].end_time = Some(400);
        sort_builds(
            &mut builds,
            Sort {
                key: SortKey::Duration,
                dir: SortDir::Desc,
            },
        );
        assert_eq!(
            builds.iter().map(|b| b.number).collect::<Vec<_>>(),
            [2, 1, 3],
            "longest first, still-running last"
        );
    }

    /// An unrecorded peak groups at the end — it is missing data, not a build
    /// that used no memory — so it lands last under the descending order a
    /// Memory column opens in.
    #[test]
    fn sorting_builds_by_peak_memory_puts_the_highest_first() {
        let mut builds = vec![
            build(1, "a", BuildState::Successful, Some(100)),
            build(2, "b", BuildState::Successful, Some(100)),
            build(3, "c", BuildState::Successful, Some(100)),
        ];
        builds[0].peak_memory = Some(300);
        builds[1].peak_memory = Some(100);
        sort_builds(
            &mut builds,
            Sort {
                key: SortKey::Memory,
                dir: SortDir::Desc,
            },
        );
        assert_eq!(
            builds.iter().map(|b| b.number).collect::<Vec<_>>(),
            [1, 2, 3],
            "highest first, unrecorded last"
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
        // So do the other measurements whose interesting end is the top: the
        // longest build and the highest peak first.
        assert_eq!(by_name.toggled(SortKey::Duration).dir, SortDir::Desc);
        assert_eq!(by_name.toggled(SortKey::Memory).dir, SortDir::Desc);
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
    // Not a `fn` pointer: the builds list has a status filter to carry through
    // the same URL, so the route a term maps to depends on more than the term.
    to_route: impl Fn(String) -> Route + Clone + 'static,
) -> Signal<String> {
    let term = use_signal(|| initial);

    use_effect(move || {
        if !sync {
            return;
        }
        // Reads `term`, so it re-runs when the box changes and not on every
        // unrelated render.
        let value = term();
        let to_route = to_route.clone();
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

impl SortKey {
    /// The spelling used in a URL.
    #[must_use]
    pub fn slug(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Status => "status",
            Self::Time => "time",
            Self::Size => "size",
            Self::Worker => "worker",
            Self::Duration => "duration",
            Self::Memory => "memory",
        }
    }

    fn from_slug(slug: &str) -> Option<Self> {
        match slug {
            "name" => Some(Self::Name),
            "status" => Some(Self::Status),
            "time" => Some(Self::Time),
            "size" => Some(Self::Size),
            "worker" => Some(Self::Worker),
            "duration" => Some(Self::Duration),
            "memory" => Some(Self::Memory),
            _ => None,
        }
    }
}

impl SortDir {
    #[must_use]
    pub fn slug(self) -> &'static str {
        match self {
            Self::Asc => "asc",
            Self::Desc => "desc",
        }
    }

    fn from_slug(slug: &str) -> Option<Self> {
        match slug {
            "asc" => Some(Self::Asc),
            "desc" => Some(Self::Desc),
            _ => None,
        }
    }
}

/// The filter and sort of a list, carried in the URL's query string.
///
/// A spread query segment (`?:..view`), so the *whole* query is this type and
/// its encoding is ours. That is what makes it usable at all: dioxus writes a
/// named parameter as `name=` whether or not it has a value, so `?:status`
/// alone put a dangling `?status=` on every unfiltered URL. Owning the string
/// means a default view writes nothing.
///
/// One `?` survives, because the generated `Display` emits it before this type
/// is consulted (`router-macro/src/query.rs`, `write!(f, "?{}", ...)`), so an
/// unfiltered list is `/packages?`. That is DioxusLabs/dioxus#5792, and #5793
/// is the one-line fix; when it lands the trailing `?` disappears with no
/// change here. `an_empty_search_leaves_no_trace_in_the_url` allows it and
/// nothing else.
///
/// Values are slugs rather than the numeric `StatusFilter::id`, since a URL is
/// read and shared: `?s=failed&o=name-desc` says what `?s=2&o=0-1` does not.
/// Neither `&` nor `=` is in dioxus's `QUERY_ASCII_SET`, so they survive
/// unescaped and the query stays legible.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ViewParams {
    pub status: Option<BuildState>,
    /// `None` means "the list's own default order", so the common view carries
    /// no `o=` at all rather than spelling out what it would have done anyway.
    pub sort: Option<Sort>,
    pub dependencies: bool,
}

impl ViewParams {
    /// Build from live control state, dropping anything that matches the
    /// list's default so an untouched list has a clean URL.
    #[must_use]
    pub fn from_state(
        status: StatusFilter,
        sort: Sort,
        default_sort: Sort,
        dependencies: bool,
    ) -> Self {
        Self {
            status: status.0,
            sort: (sort != default_sort).then_some(sort),
            dependencies,
        }
    }

    /// The sort to apply, given what this list would use by default.
    #[must_use]
    pub fn sort_or(&self, default_sort: Sort) -> Sort {
        self.sort.unwrap_or(default_sort)
    }

    #[must_use]
    pub fn status_filter(&self) -> StatusFilter {
        StatusFilter(self.status)
    }

    /// A view filtered to one status and nothing else, for links into a list.
    #[must_use]
    pub fn with_status(status: BuildState) -> Self {
        Self {
            status: Some(status),
            ..Self::default()
        }
    }
}

impl std::fmt::Display for ViewParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut sep = "";
        if self.status.is_some() {
            write!(f, "s={}", StatusFilter(self.status).slug())?;
            sep = "&";
        }
        if let Some(sort) = self.sort {
            write!(f, "{sep}o={}-{}", sort.key.slug(), sort.dir.slug())?;
            sep = "&";
        }
        if self.dependencies {
            write!(f, "{sep}d=1")?;
        }
        Ok(())
    }
}

impl From<&str> for ViewParams {
    /// Parse what [`Display`] writes. Unrecognised keys and values are ignored
    /// rather than rejected: a hand-edited or outdated URL should still show a
    /// list, and `FromQuery` has nowhere to report an error to anyway.
    fn from(query: &str) -> Self {
        let mut view = Self::default();
        for (key, value) in query.split('&').filter_map(|p| p.split_once('=')) {
            match key {
                "s" => view.status = StatusFilter::from_slug(value).0,
                "o" => {
                    if let Some((key, dir)) = value.split_once('-')
                        && let (Some(key), Some(dir)) =
                            (SortKey::from_slug(key), SortDir::from_slug(dir))
                    {
                        view.sort = Some(Sort { key, dir });
                    }
                }
                "d" => view.dependencies = value == "1",
                _ => {}
            }
        }
        view
    }
}

/// Keep the URL in step with the filter and sort controls.
///
/// `replace`, never `push`, for the same reason the search box uses it: a
/// person narrowing a list is refining one view, not walking a trail, and Back
/// should leave the list rather than undo a dropdown one notch at a time. That
/// is also why this is one hook rather than one per control -- every change
/// rewrites the whole route from live state, so the term and the controls
/// cannot overwrite each other's half of the URL.
///
/// `to_route` reads the control signals itself, so this only has to fire when
/// one of them changes; the term is read with `peek` because the search box
/// already has its own debounced writer and subscribing here would race it.
///
/// `sync` matches `use_url_search`: the packages list passes `false` behind the
/// add dialog, which owns the URL there. A parameter rather than the caller
/// skipping the hook, because Dioxus identifies hooks by order.
pub fn use_url_view(
    term: Signal<String>,
    sync: bool,
    to_route: impl Fn(String) -> Route + Clone + 'static,
) {
    // Seeded with the route we arrived on, *not* `None`. Starting empty made
    // the first effect run navigate to the URL the page was already showing;
    // that re-entered the router on mount and the app intermittently failed to
    // come up at all. Nothing is written until a control actually moves.
    let seed = to_route.clone();
    let mut previous = use_signal(move || seed(term.peek().clone()));
    use_effect(move || {
        if !sync {
            return;
        }
        let route = to_route(term.peek().clone());
        // Unchanged means an unrelated render rather than a control moving, and
        // replacing again would be a wasted navigation.
        if *previous.peek() == route {
            return;
        }
        previous.set(route.clone());
        navigator().replace(route);
    });
}
