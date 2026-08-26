//! Which architectures a package is built for.
//!
//! Shared because the same list is picked twice — once when a package is added
//! and again when it is edited — and two copies would drift the moment an
//! architecture is added.

use dioxus::prelude::*;

/// The architectures a build can target.
///
/// Whether a given package *can* build for one of these depends entirely on its
/// PKGBUILD, which is why the picker says so rather than pretending to know.
pub const ALL: [&str; 3] = ["x86_64", "aarch64", "armv7h"];

/// The default for a new package: what almost every PKGBUILD supports, and the
/// only one that needs no emulation.
pub const DEFAULT: &str = "x86_64";

/// A checkbox per architecture, reporting the whole selection on every change.
///
/// Reports the set rather than the toggle so callers hold one value; a caller
/// that had to apply add/remove events would be re-implementing this list.
#[component]
pub fn PlatformChecklist(
    selected: Vec<String>,
    onchange: EventHandler<Vec<String>>,
    #[props(default = String::from("checkbox-xs"))] size: String,
    #[props(default = false)] disabled: bool,
) -> Element {
    rsx! {
        div { class: "flex flex-col gap-1",
            for platform in ALL {
                label { key: "{platform}", class: "flex items-center gap-2 cursor-pointer",
                    input {
                        r#type: "checkbox",
                        class: "checkbox {size}",
                        // Names the box for assistive tech, and is what the
                        // interaction tests address it by.
                        "aria-label": "{platform}",
                        disabled,
                        checked: selected.iter().any(|p| p == platform),
                        onchange: {
                            let selected = selected.clone();
                            move |e: FormEvent| {
                                let mut next = selected.clone();
                                if e.checked() {
                                    if !next.iter().any(|p| p == platform) {
                                        next.push(platform.to_string());
                                    }
                                } else {
                                    next.retain(|p| p != platform);
                                }
                                onchange.call(next);
                            }
                        },
                    }
                    span { class: "font-mono text-xs", "{platform}" }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ALL, DEFAULT, PlatformChecklist};
    use crate::testing::Harness;
    use dioxus::prelude::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// The default has to be one of the offered architectures, or a new package
    /// starts out asking for something the picker cannot show.
    #[test]
    fn the_default_platform_is_one_of_the_offered_ones() {
        assert!(ALL.contains(&DEFAULT));
    }

    /// The reported set is captured rather than rendered, because it is what
    /// callers act on and it is not visible in the markup.
    fn checklist(initial: &[&str]) -> (Harness, Rc<RefCell<Vec<Vec<String>>>>) {
        let reported = Rc::new(RefCell::new(Vec::new()));
        let selected: Vec<String> = initial.iter().map(ToString::to_string).collect();

        #[component]
        fn Host(selected: Vec<String>, reported: Rc<RefCell<Vec<Vec<String>>>>) -> Element {
            let mut selected = use_signal(|| selected);
            rsx! {
                PlatformChecklist {
                    selected: selected(),
                    onchange: move |next: Vec<String>| {
                        reported.borrow_mut().push(next.clone());
                        selected.set(next);
                    },
                }
            }
        }

        let app = Harness::new_with_props(
            Host,
            HostProps {
                selected,
                reported: reported.clone(),
            },
        );
        (app, reported)
    }

    /// Ticking a box reports the whole set, not the box. Getting this wrong
    /// renders identically — the checkbox still moves — and the caller quietly
    /// receives one architecture instead of two.
    #[test]
    fn ticking_a_box_adds_it_to_the_reported_set() {
        let (mut app, reported) = checklist(&["x86_64"]);

        app.set_checked("aria-label", "aarch64", true);
        assert_eq!(
            reported.borrow().last().unwrap(),
            &["x86_64".to_string(), "aarch64".to_string()]
        );
    }

    #[test]
    fn unticking_a_box_removes_only_that_one() {
        let (mut app, reported) = checklist(&["x86_64", "aarch64"]);

        app.set_checked("aria-label", "x86_64", false);
        assert_eq!(reported.borrow().last().unwrap(), &["aarch64".to_string()]);
    }

    /// Ticking something already selected must not list it twice, which would
    /// send the same platform to the server two times over.
    #[test]
    fn ticking_an_already_selected_box_does_not_duplicate_it() {
        let (mut app, reported) = checklist(&["x86_64"]);

        app.set_checked("aria-label", "x86_64", true);
        assert_eq!(reported.borrow().last().unwrap(), &["x86_64".to_string()]);
    }
}
