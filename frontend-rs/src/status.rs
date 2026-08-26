//! Build status as a coloured badge.

use aurcache_types::build_state::BuildState;
use dioxus::prelude::*;

/// The colour a state is shown in, independent of what it is called.
///
/// Shared so a package and a build in the same state read the same, while each
/// keeps its own wording: a package is "up to date", the build that produced it
/// is "successful".
fn badge_class(state: Option<BuildState>) -> &'static str {
    match state {
        Some(BuildState::Active) => "badge-info",
        Some(BuildState::Successful) => "badge-success",
        Some(BuildState::Failed) => "badge-error",
        // The two pending states are not failures and not progress, so they
        // stay uncoloured — but visible, or they read as an empty cell.
        // `badge-ghost` has no background on this theme.
        Some(BuildState::Enqueued) => "badge-neutral",
        Some(BuildState::WaitingForDeps) => "badge-outline",
        // Only reachable against a newer server that added a state.
        None => "badge-outline",
    }
}

/// A build's status, worded for a build rather than for a package.
#[component]
pub fn BuildStatusBadge(status: i32) -> Element {
    let state = BuildState::from_i32(status);
    let label = match state {
        Some(BuildState::Active) => "building",
        Some(BuildState::Successful) => "successful",
        Some(BuildState::Failed) => "failed",
        Some(BuildState::Enqueued) => "enqueued",
        Some(BuildState::WaitingForDeps) => "waiting for deps",
        None => "unknown",
    };
    let class = badge_class(state);
    rsx! { span { class: "badge {class} badge-sm whitespace-nowrap", "{label}" } }
}

/// Status as a coloured badge, the way the Flutter table did it with chips.
///
/// Matches on [`BuildState`] rather than on the raw integer: the arms are
/// exhaustive, so adding a state server-side breaks this at compile time
/// instead of silently rendering as "unknown". Writing this against bare
/// numbers is how `WaitingForDeps` got missed the first time.
#[component]
pub fn StatusBadge(status: i32, outofdate: i32) -> Element {
    let state = BuildState::from_i32(status);
    // A package whose last build succeeded but that has newer sources upstream
    // is its own thing: successful, yet needing attention.
    let outdated = matches!(state, Some(BuildState::Successful)) && outofdate != 0;
    let label = match state {
        Some(BuildState::Active) => "building",
        Some(BuildState::Successful) if outdated => "out of date",
        Some(BuildState::Successful) => "up to date",
        Some(BuildState::Failed) => "failed",
        Some(BuildState::Enqueued) => "enqueued",
        Some(BuildState::WaitingForDeps) => "waiting for deps",
        None => "unknown",
    };
    let class = if outdated {
        "badge-warning"
    } else {
        badge_class(state)
    };
    rsx! { span { class: "badge {class} badge-sm whitespace-nowrap", "{label}" } }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Render a component to HTML so its output can be asserted on.
    fn render(element: fn() -> Element) -> String {
        let mut dom = VirtualDom::new(element);
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    /// Each build state must render its own label and colour. This is the test
    /// that would have caught `WaitingForDeps` silently rendering as "unknown"
    /// while the badges were matched on bare integers.
    #[test]
    fn every_build_state_has_its_own_badge() {
        for (state, label, class) in [
            (BuildState::Active, "building", "badge-info"),
            (BuildState::Successful, "up to date", "badge-success"),
            (BuildState::Failed, "failed", "badge-error"),
            (BuildState::Enqueued, "enqueued", "badge-neutral"),
            (
                BuildState::WaitingForDeps,
                "waiting for deps",
                "badge-outline",
            ),
        ] {
            let status = state.as_i32();
            let html = {
                let mut dom = VirtualDom::new_with_props(
                    StatusBadge,
                    StatusBadgeProps {
                        status,
                        outofdate: 0,
                    },
                );
                dom.rebuild_in_place();
                dioxus_ssr::render(&dom)
            };
            assert!(
                html.contains(label),
                "{state:?} should render {label:?}: {html}"
            );
            assert!(html.contains(class), "{state:?} should use {class}: {html}");
        }
    }

    /// A successful build that is out of date reads differently from one that
    /// is current, and the distinction is easy to invert.
    #[test]
    fn a_successful_but_outdated_build_is_flagged() {
        let mut dom = VirtualDom::new_with_props(
            StatusBadge,
            StatusBadgeProps {
                status: BuildState::Successful.as_i32(),
                outofdate: 1,
            },
        );
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains("out of date"), "{html}");
        assert!(html.contains("badge-warning"), "{html}");
    }

    /// A state this frontend does not know must not be styled as if it were
    /// understood.
    #[test]
    fn an_unrecognised_state_renders_as_unknown() {
        let mut dom = VirtualDom::new_with_props(
            StatusBadge,
            StatusBadgeProps {
                status: 99,
                outofdate: 0,
            },
        );
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains("unknown"), "{html}");
    }

    /// No state may render as `badge-ghost`.
    ///
    /// It has no background on this theme, so "enqueued" and "waiting for
    /// deps" showed as bare text beside the coloured badges and read as an
    /// empty cell rather than a status. The other tests here assert on class
    /// names, which cannot tell whether the result is legible — so this pins
    /// the one class that is known not to be.
    #[test]
    fn no_state_renders_as_an_invisible_badge() {
        for status in [
            BuildState::Active.as_i32(),
            BuildState::Successful.as_i32(),
            BuildState::Failed.as_i32(),
            BuildState::Enqueued.as_i32(),
            BuildState::WaitingForDeps.as_i32(),
            // The unknown-state arm too.
            99,
        ] {
            for outofdate in [0, 1] {
                let mut dom =
                    VirtualDom::new_with_props(StatusBadge, StatusBadgeProps { status, outofdate });
                dom.rebuild_in_place();
                let html = dioxus_ssr::render(&dom);
                assert!(
                    !html.contains("badge-ghost"),
                    "status {status} (outofdate {outofdate}) renders invisibly: {html}"
                );
            }
        }
    }

    // Silences the unused-fn warning: `render` is kept as the simple form for
    // components without props.
    #[allow(dead_code)]
    fn _unused() {
        let _ = render;
    }
}
