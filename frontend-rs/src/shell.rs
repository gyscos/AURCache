//! The app frame: side menu plus the content area every screen renders into.
//!
//! Laid out as a daisyUI drawer, which is permanent from `lg` up and slides
//! over the content below it. That is the same responsive behaviour as the Dart
//! `MenuShell`, without needing to measure the viewport in Rust.

use crate::dates::DateStylePicker;
use crate::routes::{MenuEntry, Route};
use crate::theme::ThemePicker;
use dioxus::prelude::*;

/// Where the docs and the source live. Rendered as real links rather than
/// click handlers so they behave like links — middle-click, copy address.
const DOCS_URL: &str =
    "https://lukas-heiligenbrunner.github.io/AURCache/docs/overview/introduction";
const GITHUB_URL: &str = "https://github.com/Lukas-Heiligenbrunner/AURCache";

/// Checkbox id the drawer toggle and its labels agree on.
const DRAWER_ID: &str = "menu-drawer";

#[component]
pub fn MenuShell() -> Element {
    rsx! {
        div { class: "drawer lg:drawer-open",
            input { id: DRAWER_ID, r#type: "checkbox", class: "drawer-toggle" }

            div { class: "drawer-content flex flex-col min-h-screen bg-base-200",
                // Only exists below `lg`, where the drawer is collapsed.
                div { class: "navbar bg-base-100 shadow-sm lg:hidden",
                    label {
                        r#for: DRAWER_ID,
                        class: "btn btn-square btn-ghost drawer-button",
                        MenuIcon {}
                    }
                    span { class: "font-bold px-2", "AURCache" }
                }

                main { class: "flex-1 p-4 lg:p-6", Outlet::<Route> {} }
            }

            div { class: "drawer-side z-10",
                label {
                    r#for: DRAWER_ID,
                    class: "drawer-overlay",
                    aria_label: "Close menu",
                }
                SideMenu {}
            }
        }
    }
}

#[component]
fn SideMenu() -> Element {
    // The live route, so the active entry is derived rather than tracked in a
    // signal that could fall out of step with the URL. The mapping itself lives
    // on `Route` so it can be tested without rendering.
    let active = use_route::<Route>().menu_entry();
    let mut preferences_open = use_signal(|| false);

    rsx! {
            aside { class: "bg-base-100 w-64 min-h-full flex flex-col",
                div { class: "flex items-center gap-3 px-5 pt-6 pb-4",
                    LogoIcon {}
                    div {
                        div { class: "font-black text-base leading-tight", "AURCache" }
                        div { class: "text-xs opacity-70 leading-tight",
                            "The Archlinux AUR"
                            br {}
                            "build server"
                        }
                    }
                }

                MenuSection { title: "General",
                    MenuLink {
                        to: Route::Dashboard {},
                        label: "Dashboard",
                        active: active == Some(MenuEntry::Dashboard),
                        icon: rsx! { DashboardIcon {} },
                    }
                    MenuLink {
                        to: Route::Packages { q: String::new() },
                        label: "Packages",
    active: active == Some(MenuEntry::Packages),
                        icon: rsx! { PackagesIcon {} },
                    }
                    MenuLink {
                        to: Route::Builds { q: String::new() },
                        label: "Builds",
    active: active == Some(MenuEntry::Builds),
                        icon: rsx! { BuildsIcon {} },
                    }
                    MenuLink {
                        to: Route::Activities {},
                        label: "Activities",
                        active: active == Some(MenuEntry::Activities),
                        icon: rsx! { ActivitiesIcon {} },
                    }
                    MenuLink {
                        to: Route::Workers {},
                        label: "Workers",
                        active: active == Some(MenuEntry::Workers),
                        icon: rsx! { WorkersIcon {} },
                    }
                }

                MenuSection { title: "Settings",
                    MenuLink {
                        to: Route::Settings {},
                        label: "Settings",
                        active: active == Some(MenuEntry::Settings),
                        icon: rsx! { SettingsIcon {} },
                    }
                    MenuLink {
                        to: Route::ConfigFiles {},
                        label: "Config files",
                        active: active == Some(MenuEntry::ConfigFiles),
                        icon: rsx! { ConfigFilesIcon {} },
                    }
                    ExternalMenuLink { href: DOCS_URL, label: "Help" }
                    // Client-side only: a display preference, kept in the
                    // browser rather than in server settings. See `crate::theme`.
                    MenuButton {
                    label: "Preferences",
                    onclick: move |_| preferences_open.set(true),
                    icon: rsx! { SlidersIcon {} },
                }
                }

                div { class: "flex-1" }

                PreferencesDialog { open: preferences_open }

            MenuSection { title: "Project info",
                    ExternalMenuLink { href: GITHUB_URL, label: "GitHub" }
                    div { class: "px-5 pt-1 pb-4 text-xs opacity-50",
                        "Version {env!(\"CARGO_PKG_VERSION\")}"
                    }
                }
            }
        }
}

#[component]
fn MenuSection(title: String, children: Element) -> Element {
    rsx! {
        div {
            div { class: "flex items-center gap-3 px-5 pt-2 pb-2",
                span { class: "text-xs opacity-60 whitespace-nowrap", "{title}" }
                div { class: "h-px bg-base-content/20 flex-1" }
            }
            nav { class: "flex flex-col", {children} }
        }
    }
}

/// One navigation entry.
///
/// A `Link` rather than an onclick handler, so it renders a real `href`: the
/// address is copyable and the browser handles modifier-clicks itself.
#[component]
fn MenuLink(to: Route, label: String, active: bool, icon: Element) -> Element {
    let classes = if active {
        "flex items-center gap-4 px-5 py-2.5 bg-primary text-primary-content font-bold"
    } else {
        "flex items-center gap-4 px-5 py-2.5 hover:bg-base-200"
    };

    rsx! {
        Link {
            to,
            class: classes,
            // No `aria_current` here on purpose: `Link` already sets it when
            // the href is exactly the current route. `active` is broader — it
            // stays on for a whole section, so `/build/3` keeps "Builds" lit —
            // and marking that as `aria-current="page"` would tell a screen
            // reader the section link is the page being viewed, which it is not.
            {icon}
            span { class: "text-sm", "{label}" }
        }
    }
}

/// A menu entry that acts rather than navigates.
///
/// A `button`, not a link: it opens something in place, and rendering it as an
/// anchor would offer a middle-click that goes nowhere.
#[component]
fn MenuButton(label: String, onclick: EventHandler<MouseEvent>, icon: Element) -> Element {
    rsx! {
        button {
            class: "flex items-center gap-4 px-5 py-2.5 hover:bg-base-200 w-full text-left",
            onclick: move |e| onclick.call(e),
            {icon}
            span { class: "text-sm", "{label}" }
        }
    }
}

/// Display preferences, kept out of the sidebar itself.
///
/// They are per-browser rather than account settings — the server supplies a
/// default and this overrides it locally — so the dialog says so rather than
/// leaving someone to wonder why a colleague sees something else.
#[component]
fn PreferencesDialog(open: Signal<bool>) -> Element {
    let mut open = open;

    rsx! {
        div {
            class: if open() { "modal modal-open" } else { "modal" },
            role: "dialog",
            aria_modal: "true",
            aria_label: "UI preferences",
            div { class: "modal-box",
                h3 { class: "font-bold text-lg", "UI preferences" }
                p { class: "text-sm opacity-60",
                    "Stored in this browser, not on the server."
                }
                div { class: "pt-2",
                    ThemePicker {}
                    DateStylePicker {}
                }
                div { class: "modal-action",
                    button {
                        class: "btn btn-sm",
                        onclick: move |_| open.set(false),
                        "Done"
                    }
                }
            }
            // Clicking away closes it, which is what the backdrop is for.
            button {
                class: "modal-backdrop",
                onclick: move |_| open.set(false),
                aria_label: "Close preferences",
                "Close"
            }
        }
    }
}

/// Leaves the app, so it is a plain anchor rather than a router `Link`.
#[component]
fn ExternalMenuLink(href: String, label: String) -> Element {
    rsx! {
        a {
            class: "flex items-center gap-4 px-5 py-2.5 hover:bg-base-200",
            href,
            target: "_blank",
            rel: "noopener noreferrer",
            ExternalIcon {}
            span { class: "text-sm", "{label}" }
        }
    }
}

// ---------------------------------------------------------------------------
// Icons
//
// Inline SVG paths rather than asset files: the prototype has no asset
// pipeline, and inlining keeps the menu to one dependency-free module. Each is
// 20x20 on a 24-unit grid, stroked in `currentColor` so it follows the theme
// and the active-row colour without a second rule.
// ---------------------------------------------------------------------------

#[component]
fn Icon(path: &'static str) -> Element {
    rsx! {
        svg {
            class: "h-5 w-5 shrink-0",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.8",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            path { d: path }
        }
    }
}

/// Stands in for `assets/icons/icon.svg` in the Dart tree: a package glyph,
/// drawn inline so the prototype needs no asset pipeline.
#[component]
fn LogoIcon() -> Element {
    rsx! {
        svg {
            class: "h-11 w-11 shrink-0 text-primary",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.5",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            path { d: "M21 8v8l-9 4-9-4V8l9-4 9 4Zm-18 0 9 4 9-4M12 12v8" }
        }
    }
}

#[component]
fn MenuIcon() -> Element {
    rsx! { Icon { path: "M4 6h16M4 12h16M4 18h16" } }
}

#[component]
fn DashboardIcon() -> Element {
    rsx! { Icon { path: "M4 13h6V4H4v9Zm10 7h6v-9h-6v9ZM4 20h6v-4H4v4Zm10-11h6V4h-6v5Z" } }
}

#[component]
fn BuildsIcon() -> Element {
    rsx! { Icon { path: "M14 7h7M14 12h7M14 17h7M3 7l2 2 3-3M3 16l2 2 3-3" } }
}

#[component]
fn PackagesIcon() -> Element {
    rsx! {
        Icon { path: "M21 8v8l-9 4-9-4V8l9-4 9 4Zm-18 0 9 4 9-4M12 12v8" }
    }
}

#[component]
fn ActivitiesIcon() -> Element {
    rsx! { Icon { path: "M4 6h16M4 12h16M4 18h10" } }
}

#[component]
fn WorkersIcon() -> Element {
    rsx! {
        Icon { path: "M4 5h16v5H4V5Zm0 9h16v5H4v-5Zm3-6.5h.01M7 16.5h.01" }
    }
}

#[component]
fn SettingsIcon() -> Element {
    rsx! {
        Icon { path: "M12 15a3 3 0 1 0 0-6 3 3 0 0 0 0 6Zm8-3a8 8 0 0 0-.13-1.4l2-1.6-2-3.4-2.4 1a8 8 0 0 0-2.4-1.4L14.7 2h-4l-.4 2.6a8 8 0 0 0-2.4 1.4l-2.4-1-2 3.4 2 1.6a8 8 0 0 0 0 2.8l-2 1.6 2 3.4 2.4-1a8 8 0 0 0 2.4 1.4l.4 2.6h4l.4-2.6a8 8 0 0 0 2.4-1.4l2.4 1 2-3.4-2-1.6c.08-.46.13-.93.13-1.4Z" }
    }
}

#[component]
fn ConfigFilesIcon() -> Element {
    rsx! {
        Icon { path: "M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8l-5-5Zm0 0v5h5M9 13h6M9 17h4" }
    }
}

#[component]
fn SlidersIcon() -> Element {
    rsx! {
        Icon { path: "M4 6h10M18 6h2M4 12h4M12 12h8M4 18h10M18 18h2M14 4v4M8 10v4M14 16v4" }
    }
}

#[component]
fn ExternalIcon() -> Element {
    rsx! {
        Icon { path: "M14 4h6v6M20 4l-8 8M18 14v5a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V7a1 1 0 0 1 1-1h5" }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dioxus::history::{History, MemoryHistory};
    use dioxus::router::components::HistoryProvider;
    use std::rc::Rc;

    /// The whole app rendered at one route, without a browser.
    ///
    /// Goes through the real `Router`, so this covers what the unit tests on
    /// `Route` cannot: that the route table, the shell, the outlet and the menu
    /// actually compose.
    /// The whole app rendered at one route with an in-memory history.
    ///
    /// Goes through the real `Router`, so this covers what the unit tests on
    /// `Route` cannot: that the route table, the shell, the outlet and the menu
    /// actually compose.
    ///
    /// The history callback is built inside the component on purpose —
    /// `Callback::new` needs the Dioxus runtime, which only exists in scope.
    #[component]
    fn MemoryApp(path: String) -> Element {
        rsx! {
            HistoryProvider {
                history: move |_| Rc::new(MemoryHistory::with_initial_path(path.clone())) as Rc<dyn History>,
                Router::<Route> {}
            }
        }
    }

    fn render_at(path: &str) -> String {
        let mut dom = VirtualDom::new_with_props(
            MemoryApp,
            MemoryAppProps {
                path: path.to_string(),
            },
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    /// The menu is part of the layout, so it renders once around whatever
    /// screen is current — not per screen.
    #[test]
    fn every_screen_renders_inside_the_menu_shell() {
        for path in ["/", "/settings", "/config-files", "/workers"] {
            let html = render_at(path);
            assert!(html.contains("AURCache"), "{path} lost the menu header");
            assert!(html.contains("Dashboard"), "{path} lost the menu: {html}");
            assert!(
                html.contains("drawer-side"),
                "{path} did not render the drawer"
            );
        }
    }

    /// Links are rendered as real `href`s, which is what makes them copyable
    /// and middle-clickable. A handler-only menu would render bare `<a>`s.
    #[test]
    fn menu_entries_are_real_links() {
        let html = render_at("/");
        for href in ["/builds", "/packages", "/settings", "/workers"] {
            assert!(
                html.contains(&format!("href=\"{href}\"")),
                "no link to {href}: {html}"
            );
        }
    }

    /// The exact current page is marked for assistive tech, not only coloured.
    #[test]
    fn the_current_page_is_marked_for_assistive_tech() {
        let html = render_at("/settings");
        assert!(html.contains("aria-current=\"page\""), "{html}");
    }

    /// Links are plain paths, which is what the server's app-shell fallback
    /// makes work. A stray `#` here would mean the router was still routing on
    /// the fragment.
    #[test]
    fn links_are_paths_not_fragments() {
        let html = render_at("/");
        for route in ["/builds", "/packages", "/settings"] {
            assert!(
                html.contains(&format!("href=\"{route}\"")),
                "no plain-path link to {route}: {html}"
            );
        }
        assert!(
            !html.contains("href=\"#"),
            "a fragment href survived: {html}"
        );
    }

    /// An unknown route still renders the frame, so the menu is there to
    /// navigate away with rather than a bare error page.
    #[test]
    fn an_unknown_route_renders_the_not_found_screen_in_the_shell() {
        let html = render_at("/no/such/page");
        assert!(html.contains("Not found"), "{html}");
        assert!(html.contains("drawer-side"), "menu missing: {html}");
        // Nothing is the current page, so nothing claims to be.
        assert!(!html.contains("aria-current"), "{html}");
    }
}
