//! Build status as a coloured badge.

use aurcache_types::build_state::BuildState;
use dioxus::prelude::*;

/// Status as a coloured badge, the way the Flutter table did it with chips.
///
/// Matches on [`BuildState`] rather than on the raw integer: the arms are
/// exhaustive, so adding a state server-side breaks this at compile time
/// instead of silently rendering as "unknown". Writing this against bare
/// numbers is how `WaitingForDeps` got missed the first time.
#[component]
pub fn StatusBadge(status: i32, outofdate: i32) -> Element {
    let (label, class) = match BuildState::from_i32(status) {
        Some(BuildState::Active) => ("building", "badge-info"),
        Some(BuildState::Successful) if outofdate != 0 => ("out of date", "badge-warning"),
        Some(BuildState::Successful) => ("up to date", "badge-success"),
        Some(BuildState::Failed) => ("failed", "badge-error"),
        Some(BuildState::Enqueued) => ("enqueued", "badge-ghost"),
        Some(BuildState::WaitingForDeps) => ("waiting for deps", "badge-ghost"),
        // Only reachable against a newer server that added a state.
        None => ("unknown", "badge-ghost"),
    };
    rsx! { span { class: "badge {class} badge-sm", "{label}" } }
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
            (BuildState::Enqueued, "enqueued", "badge-ghost"),
            (
                BuildState::WaitingForDeps,
                "waiting for deps",
                "badge-ghost",
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

    // Silences the unused-fn warning: `render` is kept as the simple form for
    // components without props.
    #[allow(dead_code)]
    fn _unused() {
        let _ = render;
    }
}
